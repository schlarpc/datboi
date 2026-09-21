//! The bulk blessing pass (D63's "optional background blessing pass",
//! built as D121): find every output whose first read would have to
//! materialize a route, materialize them ALL now, in parallel, and
//! leave the obao4 sidecars behind.
//!
//! ## Why a pass at all
//!
//! The D63 amendment made `serve_range` bless a non-affine route on
//! demand — the read that finds no sidecar pays for one, once, and every
//! later read is off the tree. That is right for a cold read and wrong
//! for a cold CORPUS: the bill is charged to whoever reads first, and on
//! the deployment this was written for that is `mame -verifyroms`, a
//! single serial process walking 107,090 members. One client's serialism
//! becomes the daemon's wall clock while seven of eight cores idle.
//! Nothing about the work is serial — each member is an independent
//! inflate over its own window of a container — so it belongs in a pool.
//!
//! ## Shape (D120, minus a lane)
//!
//! A bounded pool of workers does the materialize-and-hash
//! (`open_sequential` → `obao::compute` → `put_obao`) and touches no
//! `Db`. The coordinator owns the only `Db` handle and does every read
//! BEFORE dispatch — the candidate page, the plan, the carve-out
//! verdict — exactly where D120 puts its rescan-cache lookup, and for
//! the same reason: a decision that costs a worker nothing to make must
//! not cost it a materialization to discover.
//!
//! D120's writer half has nothing to do here. Blessing mutates no `Db`
//! row at all; the sidecar in the store IS the entire record. So there
//! is no commit queue to order, and the one ordered output — the failure
//! list — is made deterministic by sorting on the candidate sequence
//! number. D120 rejected sort-at-the-end because `notes`, `errors` and
//! `fresh_blobs` have no key that reproduces walk order; this report has
//! exactly one list and exactly such a key, so that rejection does not
//! reach it.
//!
//! ## What bounds memory
//!
//! `obao::compute` streams its input but buffers its OUTPUT: ~64 bytes
//! of tree per 16 KiB of blob, ~0.4% of the length. That is the only
//! thing a job holds whole, so — per D120's amendment, which charges a
//! file what it may BUFFER rather than what it is — a job is charged
//! `outboard_size(len)` against a per-worker tree budget, and a job
//! heavier than the whole budget runs alone rather than never.
//!
//! ## What it is resumable ON
//!
//! Nothing. There is no checkpoint and no progress table, because the
//! sidecar already is one: `put_obao` is temp → fsync → rename and
//! idempotent, so an interrupted run loses at most its in-flight
//! materializations and a re-run skips everything already blessed. The
//! same argument makes it safe beside a live daemon blessing on demand.

use std::collections::VecDeque;
use std::num::NonZeroUsize;
use std::sync::{Mutex, PoisonError, mpsc};

use datboi_core::hash::Blake3;
use datboi_index::Db;
use datboi_store_fs::{Namespace as StoreNs, obao};

use crate::{ExecError, Executor, Plan};

/// Outboard bytes one worker may have in flight. 64 MiB of tree is a
/// ~16 GiB blob, so nothing in a rom corpus ever queues on this; it
/// exists so that a pool pointed at TB-scale images cannot multiply one
/// enormous tree by the worker count.
const TREE_BUDGET_PER_WORKER: u64 = 64 << 20;

/// Dispatched-but-unfinished ceiling. The byte budget alone does not
/// bound the queue — a 17 KiB member is charged 64 bytes of tree, so a
/// corpus of small members would admit millions of them into the
/// channel. This is the count bound beside it (D120's `REORDER_CAP`
/// plays the same role there).
const QUEUE_CAP: usize = 256;

/// Candidate rows read per keyset page: enough that the query cost
/// disappears against the materializations, small enough that the
/// buffer is noise (~40 bytes a row).
const PAGE: usize = 4096;

#[derive(Debug, Clone, Default)]
pub struct BlessOptions {
    /// Worker count; 0 derives it from the machine (D120's convention).
    pub parallelism: usize,
    /// Also bless routes the D63 carve-out already serves — the
    /// promotion D63's own sentence describes, opt-in because D63
    /// rejected paying for it by default (D121).
    pub include_affine: bool,
    /// Decide and count, materialize nothing.
    pub dry_run: bool,
    /// Stop after selecting this many blessings; 0 = no limit.
    pub limit: u64,
}

impl BlessOptions {
    /// The worker count this config asks for, never zero.
    #[must_use]
    pub fn workers(&self) -> usize {
        if self.parallelism > 0 {
            return self.parallelism;
        }
        std::thread::available_parallelism().map_or(1, NonZeroUsize::get)
    }
}

/// What one pass concluded. Counts and hex hashes only — no store or DB
/// borrows, so the CLI prints it and a caller may keep it.
#[derive(Debug, Default, Clone)]
pub struct BlessReport {
    /// Candidates triaged: rows the index offered, before any verdict.
    pub examined: u64,
    /// Already had a tree (a previous run, ingest, or the daemon).
    pub already_blessed: u64,
    /// Bytes are local after all: `serve_range` reads them directly
    /// under D4's cheap default, so there is nothing to materialize.
    pub resident: u64,
    /// The D63 affine carve-out already serves every byte of these.
    /// Skipped unless [`BlessOptions::include_affine`].
    pub carved_out: u64,
    /// Claimed, but nothing non-poisoned can produce them (peer
    /// advertisements, evicted-and-stranded rows).
    pub no_route: u64,
    /// Candidates that need a blessing — what a `--dry-run` reports,
    /// and what a real run then works through.
    pub selected: u64,
    /// Content bytes behind [`Self::selected`].
    pub selected_bytes: u64,
    /// Blessings completed this run.
    pub blessed: u64,
    /// Content bytes materialized this run.
    pub bytes: u64,
    /// (hex hash, error) for candidates that could not be blessed, in
    /// candidate order. A blessing failure is a real failure — D49's
    /// never-bad-bytes rule leaves no soft fallback — but one bad route
    /// must not abandon the other 107,089.
    pub failed: Vec<(String, String)>,
}

impl BlessReport {
    /// Nothing refused to bless.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.failed.is_empty()
    }

    /// Blessings still owed after this run — non-zero when a `--limit`
    /// cut it short, or when something failed.
    #[must_use]
    pub fn outstanding(&self) -> u64 {
        self.selected.saturating_sub(self.blessed)
    }
}

/// The coordinator's mutable half, in one place so the loop does not
/// take an argument per counter.
struct State<'p> {
    report: BlessReport,
    /// Failures keyed by candidate sequence: the pool finishes out of
    /// order, and this is the one ordered output.
    failures: Vec<(u64, String, String)>,
    progress: &'p mut dyn FnMut(&BlessReport),
}

impl State<'_> {
    fn tick(&mut self) {
        (self.progress)(&self.report);
    }

    fn fail(&mut self, seq: u64, hash: &str, err: &str) {
        self.failures.push((seq, hash.to_owned(), err.to_owned()));
    }
}

/// One planned blessing, handed to a worker.
struct Job {
    seq: u64,
    hash: Blake3,
    len: u64,
    plan: Plan,
    /// Tree bytes charged to the in-flight budget.
    weight: u64,
}

/// A worker's answer, back on the coordinator.
struct Done {
    seq: u64,
    hash: Blake3,
    len: u64,
    weight: u64,
    result: Result<bool, ExecError>,
}

impl Executor<'_> {
    /// Bless every output that needs it (D63/D121).
    ///
    /// `progress` is called once per retired candidate with the report
    /// so far — the pass runs for minutes on a real corpus and must not
    /// be one silent block. Throttling is the CALLER's: this crate
    /// knows nothing about terminals.
    ///
    /// # Errors
    /// Environmental index failures only. A route that refuses to bless
    /// is a `failed` entry, not an `Err` (D81: deterministic conclusions
    /// about bytes are verdicts).
    pub fn bless_corpus(
        &self,
        db: &Db,
        opts: &BlessOptions,
        progress: &mut dyn FnMut(&BlessReport),
    ) -> Result<BlessReport, ExecError> {
        let mut state = State {
            report: BlessReport::default(),
            failures: Vec::new(),
            progress,
        };
        let workers = opts.workers();

        let (job_tx, job_rx) = mpsc::channel::<Job>();
        let job_rx = Mutex::new(job_rx);
        let (done_tx, done_rx) = mpsc::channel::<Done>();

        let result = std::thread::scope(|scope| {
            for _ in 0..workers {
                let done_tx = done_tx.clone();
                let job_rx = &job_rx;
                scope.spawn(move || {
                    loop {
                        // Held across the recv only: one lock per
                        // member is nothing beside an inflate.
                        let job = {
                            let rx = job_rx.lock().unwrap_or_else(PoisonError::into_inner);
                            rx.recv()
                        };
                        let Ok(job) = job else { return };
                        // A panicking worker must become one failed
                        // hash, never a coordinator blocked forever on
                        // a result nobody will send.
                        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            self.bless_plan(&job.hash, &job.plan)
                        }))
                        .unwrap_or_else(|_| {
                            Err(ExecError::Malformed(format!(
                                "blessing {} panicked",
                                job.hash
                            )))
                        });
                        let done = Done {
                            seq: job.seq,
                            hash: job.hash,
                            len: job.len,
                            weight: job.weight,
                            result,
                        };
                        if done_tx.send(done).is_err() {
                            return;
                        }
                    }
                });
            }
            // Ours would keep the channel alive forever, and a retired
            // pool has to read as a disconnect.
            drop(done_tx);
            let outcome = self.bless_loop(db, opts, &job_tx, &done_rx, &mut state);
            // Closing the job channel retires the pool; the scope joins.
            drop(job_tx);
            outcome
        });
        result?;

        let State {
            mut report,
            mut failures,
            ..
        } = state;
        failures.sort_unstable_by_key(|(seq, _, _)| *seq);
        report.failed = failures
            .into_iter()
            .map(|(_, hash, err)| (hash, err))
            .collect();
        Ok(report)
    }

    /// The coordinator: page candidates, triage them against the `Db`,
    /// dispatch within the in-flight budget, retire verdicts. EVERY
    /// `Db` read the pass makes happens on this thread.
    fn bless_loop(
        &self,
        db: &Db,
        opts: &BlessOptions,
        jobs: &mpsc::Sender<Job>,
        done: &mpsc::Receiver<Done>,
        state: &mut State<'_>,
    ) -> Result<(), ExecError> {
        let cap = (opts.workers() as u64).saturating_mul(TREE_BUDGET_PER_WORKER);
        let mut next_seq = 0u64;
        let mut inflight = 0u64;
        // Dispatched, result not yet received: the coordinator may
        // block on the done channel exactly when this is non-zero.
        let mut outstanding = 0usize;
        // A job the budget turned away. The walk stops behind it rather
        // than dispatching past it, so the charge cannot be starved by
        // a queue of cheap members jumping it.
        let mut held: Option<Job> = None;
        let mut page: VecDeque<(i64, Blake3, u64)> = VecDeque::new();
        let mut cursor = 0i64;
        let mut walking = true;

        loop {
            if let Some(job) = held.take() {
                if admits(inflight, cap, job.weight) {
                    dispatch(jobs, job, &mut inflight, &mut outstanding);
                } else {
                    held = Some(job);
                }
            }

            while held.is_none() && walking && outstanding < QUEUE_CAP {
                if page.is_empty() {
                    page = db
                        .bless_candidates_after(cursor, obao::GROUP_BYTES, PAGE)?
                        .into();
                    let Some((last, _, _)) = page.back() else {
                        walking = false;
                        break;
                    };
                    cursor = *last;
                }
                let Some((_, hash, len)) = page.pop_front() else {
                    continue;
                };
                // Every candidate takes a sequence number, skipped ones
                // included: it is the failure list's sort key, and a
                // shared key would make the order depend on the sort.
                let seq = next_seq;
                next_seq += 1;
                state.report.examined += 1;
                let Some(plan) = self.bless_triage(db, &hash, opts, state, seq)? else {
                    state.tick();
                    continue;
                };
                state.report.selected += 1;
                state.report.selected_bytes += len;
                if opts.limit != 0 && state.report.selected >= opts.limit {
                    // Staging is over, and the rest of the page goes
                    // with it: `walking` is the only termination
                    // signal, so leaving rows behind would be a
                    // coordinator that never decides it is done.
                    walking = false;
                    page.clear();
                }
                if opts.dry_run {
                    state.tick();
                    continue;
                }
                let job = Job {
                    seq,
                    hash,
                    len,
                    plan,
                    weight: obao::outboard_size(len),
                };
                if admits(inflight, cap, job.weight) {
                    dispatch(jobs, job, &mut inflight, &mut outstanding);
                } else {
                    held = Some(job);
                }
            }

            if outstanding == 0 {
                // Nothing is owed. `walking` is the ONE staging signal
                // (the walk ran out, or `--limit` closed it), and
                // `held` is always admissible against an empty budget,
                // so a coordinator with neither is finished — and one
                // with either makes progress on the next turn.
                if !walking && held.is_none() {
                    return Ok(());
                }
                continue;
            }
            let Ok(d) = done.recv() else {
                // Every worker is gone with results owed.
                let detail = format!("blessing pool died with {outstanding} member(s) in flight");
                state.fail(next_seq, "", &detail);
                return Ok(());
            };
            outstanding -= 1;
            inflight = inflight.saturating_sub(d.weight);
            match d.result {
                Ok(true) => {
                    state.report.blessed += 1;
                    state.report.bytes += d.len;
                }
                // Someone else blessed it between triage and the
                // worker's re-check: the goal state, reached cheaper.
                Ok(false) => {
                    state.report.blessed += 1;
                    state.report.already_blessed += 1;
                }
                Err(e) => state.fail(d.seq, &d.hash.to_string(), &e.to_string()),
            }
            state.tick();
        }
    }

    /// Everything the coordinator decides about one candidate before a
    /// worker may see it. `Ok(None)` is a counted skip.
    fn bless_triage(
        &self,
        db: &Db,
        hash: &Blake3,
        opts: &BlessOptions,
        state: &mut State<'_>,
        seq: u64,
    ) -> Result<Option<Plan>, ExecError> {
        if self.store.has_obao(StoreNs::Data, hash)? {
            state.report.already_blessed += 1;
            return Ok(None);
        }
        // The index said non-resident; the store is the authority (a
        // recovery window, or a materialize since the page was read).
        // Local bytes serve under D4's cheap default, so nothing here
        // is unreadable — and `ensure_obao` over every resident blob is
        // a full pass over the corpus, which is the cost D63 refused.
        // `datboi scrub` is where a whole-corpus read belongs.
        if self.store.has(StoreNs::Data, hash) {
            state.report.resident += 1;
            return Ok(None);
        }
        let plan = match self.plan(db, hash, 0, &mut Vec::new()) {
            Ok(plan) => plan,
            // Expected and uninteresting: a hash a peer advertised, or
            // one whose only routes are poisoned.
            Err(ExecError::NoRoute(_)) => {
                state.report.no_route += 1;
                return Ok(None);
            }
            // A cycle, a depth blowout, an unsupported op — the route
            // exists and refuses to plan, which is worth saying out
            // loud rather than filing under "no route".
            Err(e) => {
                state.fail(seq, &hash.to_string(), &e.to_string());
                return Ok(None);
            }
        };
        if !opts.include_affine && self.affine_carveout(db, &plan)? {
            state.report.carved_out += 1;
            return Ok(None);
        }
        Ok(Some(plan))
    }
}

/// Whether the in-flight tree budget has room. An empty budget admits
/// anything: the job the walk is holding always runs, so an outboard
/// bigger than the whole cap is slow, never stuck.
fn admits(inflight: u64, cap: u64, weight: u64) -> bool {
    inflight == 0 || inflight.saturating_add(weight) <= cap
}

fn dispatch(jobs: &mpsc::Sender<Job>, job: Job, inflight: &mut u64, outstanding: &mut usize) {
    let weight = job.weight;
    if jobs.send(job).is_ok() {
        *inflight += weight;
        *outstanding += 1;
    }
    // A closed channel means every worker is gone; the coordinator
    // notices on the next `recv` and reports it there.
}
