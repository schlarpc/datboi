//! The D72/D73 maintenance phases, run by the D71 worker thread after
//! its refine drains — one background writer, so heavy maintenance IO
//! never fights analyzer IO for the spindle:
//!
//! 1. **Eager licensing** (every cycle, NO guard — additive): replay
//!    verified-only routes so literals are evictable before pressure.
//! 2. **Orphan mark sweep** (ambient tick only — full-table scans):
//!    surface unreferenced candidates for the review gate. Marking is
//!    reversible bookkeeping; it needs no guard either.
//! 3. **Watermark eviction** (under the D72 singleton guard): the one
//!    concurrently-unsafe critical section — two grounding
//!    computations must never jointly approve stranding a
//!    mutually-inverse recipe pair. `datboi evict` and the orphan
//!    apply path contend for the same guard.
//!
//! Crash safety is compositional: licensing commits per recipe,
//! marks are derivable, eviction's unlink→flip order is recovery's
//! reconciliation direction, and a lapsed guard just re-plans.

use datboi_exec::{Executor, policy};
use datboi_index::{Db, GuardHolder};
use datboi_store_fs::Store;
use tracing::{debug, info, warn};

use crate::auth::now_unix;
use crate::jobs::Registry;

/// Routes licensed per cycle — bounds a cycle's IO so fresh-ingest
/// wakes aren't starved behind a giant backlog; the remainder rides
/// the next wake.
const LICENSE_BATCH: usize = 32;

/// Guard TTL: must outlive one full plan+drop round (the grounding
/// fixpoint at corpus scale plus the unlinks), chosen with the same
/// honesty as the D71 lease — a crashed holder stalls eviction for at
/// most this long.
const GUARD_TTL_SECS: i64 = 15 * 60;

pub(crate) struct Maintainer {
    exec: Executor<'static>,
    holder: GuardHolder,
    /// The instance identity (D75): the auto-cadence rider signs
    /// snapshots with the same key `datboi snapshot` uses.
    identity: datboi_core::identity::Identity,
}

impl Maintainer {
    pub(crate) fn new(store: &'static Store, db_dir: &std::path::Path) -> anyhow::Result<Self> {
        let mut holder = [0u8; 16];
        getrandom::getrandom(&mut holder)?;
        Ok(Self {
            exec: Executor::new(store, datboi_exec::ExecConfig::default())?,
            holder: GuardHolder(holder),
            identity: datboi_catalog::statesnap::load_or_create_identity(db_dir)?,
        })
    }

    /// One maintenance cycle. Every step is fallible-but-contained:
    /// a phase failure logs and yields to the next cycle rather than
    /// killing the worker.
    pub(crate) fn cycle(&self, db: &mut Db, store: &Store, jobs: &Registry, ambient: bool) {
        self.license(db, jobs);
        if ambient {
            self.mark_orphans(db);
            // D91: the piece-swap rides ambient ticks only (its
            // candidate scan is full-corpus, like the orphan sweep).
            self.swap(db, jobs);
            // D91/D59: consolidate chunk-set loose floods into packs
            // (after the swap, which may have evicted their originals).
            self.pack_chunks(db, jobs);
        }
        self.watermark_evict(db, store, jobs);
        if ambient {
            self.snapshot_if_dirty(db, store);
        }
    }

    /// Phase 4 — the D75 auto-cadence rider (ambient ticks, AFTER the
    /// byte-moving phases so a cycle's own keep-marks/config writes
    /// ride the same tick's snapshot). Content-derived dirtiness: mint
    /// only when the authoritative triple (sources, tags, config)
    /// differs from the newest logged snapshot — operator intent never
    /// waits on an operator remembering `datboi snapshot`.
    fn snapshot_if_dirty(&self, db: &mut Db, store: &Store) {
        match datboi_catalog::statesnap::maybe_mint(store, db, &self.identity, now_unix()) {
            Ok(Some(report)) => info!(
                "maintenance: state snapshot {} (seq {}) — authoritative state moved",
                report.hash, report.sequence
            ),
            Ok(None) => {}
            Err(e) => warn!("maintenance: auto-snapshot: {e}"),
        }
    }

    /// Phase 1 — eager licensing (D72). Tray job only when something
    /// actually licensed; persistent failures go to stderr and retry
    /// next cycle (the pool re-derives from verify state).
    fn license(&self, db: &mut Db, jobs: &Registry) {
        let pending = match self.exec.license_pending(db) {
            Ok(n) => n,
            Err(e) => {
                warn!("maintenance: license pool: {e}");
                return;
            }
        };
        if pending == 0 {
            return;
        }
        let batch = pending.min(LICENSE_BATCH);
        let job = jobs.create_gc("license — rebuild routes", batch as u64, now_unix());
        match self.exec.license_covered(db, LICENSE_BATCH) {
            Ok(report) => {
                for (hash, error) in &report.failed {
                    debug!("maintenance job {job}: license {hash}: {error}");
                }
                let done = report.replayed as u64;
                jobs.refine_progress(job, done, done + report.failed.len() as u64);
                jobs.push_note(
                    job,
                    format!(
                        "{} route(s) licensed, {} failed, {} still pending",
                        report.replayed,
                        report.failed.len(),
                        pending.saturating_sub(batch)
                    ),
                );
                jobs.finish(job, now_unix());
                info!(
                    "maintenance job {job}: licensed {}/{batch} route(s)",
                    report.replayed
                );
            }
            Err(e) => {
                warn!("maintenance job {job}: FAILED — {e}");
                jobs.fail(job, &e.to_string(), now_unix());
            }
        }
    }

    /// Phase 2 — orphan mark sweep (D73). Bookkeeping only: no tray
    /// job, no guard; the counts go to stderr and the Storage surface
    /// reads the table.
    fn mark_orphans(&self, db: &mut Db) {
        let roots = match self.exec.orphan_extra_roots(db) {
            Ok(roots) => roots,
            Err(e) => {
                warn!("maintenance: orphan roots: {e}");
                return;
            }
        };
        match db.sweep_orphan_marks(&roots, now_unix()) {
            Ok((marked, cleared)) if marked + cleared > 0 => {
                info!("maintenance: orphan sweep — {marked} newly marked, {cleared} cleared");
            }
            Ok(_) => {}
            Err(e) => warn!("maintenance: orphan sweep: {e}"),
        }
    }

    /// Phase 2b — the D91 affine piece-swap, under the singleton guard
    /// (its evict step computes grounding counterfactuals; two
    /// planners interleaving those can strand an inverse pair). Quiet
    /// when nothing trips the sharing predicate — a tray job only when
    /// bytes actually moved.
    fn swap(&self, db: &mut Db, jobs: &Registry) {
        match policy::swap_enabled(db) {
            Ok(true) => {}
            Ok(false) => return,
            Err(e) => {
                warn!("maintenance: swap policy: {e}");
                return;
            }
        }
        if !claim_guard(db, &self.holder) {
            debug!("maintenance: swap deferred; gc guard busy");
            return;
        }
        let result = self.exec.swap_covered(db);
        db.release_gc_guard(&self.holder).unwrap_or_else(|e| {
            warn!("maintenance: guard release: {e}"); // TTL is the backstop
        });
        match result {
            Ok(report) if report.swapped == 0 && report.skipped.is_empty() => {}
            Ok(report) => {
                let job = jobs.create_gc("swap — pieces over container", 0, now_unix());
                for (hash, why) in &report.skipped {
                    debug!("maintenance job {job}: swap {hash}: {why}");
                }
                let done = report.swapped as u64;
                jobs.refine_progress(job, done, done + report.skipped.len() as u64);
                jobs.push_note(
                    job,
                    format!(
                        "{} container(s) swapped into {} pack(s): {} byte(s) packed, \
                         {} reclaimed; {} below the sharing threshold, {} skipped",
                        report.swapped,
                        report.packs,
                        report.bytes_packed,
                        report.bytes_reclaimed,
                        report.below_threshold,
                        report.skipped.len()
                    ),
                );
                jobs.finish(job, now_unix());
                info!(
                    "maintenance job {job}: swapped {} container(s), reclaimed {} byte(s)",
                    report.swapped, report.bytes_reclaimed
                );
            }
            Err(e) => warn!("maintenance: swap phase: {e}"),
        }
    }

    /// Phase 2c — pack-per-chunking (D91/D59), under the singleton
    /// guard (it drops loose piece files; keeping it serial with the
    /// evictor avoids a piece being packed and watermark-evicted at
    /// once). Quiet unless a chunk flood was actually consolidated.
    fn pack_chunks(&self, db: &mut Db, jobs: &Registry) {
        match policy::chunk_pack_enabled(db) {
            Ok(true) => {}
            Ok(false) => return,
            Err(e) => {
                warn!("maintenance: chunk-pack policy: {e}");
                return;
            }
        }
        if !claim_guard(db, &self.holder) {
            debug!("maintenance: chunk-pack deferred; gc guard busy");
            return;
        }
        let result = self.exec.pack_chunk_sets(db);
        db.release_gc_guard(&self.holder).unwrap_or_else(|e| {
            warn!("maintenance: guard release: {e}");
        });
        match result {
            Ok(report) if report.sets_packed == 0 && report.swept_loose == 0 => {}
            Ok(report) => {
                let job = jobs.create_gc("chunk-pack — pieces into packs", 0, now_unix());
                for (hash, why) in &report.skipped {
                    debug!("maintenance job {job}: chunk-pack {hash}: {why}");
                }
                jobs.refine_progress(job, report.sets_packed as u64, report.sets_packed as u64);
                jobs.push_note(
                    job,
                    format!(
                        "{} chunk set(s) packed: {} piece(s), {} byte(s) consolidated; \
                         {} stale loose copy(ies) swept, {} skipped",
                        report.sets_packed,
                        report.members,
                        report.bytes_packed,
                        report.swept_loose,
                        report.skipped.len()
                    ),
                );
                jobs.finish(job, now_unix());
                info!(
                    "maintenance job {job}: packed {} chunk set(s), {} piece(s)",
                    report.sets_packed, report.members
                );
            }
            Err(e) => warn!("maintenance: chunk-pack phase: {e}"),
        }
    }

    /// Phase 3 — watermark eviction (D72), under the singleton guard.
    fn watermark_evict(&self, db: &mut Db, store: &Store, jobs: &Registry) {
        let usage = match store.fs_usage() {
            Ok(Some(pair)) => pair,
            Ok(None) => return, // unanswerable platform: stay additive
            Err(e) => {
                warn!("maintenance: fs usage: {e}");
                return;
            }
        };
        let (total, avail) = usage;
        let used = total.saturating_sub(avail);
        let high = match policy::high_water(db).map(|w| w.threshold_bytes(total)) {
            Ok(Some(high)) => high,
            Ok(None) => return, // disarmed
            Err(e) => {
                warn!("maintenance: watermark config: {e}");
                return;
            }
        };
        if used < high {
            return;
        }
        let low = policy::low_water(db)
            .ok()
            .and_then(|w| w.threshold_bytes(total))
            .unwrap_or(high);
        let reclaim = used.saturating_sub(low.min(high));

        if !claim_guard(db, &self.holder) {
            warn!("maintenance: watermark crossed but gc guard is busy; retrying next cycle");
            return;
        }
        let job = jobs.create_gc("evict — watermark", 0, now_unix());
        info!(
            "maintenance job {job}: watermark crossed (used {used} of {total}); reclaiming ~{reclaim} byte(s)"
        );
        let result = self.exec.evict_reclaim(db, reclaim);
        db.release_gc_guard(&self.holder).unwrap_or_else(|e| {
            warn!("maintenance: guard release: {e}"); // TTL is the backstop
        });
        match result {
            Ok(report) => {
                let evicted = report.evicted as u64;
                jobs.refine_progress(job, evicted, evicted);
                jobs.push_note(
                    job,
                    format!(
                        "{} blob(s) evicted, {} byte(s) reclaimed, {} blocked",
                        report.evicted,
                        report.bytes_reclaimed,
                        report.blocked.len()
                    ),
                );
                jobs.finish(job, now_unix());
                info!(
                    "maintenance job {job}: evicted {} blob(s), {} byte(s)",
                    report.evicted, report.bytes_reclaimed
                );
            }
            Err(e) => {
                warn!("maintenance job {job}: FAILED — {e}");
                jobs.fail(job, &e.to_string(), now_unix());
            }
        }
    }
}

/// Claim the D72 guard for one critical section. Exposed shape shared
/// with the orphan-apply path (api.rs).
pub(crate) fn claim_guard(db: &Db, holder: &GuardHolder) -> bool {
    db.claim_gc_guard(holder, now_unix(), GUARD_TTL_SECS)
        .unwrap_or(false)
}
