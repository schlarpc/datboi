//! The in-daemon refinement worker (D71): analysis "just happens" in
//! serve mode — immediately for freshly-ingested content, ambiently
//! for the rest of the corpus — instead of waiting for a CLI sweep.
//!
//! Shape: ONE dedicated worker thread for the daemon's lifetime,
//! niced to the lowest CPU priority (it is optimization, never the
//! product; on Linux `nice` also steers bfq's io priority — an
//! explicit `ioprio_set` has no safe wrapper and waits for a measured
//! need). The worker owns a PRIVATE `Db` connection so a minutes-long
//! preflate split never holds the request path's `Mutex<Db>`; SQLite
//! WAL + busy_timeout arbitrate the two connections, and every index
//! write in the sweep path is a short transaction between long
//! byte-crunching stretches.
//!
//! Scheduling is three tiers of one queue (datboi-index consts):
//! fresh-ingest > dat-matched > ambient backlog — D47 intact (tiers
//! order work; membership stays dat-blind). Ingest completion feeds
//! the fresh tier through [`Refiner::notify_fresh`] and wakes the
//! worker; an ambient rescan re-enqueues the corpus on a slow clock
//! (new blobs are caught by the wake path — the rescan exists for
//! rematerialized blobs and expired error-leases).
//!
//! Deconfliction is the D71 lease column: the worker claims items
//! before analyzing, so a concurrent CLI sweep skips them (and vice
//! versa) — dedup of expensive work, never a correctness gate, since
//! analyzers are pure functions and completion is at-least-once.
//! Startup clears all leases: one daemon per db-dir, so any lease on
//! disk belonged to a dead process. Eviction needs no coordination
//! with this worker by construction: analysis is additive (mints
//! recipes, never destroys bytes), evict only drops replay-licensed
//! literals, and an analyzer that loses a race to eviction sees
//! "blob not resident" — a retryable error, not corruption.
//!
//! Family order is deliberate: preflate before chunk, so containers
//! gain rebuild routes before the chunker asks "route-less?" (D59) —
//! chunking a zip that preflate is about to cover would be pure waste.

use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use datboi_index::Db;
use datboi_ingest::analyzers::{ChunkAnalyzer, EcmAnalyzer, NdsAnalyzer, PreflateZipAnalyzer};
use datboi_ingest::refine::{
    Analyzer, SweepObserver, analyzer_enabled, process_round, refresh_queue,
};
use datboi_store_fs::Store;
use tracing::{debug, error, info, warn};

use crate::auth::now_unix;
use crate::jobs::Registry;

/// Ambient rescan cadence. The full-corpus candidate scan
/// (`enqueue_unanalyzed`) is one INSERT‥SELECT per family — cheap, but
/// not per-minute cheap at 10M blobs, and the wake path already covers
/// the latency-sensitive case (fresh ingest).
const AMBIENT_RESCAN: Duration = Duration::from_secs(30 * 60);

/// After a database-level error the worker backs off instead of
/// spinning (the environment failed, not one item).
const ERROR_BACKOFF: Duration = Duration::from_secs(60);

/// Items claimed per round. One at a time on purpose: the claim
/// transaction is trivia next to any real analysis, and re-claiming
/// per item means a fresh-tier arrival preempts the ambient backlog at
/// the next item boundary, not after a long batch.
const ROUND: usize = 1;

/// Handle held by the daemon: wake the worker, feed the fresh tier.
/// Dropping it does not stop the worker — the thread lives as long as
/// the process, same posture as ingest jobs.
pub(crate) struct Refiner {
    shared: Arc<Shared>,
}

struct Shared {
    pending: Mutex<Vec<i64>>,
    wake: Condvar,
}

impl Refiner {
    /// Spawn the worker thread. `db_dir` is opened as a second, private
    /// connection pair; `jobs` receives one refine job per family drain.
    pub(crate) fn spawn(
        db_dir: std::path::PathBuf,
        store: &'static Store,
        jobs: Arc<Registry>,
    ) -> Self {
        let shared = Arc::new(Shared {
            pending: Mutex::new(Vec::new()),
            wake: Condvar::new(),
        });
        let worker_shared = Arc::clone(&shared);
        std::thread::spawn(move || worker(&db_dir, store, &jobs, &worker_shared));
        Self { shared }
    }

    /// Feed just-ingested blob ids into the fresh tier and wake the
    /// worker. Ids come from the SAME database the worker reads
    /// (`IngestReport::fresh_blobs`).
    pub(crate) fn notify_fresh(&self, blob_ids: Vec<i64>) {
        if blob_ids.is_empty() {
            return;
        }
        lock(&self.shared.pending).extend(blob_ids);
        self.shared.wake.notify_one();
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The auto-swept analyzer families, in dependency-aware order (module
/// docs). `noop` is CLI-only plumbing; a new analyzer joins by adding
/// a constructor here.
fn families() -> Vec<Box<dyn Analyzer>> {
    vec![
        Box::new(PreflateZipAnalyzer::new()),
        Box::new(EcmAnalyzer::new()),
        // nds-split before chunk: it mints the covering rebuild route
        // that lets the D59 gate skip chunking the same ROM.
        Box::new(NdsAnalyzer),
        Box::new(ChunkAnalyzer),
    ]
}

fn worker(db_dir: &std::path::Path, store: &'static Store, jobs: &Registry, shared: &Shared) {
    // Lowest CPU priority for THIS thread (Linux niceness is per-task,
    // so the request path is untouched). Best-effort everywhere else.
    if let Err(e) = rustix::process::nice(19) {
        warn!("refine worker: nice(19) failed ({e}); running at normal priority");
    }
    let mut db = match Db::open(db_dir) {
        Ok(db) => db,
        Err(e) => {
            error!("refine worker: cannot open databases: {e} — ambient refinement is OFF");
            return;
        }
    };
    // Startup amnesty: leases in this db-dir belonged to a dead
    // process (one daemon per db-dir); waiting them out would stall
    // the fresh path for nothing.
    if let Err(e) = db.clear_sweep_leases() {
        warn!("refine worker: clearing stale leases: {e}");
    }
    // Planner-stats seed (SQLite's long-lived-connection guidance):
    // ANALYZE whatever has never been analyzed, bounded. The ambient
    // tick keeps stats current from here on.
    if let Err(e) = db.optimize_at_open() {
        warn!("refine worker: pragma optimize at open: {e}");
    }
    let mut analyzers = families();
    // The D72/D73 maintenance phases ride this same thread (one
    // background writer). Losing them degrades to refine-only, loudly.
    let maintainer = match crate::maintain::Maintainer::new(store, db_dir) {
        Ok(m) => Some(m),
        Err(e) => {
            warn!("refine worker: maintenance disabled (executor: {e})");
            None
        }
    };
    // Due immediately: the startup pass drains whatever the corpus
    // accumulated while the daemon was down.
    let mut next_ambient = Instant::now();

    loop {
        let fresh = std::mem::take(&mut *lock(&shared.pending));
        let ambient_due = Instant::now() >= next_ambient;
        if fresh.is_empty() && !ambient_due {
            // Nothing to do: sleep until a wake or the ambient clock.
            let timeout = next_ambient.saturating_duration_since(Instant::now());
            let guard = lock(&shared.pending);
            drop(shared.wake.wait_timeout(guard, timeout));
            continue;
        }
        if let Err(e) = enqueue(&mut db, &mut analyzers, &fresh, ambient_due) {
            warn!("refine worker: enqueue: {e}");
            std::thread::sleep(ERROR_BACKOFF);
            continue;
        }
        if ambient_due {
            next_ambient = Instant::now() + AMBIENT_RESCAN;
        }
        for analyzer in &mut analyzers {
            drain_family(&mut db, store, jobs, shared, analyzer.as_mut());
        }
        // Maintenance AFTER refinement: licensing wants the routes the
        // drains just minted (the "drop a zip → evictable" motion).
        if let Some(maintainer) = &maintainer {
            maintainer.cycle(&mut db, store, jobs, ambient_due);
        }
        // Last, so a tick's own churn (drains, licensing, eviction) is
        // what the stats refresh sees. Near-free when content hasn't
        // drifted, hence every ambient tick rather than its own clock.
        if ambient_due && let Err(e) = db.optimize() {
            warn!("refine worker: pragma optimize: {e}");
        }
    }
}

/// Queue maintenance for one wake: fresh ids into the fresh tier, and
/// (on the ambient clock) the full dat-blind candidate scan.
fn enqueue(
    db: &mut Db,
    analyzers: &mut [Box<dyn Analyzer>],
    fresh: &[i64],
    ambient_due: bool,
) -> Result<(), datboi_index::IndexError> {
    for analyzer in analyzers {
        if !analyzer_enabled(db, analyzer.family())? {
            continue;
        }
        if !fresh.is_empty() {
            db.enqueue_fresh(&analyzer.id(), fresh, now_unix())?;
        }
        if ambient_due {
            refresh_queue(db, analyzer.as_ref())?;
        }
    }
    Ok(())
}

/// Names the blob under the knife in the tray ("current" line).
struct TrayObserver<'a> {
    jobs: &'a Registry,
    job: i64,
}

impl SweepObserver for TrayObserver<'_> {
    fn item_started(&mut self, item: &datboi_index::SweepItem) {
        self.jobs.set_current(self.job, &item.hash.to_hex());
    }
}

/// Drain one family's queue to empty (or to all-errors — failed items
/// keep their lease as backoff, so the loop can't hot-spin; the next
/// ambient pass retries them after expiry). One tray job per drain;
/// per-item analyzer errors ride its report like ingest refusals do.
fn drain_family(
    db: &mut Db,
    store: &'static Store,
    jobs: &Registry,
    shared: &Shared,
    analyzer: &mut dyn Analyzer,
) {
    let id = analyzer.id();
    let queued = match (
        analyzer_enabled(db, analyzer.family()),
        db.sweep_queue_len(&id),
    ) {
        (Ok(false), _) | (_, Ok(0)) => return,
        (Ok(true), Ok(n)) => n,
        (Err(e), _) | (_, Err(e)) => {
            warn!("refine worker: {}: {e}", analyzer.family());
            return;
        }
    };
    let job = jobs.create_refine(analyzer.family(), queued, now_unix());
    info!(
        "refine job {job}: {} — {queued} item(s) queued",
        analyzer.name()
    );
    let (mut done, mut positive, mut negative, mut failed) = (0u64, 0u64, 0u64, 0u64);
    loop {
        // Mid-drain ingests jump the line: their ids enter the fresh
        // tier now, and the per-item claim picks them up next.
        let fresh = std::mem::take(&mut *lock(&shared.pending));
        if !fresh.is_empty()
            && let Err(e) = db.enqueue_fresh(&id, &fresh, now_unix())
        {
            // Re-stage for the outer loop rather than losing them.
            lock(&shared.pending).extend(fresh);
            warn!("refine worker: fresh enqueue: {e}");
        }
        let mut observer = TrayObserver { jobs, job };
        let report = match process_round(db, store, analyzer, ROUND, &mut observer) {
            Ok(report) => report,
            Err(e) => {
                warn!("refine job {job}: FAILED — {e}");
                jobs.fail(job, &e.to_string(), now_unix());
                return;
            }
        };
        if report.disabled {
            break; // operator flipped the family off mid-drain
        }
        done += report.analyzed as u64;
        positive += report.positive as u64;
        negative += report.negative as u64;
        for (hash, error) in &report.errors {
            failed += 1;
            debug!("refine job {job}: {hash}: {error}");
            jobs.refine_error(job, &hash.to_hex(), error);
        }
        let remaining = db.sweep_queue_len(&id).unwrap_or(0);
        jobs.refine_progress(job, done, done + remaining);
        if report.analyzed == 0 && report.errors.is_empty() {
            break; // nothing claimable: drained (or all leased out)
        }
    }
    jobs.push_note(
        job,
        format!("{positive} rebuildable, {negative} concluded negative, {failed} error(s)"),
    );
    jobs.refine_progress(job, done, done);
    jobs.finish(job, now_unix());
    info!(
        "refine job {job}: done — {done} analyzed ({positive} positive, {negative} negative, {failed} error(s))"
    );
}
