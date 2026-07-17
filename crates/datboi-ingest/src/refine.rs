//! The refinement fixpoint skeleton (D45/D47/D48): background sweeps that
//! advance analysis over the corpus. Ingest is custody; everything
//! expensive happens here, later, resumably.
//!
//! An analyzer is a pure function of `bytes × analyzer identity`. Its
//! identity hash pins exactly what ran: a wasm analyzer's component hash,
//! or [`analyzer_tag`] over a versioned name for native ones. Shipping a
//! new analyzer version = new identity = every blob becomes unanalyzed
//! again for it — "new analyzer ships" and "keys arrive late" are the
//! same event, and the fixpoint advances (D45).
//!
//! The sweep loop is crash-safe by at-least-once: provenance row +
//! queue-row removal commit together AFTER the work; a crash mid-item
//! re-runs a pure function.

use std::time::{SystemTime, UNIX_EPOCH};

use datboi_core::hash::Blake3;
use datboi_exec::Executor;
use datboi_index::{AnalysisOutcome, Db, Residency, SweepItem};
use datboi_store_fs::{Namespace as StoreNs, Store};

/// Identity hash for a native (non-wasm) analyzer: a domain-separated
/// hash over a versioned name, e.g. `"noop/1"`. Bump the version to make
/// the fixpoint re-run it over everything.
#[must_use]
pub fn analyzer_tag(versioned_name: &str) -> Blake3 {
    Blake3::compute(format!("datboi-analyzer:{versioned_name}").as_bytes())
}

/// What one analysis produced.
pub struct AnalysisResult {
    pub outcome: AnalysisOutcome,
    /// Analyzer-owned annotation (why negative / what was minted).
    pub detail: Option<String>,
}

/// Liveness-as-progress (D71): analyzers pulse this as bytes move
/// through their streaming loops — typically by wrapping the long
/// reader in [`TickReader`]. The sweep driver wires it to lease
/// renewal, so a lease stays alive exactly as long as work advances:
/// a wedged analyzer (dead NFS mount, livelocked guest) stops pulsing
/// and its lease lapses for another worker to claim. Byte counts ride
/// along for a future intra-item progress surface (the jobs-tray open
/// question).
pub trait Pulse {
    fn tick(&mut self, bytes: u64);
}

/// For direct analyzer invocations that have nothing to keep alive.
pub struct NoPulse;
impl Pulse for NoPulse {
    fn tick(&mut self, _bytes: u64) {}
}

/// `Read` adapter that pulses per successful read — the one-line way
/// for an analyzer to make its longest loop heartbeat-bearing.
pub struct TickReader<'p, R> {
    inner: R,
    pulse: &'p mut dyn Pulse,
}

impl<'p, R: std::io::Read> TickReader<'p, R> {
    pub fn new(inner: R, pulse: &'p mut dyn Pulse) -> Self {
        Self { inner, pulse }
    }
}

impl<R: std::io::Read> std::io::Read for TickReader<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        if n > 0 {
            self.pulse.tick(n as u64);
        }
        Ok(n)
    }
}

/// D92: the analyzer's byte source — the LOGICAL CAS. Resident
/// literals open straight from the store; grounded absents spill
/// through the executor's verified sequential path into an anonymous
/// temp file (bounded, dropped with the handle — never a residency
/// flip; that would be the planner's decision, D91's territory). One
/// concrete type on purpose: every sweep reads through the same
/// semantics, so "which byte source am I under" divergence is
/// unrepresentable.
pub struct Logical<'a, 's> {
    store: &'a Store,
    exec: &'a Executor<'s>,
}

impl<'a, 's> Logical<'a, 's> {
    pub fn new(store: &'a Store, exec: &'a Executor<'s>) -> Self {
        Self { store, exec }
    }

    /// Open one sweep item's bytes as a plain seekable file.
    ///
    /// The claim gate only hands out items whose bytes are obtainable
    /// (resident, or admitted by [`Db::refresh_absent_eligibility`]),
    /// so every failure here is environmental: the item stays queued.
    /// One self-heal rides the resident path (D81): the index claiming
    /// Resident while the store has no bytes means the INDEX is wrong
    /// (wiped or swapped store dir) — demote to Absent so the row
    /// drops out of future claims until something rematerializes it.
    ///
    /// A spill pulses per copied chunk — the bytes moving ARE the
    /// lease heartbeat (D71) — and re-hashes the produced stream:
    /// analyzers must never conclude over bytes that aren't the
    /// item's (a mismatch wastes this sweep, never mints a claim).
    ///
    /// # Errors
    /// Environmental (store I/O, no groundable route right now, spill
    /// I/O, hash mismatch): the analysis is retryable, not settled.
    pub fn open(
        &self,
        item: &SweepItem,
        db: &Db,
        pulse: &mut dyn Pulse,
    ) -> Result<datboi_store_fs::Blob, String> {
        use std::io::{Read, Seek, Write};

        if let Some(file) = self
            .store
            .get(StoreNs::Data, &item.hash)
            .map_err(|e| e.to_string())?
        {
            return Ok(file);
        }
        match db.blob_by_hash(&item.hash).map_err(|e| e.to_string())? {
            Some(row) if row.residency == Residency::Resident => {
                return Err(match db.set_residency(row.blob_id, Residency::Absent) {
                    Ok(()) => "blob not resident — index said resident, store had no bytes; \
                               demoted to absent (D81)"
                        .into(),
                    Err(e) => format!("blob not resident (and the index demote failed: {e})"),
                });
            }
            Some(_) => {}
            None => return Err("sweep item has no blob row".into()),
        }

        // Grounded absent: verified sequential stream, spilled to the
        // executor's spill location (never the OS tmp by accident —
        // dual-layer images do not fit a tmpfs).
        let mut stream = self
            .exec
            .open_stream(db, &item.hash)
            .map_err(|e| format!("logical open of absent blob: {e}"))?;
        let mut tmp = self.exec.spill_tempfile().map_err(|e| e.to_string())?;
        let mut hasher = blake3::Hasher::new();
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let n = stream.read(&mut buf).map_err(|e| e.to_string())?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            tmp.write_all(&buf[..n]).map_err(|e| e.to_string())?;
            pulse.tick(n as u64);
        }
        if Blake3(*hasher.finalize().as_bytes()) != item.hash {
            return Err("spilled route produced bytes that are not the item's".into());
        }
        tmp.rewind().map_err(|e| e.to_string())?;
        Ok(datboi_store_fs::Blob::loose(tmp))
    }
}

/// One analyzer. Implementations mint recipes/claims through their own
/// side effects (they get store + db access) and report provenance via
/// the returned result. What they claim must be dat-blind (D47).
pub trait Analyzer {
    /// Versioned, human-readable name (also the CLI selector).
    fn name(&self) -> &'static str;

    /// Stable operator-facing family name — the D60 config key. Unlike
    /// [`Analyzer::name`]/[`Analyzer::id`], it survives version bumps:
    /// an operator's enable/disable choice carries across analyzer
    /// revisions (identity re-runs the fixpoint; policy stays).
    fn family(&self) -> &'static str;

    /// Identity hash — what provenance rows pin (D48).
    fn id(&self) -> Blake3;

    /// Analyze one blob's bytes — read via `bytes` (the logical CAS,
    /// D92), minted results written via `store`. Long byte-crunching
    /// loops should `pulse` as they progress (wrap the reader in
    /// [`TickReader`]) so the item's lease outlives any fixed TTL
    /// exactly while work advances.
    ///
    /// # Errors
    /// A per-blob error string: recorded nowhere, item stays queued (the
    /// environment failed, not the analysis — a negative CONCLUSION must
    /// be returned as `Ok(Negative)`).
    fn analyze(
        &mut self,
        item: &SweepItem,
        bytes: &Logical<'_, '_>,
        store: &Store,
        db: &mut Db,
        pulse: &mut dyn Pulse,
    ) -> Result<AnalysisResult, String>;
}

/// A sweep that concludes "nothing found" about everything it touches —
/// the fixpoint plumbing proof (D50 exit criterion: a no-op analyzer
/// sweep records provenance and survives the recovery drill).
pub struct NoopAnalyzer;

impl Analyzer for NoopAnalyzer {
    fn name(&self) -> &'static str {
        "noop/1"
    }

    fn family(&self) -> &'static str {
        "noop"
    }

    fn id(&self) -> Blake3 {
        analyzer_tag(self.name())
    }

    fn analyze(
        &mut self,
        _item: &SweepItem,
        _bytes: &Logical<'_, '_>,
        _store: &Store,
        _db: &mut Db,
        _pulse: &mut dyn Pulse,
    ) -> Result<AnalysisResult, String> {
        Ok(AnalysisResult {
            outcome: AnalysisOutcome::Negative,
            detail: None,
        })
    }
}

#[derive(Debug, Default)]
pub struct SweepReport {
    pub enqueued: usize,
    pub analyzed: usize,
    pub positive: usize,
    pub negative: usize,
    /// (blob hash, error) — items left queued for a later sweep.
    pub errors: Vec<(Blake3, String)>,
    /// The analyzer family is disabled (D60): nothing ran.
    pub disabled: bool,
}

// ---- D60 ingest-policy config: the minimal shape ----
//
// Per-analyzer enable/disable + analyzer-owned opaque params in the
// state.db config KV (`analyzer:<family>:enabled` / `:params`), which
// rides the state snapshot like every other config row. Sweep ordering
// stays a single global dat-aware policy — no per-analyzer knobs.

fn enabled_key(family: &str) -> String {
    format!("analyzer:{family}:enabled")
}

fn params_key(family: &str) -> String {
    format!("analyzer:{family}:params")
}

/// The shipped analyzer families that carry per-family config
/// (enable/disable + opaque params). The ONE source both the CLI's
/// `analyzer` subcommand and the daemon's `/v1/analyzers` surface agree
/// on (D96) — a family added here appears on both without a second edit.
pub const FAMILIES: &[&str] = &["noop", "chunk", "preflate", "ecm", "nds"];

/// Is the family enabled? Absent means yes (opt-out policy).
///
/// # Errors
/// Index I/O.
pub fn analyzer_enabled(db: &Db, family: &str) -> Result<bool, datboi_index::IndexError> {
    Ok(db
        .config_get(&enabled_key(family))?
        .is_none_or(|v| v != b"0"))
}

/// # Errors
/// Index I/O.
pub fn set_analyzer_enabled(
    db: &Db,
    family: &str,
    enabled: bool,
) -> Result<(), datboi_index::IndexError> {
    db.config_set(&enabled_key(family), if enabled { b"1" } else { b"0" })
}

/// Analyzer-owned opaque params (the analyzer defines the encoding;
/// nothing else interprets them). Empty and absent are both `None`.
///
/// # Errors
/// Index I/O.
pub fn analyzer_params(db: &Db, family: &str) -> Result<Option<Vec<u8>>, datboi_index::IndexError> {
    Ok(db
        .config_get(&params_key(family))?
        .filter(|v| !v.is_empty()))
}

/// Set (or clear, with `None`) a family's opaque params.
///
/// # Errors
/// Index I/O.
pub fn set_analyzer_params(
    db: &Db,
    family: &str,
    params: Option<&[u8]>,
) -> Result<(), datboi_index::IndexError> {
    db.config_set(&params_key(family), params.unwrap_or(b""))
}

/// Lease TTL for a claimed sweep item (D71). Short on purpose: leases
/// renew while the analyzer makes progress (the [`Pulse`] heartbeat),
/// so the TTL only has to outlive renewal jitter — not the analysis.
/// A wedged worker stops pulsing and its item frees in at most this
/// long; a crashed CLI's leases lapse on the same clock (the daemon
/// additionally clears all leases at startup). Doubles as the error
/// backoff: a failed item keeps its lease, so it retries after expiry
/// instead of hot-looping — one ambient-rescan beat later.
pub const SWEEP_LEASE_SECS: i64 = 15 * 60;

/// Progress-gated renewal cadence: the heartbeat re-stamps the lease
/// at most this often, and only when [`Pulse::tick`] fires — bytes
/// moving is the proof of liveness, a timer would keep renewing for a
/// worker wedged on a dead mount.
const LEASE_RENEW_SECS: u64 = 5 * 60;

/// The sweep driver's pulse: renews one item's lease through a
/// [`SweepLeaseKeeper`] (its own connection — the main one is mutably
/// borrowed by the analyzer while this fires). A renewal failure is
/// deliberately swallowed: the lease lapsing costs at worst a
/// duplicated pure function, while aborting a half-done split over a
/// scheduling row would cost real work.
struct LeaseHeartbeat<'a> {
    keeper: &'a datboi_index::SweepLeaseKeeper,
    analyzer: Blake3,
    blob_id: i64,
    last: std::time::Instant,
}

impl<'a> LeaseHeartbeat<'a> {
    fn new(keeper: &'a datboi_index::SweepLeaseKeeper, analyzer: Blake3, blob_id: i64) -> Self {
        Self {
            keeper,
            analyzer,
            blob_id,
            last: std::time::Instant::now(),
        }
    }
}

impl Pulse for LeaseHeartbeat<'_> {
    fn tick(&mut self, _bytes: u64) {
        if self.last.elapsed().as_secs() < LEASE_RENEW_SECS {
            return;
        }
        self.last = std::time::Instant::now();
        let _ = self
            .keeper
            .renew(self.blob_id, &self.analyzer, now_unix(), SWEEP_LEASE_SECS);
    }
}

/// Per-item hooks for a sweep driver that reports progress (the
/// daemon's refine worker). Callbacks fire on the sweeping thread,
/// between db transactions — keep them cheap.
pub trait SweepObserver {
    fn item_started(&mut self, _item: &SweepItem) {}
    fn item_finished(&mut self, _item: &SweepItem, _outcome: Result<AnalysisOutcome, &str>) {}
    /// Checked before each item: `true` ends the round early. Nothing
    /// needs releasing — items are claimed one at a time, so an early
    /// stop simply never claims the rest.
    fn should_stop(&mut self) -> bool {
        false
    }
}

/// The CLI's observer: nothing to report mid-round, never stops early.
pub struct NoObserver;
impl SweepObserver for NoObserver {}

/// Enqueue one analyzer's unanalyzed candidates (dat-blind, D47).
/// Returns rows enqueued. The per-FAMILY half of a queue refresh; the
/// analyzer-independent half is [`refresh_admission`].
///
/// # Errors
/// Index I/O.
pub fn enqueue_candidates(
    db: &Db,
    analyzer: &dyn Analyzer,
) -> Result<usize, datboi_index::IndexError> {
    db.enqueue_unanalyzed(&analyzer.id(), now_unix())
}

/// The analyzer-INDEPENDENT half of a queue refresh: dat-aware priority
/// ordering + the D92 absent-admission table (which recomputes the
/// grounding fixpoint). Both operate over blobs/queues globally, not per
/// analyzer, so a multi-family driver runs this ONCE per wake after all
/// families enqueue — paying the corpus-scale fixpoint once, not once
/// per family (D92 owed: grounded-set-aware enqueue). Order: after
/// enqueue, so the priority bump sees every family's fresh rows.
///
/// # Errors
/// Index I/O.
pub fn refresh_admission(db: &Db) -> Result<(), datboi_index::IndexError> {
    db.bump_dat_matched_priorities()?;
    db.refresh_absent_eligibility()?;
    Ok(())
}

/// Refresh one analyzer's queue whole: enqueue candidates then run the
/// admission pass. The single-family entry point ([`run_sweep`], CLI
/// sweeps); a multi-family driver splits the two halves so the fixpoint
/// in [`refresh_admission`] runs once per wake, not once per family.
///
/// # Errors
/// Index I/O.
pub fn refresh_queue(
    db: &mut Db,
    analyzer: &dyn Analyzer,
) -> Result<usize, datboi_index::IndexError> {
    let enqueued = enqueue_candidates(db, analyzer)?;
    refresh_admission(db)?;
    Ok(enqueued)
}

/// Process up to `limit` already-queued items, one claim at a time:
/// the lease clock starts when the ITEM's work starts, never when a
/// batch was planned (claim granularity = execution granularity — the
/// fix for upfront batch leases going stale before their turn). Each
/// item runs under a [`SWEEP_LEASE_SECS`] lease that the analyzer's
/// progress renews (the [`Pulse`] heartbeat); provenance + queue
/// removal commit per item (the at-least-once discipline). A per-blob
/// analyzer ERROR keeps its lease — the item retries after expiry, so
/// a draining loop can't hot-spin on a poisoned item; a deliberate
/// early stop simply claims nothing further.
///
/// # Errors
/// Database errors abort the round; per-blob analyzer errors are
/// reported in the result and leave their items queued.
pub fn process_round(
    db: &mut Db,
    store: &Store,
    bytes: &Logical<'_, '_>,
    analyzer: &mut dyn Analyzer,
    limit: usize,
    observer: &mut dyn SweepObserver,
) -> Result<SweepReport, datboi_index::IndexError> {
    // D60: policy gate at the one entry point every sweep caller uses.
    if !analyzer_enabled(db, analyzer.family())? {
        return Ok(SweepReport {
            disabled: true,
            ..SweepReport::default()
        });
    }
    let id = analyzer.id();
    let mut report = SweepReport::default();
    let keeper = db.lease_keeper()?;
    for _ in 0..limit {
        if observer.should_stop() {
            break;
        }
        let Some(item) = db
            .claim_sweep_items(&id, 1, now_unix(), SWEEP_LEASE_SECS)?
            .pop()
        else {
            break;
        };
        observer.item_started(&item);
        let mut pulse = LeaseHeartbeat::new(&keeper, id, item.blob_id);
        match analyzer.analyze(&item, bytes, store, db, &mut pulse) {
            Ok(result) => {
                db.complete_sweep_item(
                    item.blob_id,
                    &id,
                    result.outcome,
                    result.detail.as_deref(),
                    now_unix(),
                )?;
                report.analyzed += 1;
                match result.outcome {
                    AnalysisOutcome::Positive => report.positive += 1,
                    AnalysisOutcome::Negative => report.negative += 1,
                }
                observer.item_finished(&item, Ok(result.outcome));
            }
            Err(e) => {
                observer.item_finished(&item, Err(&e));
                report.errors.push((item.hash, e));
            }
        }
    }
    Ok(report)
}

/// Run one sweep round: [`refresh_queue`] + [`process_round`] — the
/// CLI's single-shot shape.
///
/// # Errors
/// Database errors abort the sweep; per-blob analyzer errors are
/// reported and leave their items queued (leased until expiry).
pub fn run_sweep(
    db: &mut Db,
    store: &Store,
    bytes: &Logical<'_, '_>,
    analyzer: &mut dyn Analyzer,
    limit: usize,
) -> Result<SweepReport, datboi_index::IndexError> {
    if !analyzer_enabled(db, analyzer.family())? {
        return Ok(SweepReport {
            disabled: true,
            ..SweepReport::default()
        });
    }
    let enqueued = refresh_queue(db, analyzer)?;
    let mut report = process_round(db, store, bytes, analyzer, limit, &mut NoObserver)?;
    report.enqueued = enqueued;
    Ok(report)
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

#[cfg(test)]
mod pulse_tests {
    use super::*;
    use std::io::Read as _;

    struct Counter {
        ticks: usize,
        bytes: u64,
    }
    impl Pulse for Counter {
        fn tick(&mut self, bytes: u64) {
            self.ticks += 1;
            self.bytes += bytes;
        }
    }

    /// The heartbeat-bearing adapter pulses exactly the bytes that
    /// flow, and never on EOF — the mechanism every analyzer's long
    /// loop rides.
    #[test]
    fn tick_reader_pulses_bytes_read() {
        let mut pulse = Counter { ticks: 0, bytes: 0 };
        let data = vec![7u8; 10_000];
        let mut out = Vec::new();
        TickReader::new(std::io::Cursor::new(&data), &mut pulse)
            .read_to_end(&mut out)
            .expect("read");
        assert_eq!(out.len(), data.len());
        assert_eq!(pulse.bytes, data.len() as u64);
        assert!(pulse.ticks >= 1);
        // EOF reads don't tick.
        let before = pulse.ticks;
        let mut empty = TickReader::new(std::io::empty(), &mut pulse);
        empty.read_to_end(&mut Vec::new()).expect("read");
        assert_eq!(pulse.ticks, before);
    }
}
