//! The in-daemon refinement worker (D71): analysis "just happens" in
//! serve mode — immediately for freshly-ingested content, ambiently
//! for the rest of the corpus — instead of waiting for a CLI sweep.
//!
//! Shape (D93): a PRIME worker plus a fleet of DRONES, all niced to
//! the lowest CPU priority (optimization, never the product; on Linux
//! `nice` also steers bfq's io priority — an explicit `ioprio_set`
//! has no safe wrapper and waits for a measured need). The prime owns
//! everything coordination-shaped — wake handling, queue refresh (the
//! per-wake grounding fixpoint), the tray job per family drain, lease
//! amnesty at startup, and ALL maintenance phases — while drones do
//! nothing but claim-analyze-complete loops. The D71 lease column is
//! the work distribution: claims are atomic, at-least-once absorbs
//! every race, so worker count is pure scheduling. Every worker owns
//! a PRIVATE `Db` connection (a minutes-long preflate split never
//! holds anyone else's lock; WAL + busy_timeout arbitrate) and shares
//! ONE `Executor` (it is `Sync` — pinned by test — so compiled
//! components cache once). Worker count is the D93 formula
//! max(⌈n/2⌉, n−2), molten as `refine:workers` (read at start).
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
    Analyzer, Logical, SweepObserver, analyzer_enabled, process_round, refresh_queue,
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

/// Drone batch per family pass — modest, so a drone re-visits earlier
/// families (the dependency order) instead of camping on one queue.
const DRONE_ROUND: usize = 8;

/// Drones also wake on this clock with no signal: CLI-side enqueues
/// and expired error-leases arrive outside the prime's wake path.
const DRONE_POLL: Duration = Duration::from_secs(120);

/// Handle held by the daemon: wake the worker, feed the fresh tier.
/// Dropping it does not stop the worker — the thread lives as long as
/// the process, same posture as ingest jobs.
pub(crate) struct Refiner {
    shared: Arc<Shared>,
}

/// Everything the prime can be woken FOR, under the one mutex its
/// condvar is tied to — signal state outside this struct would
/// reintroduce the lost-wake window (check flag → drone sets flag and
/// notifies into the void → prime sleeps a full ambient period on a
/// stale decision). Correct by construction: you cannot signal the
/// prime without holding the inbox.
#[derive(Default)]
struct PrimeInbox {
    /// Just-ingested blob ids for the fresh tier.
    fresh: Vec<i64>,
    /// Drone → prime: a drain burst finished real work, so the routes
    /// it minted are waiting on a maintenance pass (the drop-a-zip →
    /// auto-evict one-wake motion).
    maintenance_due: bool,
}

struct Shared {
    inbox: Mutex<PrimeInbox>,
    wake: Condvar,
    /// Drones currently inside a drain burst. The prime's family job
    /// stays open while this is nonzero and its queue is nonempty —
    /// completion must not race a drone holding the last item.
    active_drones: std::sync::atomic::AtomicUsize,
    /// D93 drone signal: the prime bumps the generation after every
    /// enqueue pass; drones drain when it moves (or on DRONE_POLL).
    drone_gen: Mutex<u64>,
    drone_wake: Condvar,
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
            inbox: Mutex::new(PrimeInbox::default()),
            wake: Condvar::new(),
            active_drones: std::sync::atomic::AtomicUsize::new(0),
            drone_gen: Mutex::new(0),
            drone_wake: Condvar::new(),
        });
        let worker_shared = Arc::clone(&shared);
        std::thread::spawn(move || worker(db_dir, store, &jobs, worker_shared));
        Self { shared }
    }

    /// Feed just-ingested blob ids into the fresh tier and wake the
    /// worker. Ids come from the SAME database the worker reads
    /// (`IngestReport::fresh_blobs`).
    pub(crate) fn notify_fresh(&self, blob_ids: Vec<i64>) {
        if blob_ids.is_empty() {
            return;
        }
        lock(&self.shared.inbox).fresh.extend(blob_ids);
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

/// D93 worker count: `refine:workers` ("auto" or a number ≥ 1; 1
/// restores the single-worker shape). Auto = ⌈n/2⌉ clamped to 6.
///
/// Why not scale with cores: nice(19) already protects the request
/// path, so CPU count is NOT the binding constraint — three other
/// ceilings are. (1) Claims serialize on the IMMEDIATE write lock,
/// so small-item throughput plateaus around a handful of workers.
/// (2) Preflate's split state is ~70 MiB worst-case per active
/// worker (4 MiB window + 32 MiB plaintext limit + pending frame) —
/// six workers is ~0.4 GiB of ceiling, acceptable on a NAS-adjacent
/// daemon; thirty would not be. (3) The corpus lives on NFS, where
/// interleaving many sequential readers can sink aggregate
/// throughput below one reader on spinning arrays. A 32-core box
/// with NVMe and RAM to spare overrides the molten knob; the DEFAULT
/// serves the deployment the docs actually target.
fn worker_count(db: &Db) -> usize {
    if let Ok(Some(raw)) = db.config_get("refine:workers")
        && let Ok(text) = std::str::from_utf8(&raw)
        && let Ok(n) = text.trim().parse::<usize>()
        && n >= 1
    {
        return n;
    }
    let n = std::thread::available_parallelism().map_or(1, std::num::NonZero::get);
    n.div_ceil(2).clamp(1, 6)
}

fn worker(
    db_dir: std::path::PathBuf,
    store: &'static Store,
    jobs: &Registry,
    shared: Arc<Shared>,
) {
    // Lowest CPU priority for THIS thread (Linux niceness is per-task,
    // so the request path is untouched). Best-effort everywhere else.
    if let Err(e) = rustix::process::nice(19) {
        warn!("refine worker: nice(19) failed ({e}); running at normal priority");
    }
    let mut db = match Db::open(&db_dir) {
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
    // D92: analyzers read the LOGICAL CAS — absent-but-grounded items
    // spill through this executor. Losing it means no byte source at
    // all, the same severity as losing the databases.
    let exec = match datboi_exec::Executor::new(store, datboi_exec::ExecConfig::default()) {
        Ok(exec) => Arc::new(exec),
        Err(e) => {
            error!("refine worker: executor: {e} — ambient refinement is OFF");
            return;
        }
    };
    // D93: spawn the drone fleet AFTER lease amnesty (a drone claiming
    // pre-amnesty would only duplicate a pure function — dedup grade —
    // but there is no reason to invite it).
    let workers = worker_count(&db);
    for ix in 1..workers {
        let db_dir = db_dir.clone();
        let exec = Arc::clone(&exec);
        let shared = Arc::clone(&shared);
        std::thread::spawn(move || drone(ix, &db_dir, store, &exec, &shared));
    }
    info!(
        "refinement: {workers} worker(s) (1 prime + {} drone(s))",
        workers - 1
    );
    let bytes = Logical::new(store, &exec);
    let mut analyzers = families();
    // The D72/D73 maintenance phases ride this same thread (one
    // background writer). Losing them degrades to refine-only, loudly.
    let maintainer = match crate::maintain::Maintainer::new(store, &db_dir) {
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
        // Take the whole inbox in one lock hold; when it is empty and
        // the ambient clock is idle, WAIT UNDER THE SAME HOLD — the
        // re-check-then-sleep is atomic against every signaler, so a
        // wake cannot be lost between the decision and the sleep.
        let (fresh, drone_work_done) = {
            let mut inbox = lock(&shared.inbox);
            let ambient_due = Instant::now() >= next_ambient;
            if inbox.fresh.is_empty() && !inbox.maintenance_due && !ambient_due {
                let timeout = next_ambient.saturating_duration_since(Instant::now());
                drop(shared.wake.wait_timeout(inbox, timeout));
                continue;
            }
            (
                std::mem::take(&mut inbox.fresh),
                std::mem::take(&mut inbox.maintenance_due),
            )
        };
        let ambient_due = Instant::now() >= next_ambient;
        let _ = drone_work_done; // consumed: this pass runs maintenance below.
        if let Err(e) = enqueue(&mut db, &mut analyzers, &fresh, ambient_due) {
            warn!("refine worker: enqueue: {e}");
            std::thread::sleep(ERROR_BACKOFF);
            continue;
        }
        if ambient_due {
            next_ambient = Instant::now() + AMBIENT_RESCAN;
        }
        // Wake the drones: the queues just gained (or regained) work.
        {
            *lock(&shared.drone_gen) += 1;
            shared.drone_wake.notify_all();
        }
        for analyzer in &mut analyzers {
            drain_family(&mut db, store, &bytes, jobs, &shared, analyzer.as_mut());
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

/// A D93 drone: claim-analyze-complete and nothing else. No tray
/// jobs (the prime's per-family job tracks progress by queue depth,
/// which drone completions shrink), no maintenance, no enqueueing —
/// pure drain bandwidth over the same leased queue.
fn drone(
    ix: usize,
    db_dir: &std::path::Path,
    store: &'static Store,
    exec: &datboi_exec::Executor<'static>,
    shared: &Shared,
) {
    if let Err(e) = rustix::process::nice(19) {
        warn!("refine drone {ix}: nice(19) failed ({e}); running at normal priority");
    }
    let mut db = match Db::open(db_dir) {
        Ok(db) => db,
        Err(e) => {
            warn!("refine drone {ix}: cannot open databases: {e} — drone off");
            return;
        }
    };
    let bytes = Logical::new(store, exec);
    let mut analyzers = families();
    let mut seen = 0u64;
    loop {
        {
            let generation = lock(&shared.drone_gen);
            let generation = if *generation == seen {
                shared
                    .drone_wake
                    .wait_timeout(generation, DRONE_POLL)
                    .map(|(g, _)| g)
                    .unwrap_or_else(|e| e.into_inner().0)
            } else {
                generation
            };
            seen = *generation;
        }
        // Drain until nothing claims: leased/settled items make the
        // claim come back empty, so error backoff (items keep their
        // lease) can't hot-spin this loop. The activity bracket is
        // what lets the prime's job completion wait for us.
        shared
            .active_drones
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut did_work = false;
        loop {
            let mut progressed = false;
            for analyzer in &mut analyzers {
                match process_round(
                    &mut db,
                    store,
                    &bytes,
                    analyzer.as_mut(),
                    DRONE_ROUND,
                    &mut datboi_ingest::refine::NoObserver,
                ) {
                    Ok(report) => {
                        progressed |= report.analyzed > 0;
                        for (hash, error) in &report.errors {
                            debug!("refine drone {ix}: {hash}: {error}");
                        }
                    }
                    Err(e) => {
                        warn!("refine drone {ix}: {e}");
                        std::thread::sleep(ERROR_BACKOFF);
                    }
                }
            }
            did_work |= progressed;
            if !progressed {
                break;
            }
        }
        shared
            .active_drones
            .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
        // The routes this burst minted want a maintenance pass NOW
        // (licensing → watermark is the one-wake motion). Signal
        // UNDER the inbox lock: the prime's sleep decision holds the
        // same lock, so this wake cannot fall into the gap between
        // its check and its wait.
        if did_work {
            lock(&shared.inbox).maintenance_due = true;
            shared.wake.notify_one();
        }
    }
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
    bytes: &Logical<'_, '_>,
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
    // Fleet-wide outcome baseline (D93): the note reports what the
    // WHOLE drain concluded — drones included — as a provenance-table
    // delta, never this worker's private counters.
    let (pos0, neg0) = db.analysis_counts(&id).unwrap_or((0, 0));
    let (mut done, mut failed) = (0u64, 0u64);
    loop {
        // Mid-drain ingests jump the line: their ids enter the fresh
        // tier now, and the per-item claim picks them up next.
        let fresh = std::mem::take(&mut lock(&shared.inbox).fresh);
        if !fresh.is_empty()
            && let Err(e) = db.enqueue_fresh(&id, &fresh, now_unix())
        {
            // Re-stage for the outer loop rather than losing them.
            lock(&shared.inbox).fresh.extend(fresh);
            warn!("refine worker: fresh enqueue: {e}");
        }
        let mut observer = TrayObserver { jobs, job };
        let report = match process_round(db, store, bytes, analyzer, ROUND, &mut observer) {
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
        for (hash, error) in &report.errors {
            failed += 1;
            debug!("refine job {job}: {hash}: {error}");
            jobs.refine_error(job, &hash.to_hex(), error);
        }
        let remaining = db.sweep_queue_len(&id).unwrap_or(0);
        jobs.refine_progress(job, done, done + remaining);
        if report.analyzed == 0 && report.errors.is_empty() {
            // Nothing claimable. If a drone is mid-burst and this
            // family still has leased items, the drain is NOT done —
            // finishing now would under-report the note and flash a
            // false "done" (the race the ingest e2e caught). Error-
            // leased leftovers with an idle fleet break out as before.
            let leased = db.sweep_queue_len(&id).unwrap_or(0);
            let fleet_busy = shared
                .active_drones
                .load(std::sync::atomic::Ordering::Relaxed)
                > 0;
            if leased == 0 || !fleet_busy {
                break;
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    }
    let (pos1, neg1) = db.analysis_counts(&id).unwrap_or((0, 0));
    jobs.push_note(
        job,
        format!(
            "{} rebuildable, {} concluded negative, {failed} error(s)",
            pos1.saturating_sub(pos0),
            neg1.saturating_sub(neg0)
        ),
    );
    let remaining = db.sweep_queue_len(&id).unwrap_or(0);
    let total = queued.max(done + remaining);
    jobs.refine_progress(job, total.saturating_sub(remaining), total);
    jobs.finish(job, now_unix());
    info!(
        "refine job {job}: done — {done} analyzed by this worker ({} fleet-wide positive, {} negative, {failed} error(s))",
        pos1.saturating_sub(pos0),
        neg1.saturating_sub(neg0)
    );
}
