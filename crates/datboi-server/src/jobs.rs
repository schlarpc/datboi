//! The job registry: in-memory LIVE surface + the D74 durable ledger.
//!
//! Running jobs live here (progress moves without touching disk);
//! the ledger (datboi-index jobs.rs, state.db) gets exactly three
//! writes per job — insert at create, finalize at finish/fail, prune —
//! and hydrates the finished tail back into this registry at startup,
//! so history survives restarts and ids stay unique across them (the
//! db assigns them). Rows still `running` at startup are marked
//! interrupted: a crashed 40-minute job leaves a tombstone, never a
//! blank. Upload tokens remain memory-only on purpose (staged bytes
//! are swept with the store's tmp/; a token that outlives its bytes
//! would be a lie).

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::Mutex;

use datboi_api::{
    IngestErrorItem, IngestMemberSkipItem, IngestReportBody, Job, JobDetail, JobKind, JobRunState,
    MatchedEntry,
};
use datboi_index::jobs::{JobRow, LEDGER_KEEP};
use datboi_index::{Db, JobKind as LedgerKind, JobState as LedgerState};
use datboi_ingest::IngestReport;
use tracing::warn;

/// Finished jobs kept IN MEMORY for the tray after they complete;
/// running jobs are never pruned.
const KEEP_FINISHED: usize = 20;

/// Ledger kinds ↔ the wire enum. The vocabulary is defined ONCE in
/// datboi-index (the CLI's ledger_stamp writes it too); this pair only
/// translates, and the exhaustive matches mean a new kind on either
/// side fails to compile until it gets a partner.
fn to_ledger(kind: JobKind) -> LedgerKind {
    match kind {
        JobKind::Ingest => LedgerKind::Ingest,
        JobKind::Refine => LedgerKind::Refine,
        JobKind::Gc => LedgerKind::Gc,
        JobKind::Scrub => LedgerKind::Scrub,
        JobKind::Eval => LedgerKind::Eval,
        JobKind::Mint => LedgerKind::Mint,
    }
}

fn from_ledger(kind: LedgerKind) -> JobKind {
    match kind {
        LedgerKind::Ingest => JobKind::Ingest,
        LedgerKind::Refine => JobKind::Refine,
        LedgerKind::Gc => JobKind::Gc,
        LedgerKind::Scrub => JobKind::Scrub,
        LedgerKind::Eval => JobKind::Eval,
        LedgerKind::Mint => JobKind::Mint,
    }
}

/// One staged upload: bytes already on disk in the store's tmp/,
/// waiting for a `POST /v1/ingest` to spend the token.
#[derive(Debug)]
pub(crate) struct StagedUpload {
    /// Staging path (a flat file in `<store>/tmp/`).
    pub(crate) path: PathBuf,
    /// The client's original relative name — every report entry wears
    /// this, never the staging path.
    pub(crate) name: String,
    pub(crate) bytes: u64,
}

struct JobState {
    id: i64,
    kind: JobKind,
    /// Display name; ingest jobs bake the file count in at creation,
    /// refine jobs carry their analyzer family.
    name: String,
    /// For refine jobs these count sweep ITEMS, not files — the honest
    /// unit of that work (JobKind doc in datboi-api).
    files_total: u64,
    files_done: u64,
    bytes_total: u64,
    bytes_done: u64,
    current: Option<String>,
    state: JobRunState,
    report: IngestReportBody,
    /// Newly satisfied entries (capped) + uncapped count; run_job sets
    /// them after the closing relink/rollup pass.
    matched: Vec<MatchedEntry>,
    matched_total: u64,
    error: Option<String>,
    started_at: i64,
    finished_at: Option<i64>,
}

impl JobState {
    /// Byte-weighted at file granularity (the pipeline reports no
    /// intra-file progress), capped at 99 while running so only a
    /// finished job reads 100. Refine jobs mirror item counts into the
    /// byte fields, so the same arithmetic is item-weighted there.
    fn progress(&self) -> u8 {
        match self.state {
            JobRunState::Running => {
                let total = self.bytes_total.max(1);
                ((self.bytes_done * 100 / total) as u8).min(99)
            }
            JobRunState::Done | JobRunState::Failed => 100,
        }
    }

    fn row(&self) -> Job {
        Job {
            id: self.id,
            name: self.name.clone(),
            progress: self.progress(),
            kind: self.kind,
            state: self.state,
            started_at: self.started_at,
            finished_at: self.finished_at.into(),
        }
    }
}

pub(crate) struct Registry {
    uploads: Mutex<HashMap<String, StagedUpload>>,
    jobs: Mutex<JobsInner>,
    /// The D74 ledger's own connection pair (`None` in unit tests):
    /// writes are rare and short, so one mutex is honest.
    ledger: Option<Mutex<Db>>,
}

struct JobsInner {
    next_id: i64,
    /// Oldest first; pruned to running + the last KEEP_FINISHED done.
    jobs: VecDeque<JobState>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl Registry {
    /// Memory-only (unit tests): ids from a counter, nothing persists.
    #[cfg(test)]
    pub(crate) fn ephemeral() -> Self {
        Self {
            uploads: Mutex::new(HashMap::new()),
            jobs: Mutex::new(JobsInner {
                next_id: 1,
                jobs: VecDeque::new(),
            }),
            ledger: None,
        }
    }

    /// The production registry (D74): sweep crash evidence, hydrate
    /// the finished tail, and persist every job from here on.
    pub(crate) fn durable(db: Db, now: i64) -> Result<Self, datboi_index::IndexError> {
        let interrupted = db.interrupt_running_jobs(now)?;
        if interrupted > 0 {
            warn!(
                "jobs: {interrupted} job(s) were running when the last daemon died — marked interrupted"
            );
        }
        let mut jobs = VecDeque::new();
        for row in db.recent_jobs(KEEP_FINISHED)? {
            jobs.push_back(hydrate(row));
        }
        Ok(Self {
            uploads: Mutex::new(HashMap::new()),
            jobs: Mutex::new(JobsInner { next_id: 1, jobs }),
            ledger: Some(Mutex::new(db)),
        })
    }

    /// Record a staged upload under a fresh token.
    pub(crate) fn stage(&self, token: String, upload: StagedUpload) {
        lock(&self.uploads).insert(token, upload);
    }

    /// Spend tokens all-or-nothing. On an unknown token, nothing is
    /// consumed and the offender comes back as the error.
    pub(crate) fn take(&self, tokens: &[String]) -> Result<Vec<StagedUpload>, String> {
        let mut uploads = lock(&self.uploads);
        if let Some(missing) = tokens.iter().find(|t| !uploads.contains_key(*t)) {
            return Err(missing.clone());
        }
        Ok(tokens
            .iter()
            .map(|t| uploads.remove(t).expect("presence checked under the lock"))
            .collect())
    }

    fn push_job(
        &self,
        kind: JobKind,
        name: String,
        files_total: u64,
        bytes_total: u64,
        now: i64,
    ) -> i64 {
        // The ledger assigns ids (unique across restarts); the counter
        // is the ephemeral fallback — and the escape hatch if the
        // ledger write fails (a job must run even when history can't).
        let ledger_id = self.ledger.as_ref().and_then(|db| {
            lock(db)
                .insert_job(to_ledger(kind), &name, now)
                .map_err(|e| warn!("jobs: ledger insert failed ({e}); job runs unrecorded"))
                .ok()
        });
        let mut inner = lock(&self.jobs);
        let id = ledger_id.unwrap_or(inner.next_id);
        inner.next_id = inner.next_id.max(id) + 1;
        inner.jobs.push_back(JobState {
            id,
            kind,
            name,
            files_total,
            files_done: 0,
            bytes_total,
            bytes_done: 0,
            current: None,
            state: JobRunState::Running,
            report: IngestReportBody::default(),
            matched: Vec::new(),
            matched_total: 0,
            error: None,
            started_at: now,
            finished_at: None,
        });
        id
    }

    /// Create a running ingest job over the staged set; answers the id.
    pub(crate) fn create(&self, files: &[StagedUpload], now: i64) -> i64 {
        let name = format!(
            "ingest — {} file{}",
            files.len(),
            if files.len() == 1 { "" } else { "s" }
        );
        self.push_job(
            JobKind::Ingest,
            name,
            files.len() as u64,
            files.iter().map(|f| f.bytes).sum(),
            now,
        )
    }

    /// Create a running refine job for one analyzer family's drain
    /// (D71). Totals start at the current queue depth and move via
    /// [`Registry::refine_progress`] as the drain claims and finishes
    /// items (fresh arrivals can grow the total mid-drain).
    pub(crate) fn create_refine(&self, family: &str, queued: u64, now: i64) -> i64 {
        self.push_job(
            JobKind::Refine,
            format!("refine — {family}"),
            queued,
            queued,
            now,
        )
    }

    /// A GC-family maintenance job (D72/D73): licensing drain,
    /// watermark eviction, orphan apply. Same item-counting shape as
    /// refine.
    pub(crate) fn create_gc(&self, name: &str, items: u64, now: i64) -> i64 {
        self.push_job(JobKind::Gc, name.to_owned(), items, items, now)
    }

    /// A verify-one job (D80): scrub kind, item-counted like refine.
    pub(crate) fn create_scrub(&self, name: &str, items: u64, now: i64) -> i64 {
        self.push_job(JobKind::Scrub, name.to_owned(), items, items, now)
    }

    /// A view-evaluation job (D96): one opaque `evaluate_view` call, so
    /// there is no intra-op progress to weight — it reads running until
    /// the snapshot lands. The closing note carries the row/missing
    /// summary.
    pub(crate) fn create_eval(&self, view: &str, now: i64) -> i64 {
        self.push_job(JobKind::Eval, format!("eval — {view}"), 0, 0, now)
    }

    /// A FAT32 image-mint job (D62/D96): opaque like eval (materialize
    /// missing inputs, then mint), summarized in the closing note.
    pub(crate) fn create_mint(&self, view: &str, now: i64) -> i64 {
        self.push_job(JobKind::Mint, format!("mint — {view}"), 0, 0, now)
    }

    /// Refine drain progress: `done` items finished, `total` = done +
    /// still queued. Item counts mirror into the byte fields so the
    /// shared progress arithmetic stays item-weighted.
    pub(crate) fn refine_progress(&self, id: i64, done: u64, total: u64) {
        self.with_job(id, |j| {
            j.files_done = done;
            j.files_total = total;
            j.bytes_done = done;
            j.bytes_total = total;
        });
    }

    /// One refine item failed (analyzer error, not a negative
    /// conclusion): rides the report's error lane under the blob hash.
    pub(crate) fn refine_error(&self, id: i64, blob: &str, error: &str) {
        self.with_job(id, |j| {
            j.report.errors.push(IngestErrorItem {
                path: blob.to_owned(),
                error: error.to_owned(),
            });
        });
    }

    /// Closing summary line for a refine drain (outcome counts).
    pub(crate) fn push_note(&self, id: i64, note: String) {
        self.with_job(id, |j| j.report.notes.push(note));
    }

    /// Prune finished history beyond the keep window (running jobs are
    /// exempt however old). Runs when a job ENTERS a finished state —
    /// the only moment the finished count grows.
    fn prune(inner: &mut JobsInner) {
        let finished = inner
            .jobs
            .iter()
            .filter(|j| j.state != JobRunState::Running)
            .count();
        let mut drop = finished.saturating_sub(KEEP_FINISHED);
        inner.jobs.retain(|j| {
            if j.state == JobRunState::Running || drop == 0 {
                true
            } else {
                drop -= 1;
                false
            }
        });
    }

    fn with_job(&self, id: i64, f: impl FnOnce(&mut JobState)) {
        let mut inner = lock(&self.jobs);
        if let Some(job) = inner.jobs.iter_mut().find(|j| j.id == id) {
            f(job);
        }
    }

    pub(crate) fn set_current(&self, id: i64, name: &str) {
        self.with_job(id, |j| j.current = Some(name.to_owned()));
    }

    /// One file finished (well or badly — refusals ride the report):
    /// merge its translated report and advance the byte counters.
    pub(crate) fn file_done(&self, id: i64, bytes: u64, per_file: IngestReportBody) {
        self.with_job(id, |j| {
            j.current = None;
            j.files_done += 1;
            j.bytes_done += bytes;
            merge(&mut j.report, per_file);
        });
    }

    /// The shelf lights: entries the job's content newly satisfied,
    /// diffed across the run by run_job's matched computation.
    pub(crate) fn set_matched(&self, id: i64, matched: Vec<MatchedEntry>, total: u64) {
        self.with_job(id, |j| {
            j.matched = matched;
            j.matched_total = total;
        });
    }

    pub(crate) fn finish(&self, id: i64, now: i64) {
        {
            let mut inner = lock(&self.jobs);
            if let Some(j) = inner.jobs.iter_mut().find(|j| j.id == id) {
                j.state = JobRunState::Done;
                j.current = None;
                j.finished_at = Some(now);
            }
            Self::prune(&mut inner);
        }
        self.persist(id, LedgerState::Done, now);
    }

    /// Infrastructure failure (not a per-file refusal): the job's
    /// closing relink/rollup pass hitting the db is exactly this case.
    pub(crate) fn fail(&self, id: i64, error: &str, now: i64) {
        {
            let mut inner = lock(&self.jobs);
            if let Some(j) = inner.jobs.iter_mut().find(|j| j.id == id) {
                j.state = JobRunState::Failed;
                j.current = None;
                j.error = Some(error.to_owned());
                j.finished_at = Some(now);
            }
            Self::prune(&mut inner);
        }
        self.persist(id, LedgerState::Failed, now);
    }

    /// Finalize the ledger row with the frozen wire detail (D74).
    /// Best-effort by design: history failing to write must never fail
    /// the job it describes.
    fn persist(&self, id: i64, state: LedgerState, now: i64) {
        let Some(db) = &self.ledger else { return };
        let detail = self.detail(id).and_then(|d| serde_json::to_vec(&d).ok());
        let db = lock(db);
        if let Err(e) = db.finalize_job(id, state, now, detail.as_deref()) {
            warn!("jobs: ledger finalize failed for job {id}: {e}");
        }
        if let Err(e) = db.prune_jobs(LEDGER_KEEP) {
            warn!("jobs: ledger prune failed: {e}");
        }
    }

    /// Tray rows, newest first — memory (live + this process's
    /// finished tail) MERGED with recent ledger rows memory has never
    /// seen (CLI-written history lands live, not after a restart).
    /// Memory wins on id collision: it is at least as fresh.
    pub(crate) fn list(&self) -> Vec<Job> {
        let mut rows: Vec<Job> = lock(&self.jobs).jobs.iter().map(JobState::row).collect();
        if let Some(db) = &self.ledger
            && let Ok(ledger_rows) = lock(db).recent_jobs(KEEP_FINISHED)
        {
            let seen: std::collections::HashSet<i64> = rows.iter().map(|j| j.id).collect();
            rows.extend(
                ledger_rows
                    .into_iter()
                    .filter(|r| !seen.contains(&r.job_id))
                    .map(|r| hydrate(r).row()),
            );
        }
        rows.sort_by_key(|j| std::cmp::Reverse(j.id)); // ids are monotonic: newest first
        rows
    }

    pub(crate) fn detail(&self, id: i64) -> Option<JobDetail> {
        {
            let inner = lock(&self.jobs);
            if let Some(j) = inner.jobs.iter().find(|j| j.id == id) {
                return Some(detail_of(j));
            }
        }
        // Not in memory: CLI-written or pruned-from-memory history.
        let db = self.ledger.as_ref()?;
        let row = lock(db).job_by_id(id).ok()??;
        Some(detail_of(&hydrate(row)))
    }
}

/// The wire detail of one registry entry (memory-held or hydrated).
fn detail_of(j: &JobState) -> JobDetail {
    JobDetail {
        job: j.row(),
        files_total: j.files_total,
        files_done: j.files_done,
        bytes_total: j.bytes_total,
        bytes_done: j.bytes_done,
        current: j.current.clone(),
        report: j.report.clone(),
        matched: j.matched.clone(),
        matched_total: j.matched_total,
        error: j.error.clone().into(),
    }
}

/// A ledger row back into registry shape (startup hydration). The
/// frozen detail JSON restores counters and report when it parses; a
/// future-shape miss (or an interrupted row, which never finalized)
/// renders a stub from the columns — degraded honestly, never an
/// error.
fn hydrate(row: JobRow) -> JobState {
    let detail: Option<JobDetail> = row
        .detail
        .as_deref()
        .and_then(|bytes| serde_json::from_slice(bytes).ok());
    let (state, error) = match row.state {
        LedgerState::Done => (JobRunState::Done, None),
        LedgerState::Interrupted => (
            JobRunState::Failed,
            Some("interrupted: the daemon restarted while this job was running".to_owned()),
        ),
        LedgerState::Failed => (JobRunState::Failed, None),
        // The insert→push window in push_job: a list() poll can read
        // the ledger row before memory has the live entry. Momentary;
        // the memory row wins on the next poll.
        LedgerState::Running => (JobRunState::Failed, None),
    };
    let mut job = JobState {
        id: row.job_id,
        kind: from_ledger(row.kind),
        name: row.name,
        files_total: 0,
        files_done: 0,
        bytes_total: 0,
        bytes_done: 0,
        current: None,
        state,
        report: IngestReportBody::default(),
        matched: Vec::new(),
        matched_total: 0,
        error,
        started_at: row.started_at,
        finished_at: row.finished_at,
    };
    if let Some(d) = detail {
        job.files_total = d.files_total;
        job.files_done = d.files_done;
        job.bytes_total = d.bytes_total;
        job.bytes_done = d.bytes_done;
        job.report = d.report;
        job.matched = d.matched;
        job.matched_total = d.matched_total;
        job.error = job.error.or(d.error.into_inner());
    }
    job
}

/// Render one file's `IngestReport` for the wire, translating the
/// staging path back to the client's original name — staging paths
/// never leak into responses.
pub(crate) fn translate(report: IngestReport, staged: &StagedUpload) -> IngestReportBody {
    let name = |path: &std::path::Path| {
        if path == staged.path {
            staged.name.clone()
        } else {
            // Shouldn't happen for a flat staged file, but stay honest
            // rather than mislabel.
            path.display().to_string()
        }
    };
    IngestReportBody {
        files_scanned: report.files_scanned as u64,
        files_unchanged: report.files_unchanged as u64,
        files_stored: report.files_stored as u64,
        files_already_present: report.files_already_present as u64,
        chd_v5: report.chd_v5 as u64,
        members_claimed: report.members_claimed as u64,
        members_extracted: report.members_extracted as u64,
        detector_hits: report.detector_hits as u64,
        skipper_skipped_large: report.skipper_skipped_large as u64,
        // The pipeline never imports dats; run_job fills this lane
        // directly for files it classified as dats (ingest.rs).
        dats_imported: Vec::new(),
        errors: report
            .errors
            .iter()
            .map(|(path, error)| IngestErrorItem {
                path: name(path),
                error: error.clone(),
            })
            .collect(),
        member_skips: report
            .member_skips
            .iter()
            .map(|(path, member, reason)| IngestMemberSkipItem {
                path: name(path),
                member: member.clone(),
                reason: reason.clone(),
            })
            .collect(),
        notes: report.notes,
    }
}

/// Accumulate one file's report into the job's running totals.
fn merge(into: &mut IngestReportBody, from: IngestReportBody) {
    into.files_scanned += from.files_scanned;
    into.files_unchanged += from.files_unchanged;
    into.files_stored += from.files_stored;
    into.files_already_present += from.files_already_present;
    into.chd_v5 += from.chd_v5;
    into.members_claimed += from.members_claimed;
    into.members_extracted += from.members_extracted;
    into.detector_hits += from.detector_hits;
    into.skipper_skipped_large += from.skipper_skipped_large;
    into.dats_imported.extend(from.dats_imported);
    into.errors.extend(from.errors);
    into.member_skips.extend(from.member_skips);
    into.notes.extend(from.notes);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn staged(name: &str, bytes: u64) -> StagedUpload {
        StagedUpload {
            path: PathBuf::from(format!("/store/tmp/x-{name}.temp")),
            name: name.to_owned(),
            bytes,
        }
    }

    /// D74 round-trip: finished jobs survive re-opening the ledger
    /// (report intact, db-assigned ids), and a row still running at
    /// the next open is marked interrupted — crash evidence, not
    /// amnesia.
    #[test]
    fn ledger_survives_restart_and_marks_interrupted() {
        let dir = tempfile::tempdir().expect("tempdir");
        let open_db = || datboi_index::Db::open(dir.path()).expect("db");

        let first = Registry::durable(open_db(), 1_000).expect("open");
        let done_id = first.create(&[staged("kept.gba", 7)], 1_000);
        first.file_done(
            done_id,
            7,
            IngestReportBody {
                files_stored: 1,
                ..IngestReportBody::default()
            },
        );
        first.finish(done_id, 1_500);
        let crashed_id = first.create_gc("evict — watermark", 3, 1_600);
        assert_ne!(done_id, crashed_id);
        drop(first); // daemon dies with the gc job still running

        let second = Registry::durable(open_db(), 2_000).expect("reopen");
        let detail = second.detail(done_id).expect("history survived");
        assert_eq!(detail.job.state, JobRunState::Done);
        assert_eq!(detail.report.files_stored, 1, "frozen report hydrated");
        assert_eq!(*detail.job.finished_at, Some(1_500));
        let crashed = second.detail(crashed_id).expect("tombstone exists");
        assert_eq!(crashed.job.state, JobRunState::Failed);
        assert!(
            crashed
                .error
                .as_deref()
                .unwrap_or("")
                .contains("interrupted"),
            "{crashed:?}"
        );
        // Fresh ids continue past history — never collide with it.
        let next = second.create(&[staged("new.gba", 1)], 2_100);
        assert!(next > crashed_id);
    }

    /// The poll-time merge (D74 CLI wiring): a terminal row written by
    /// another process (the CLI's shape) appears in list() and serves
    /// detail() WITHOUT a daemon restart; memory wins on id collision.
    #[test]
    fn ledger_merge_surfaces_cli_rows_live() {
        let dir = tempfile::tempdir().expect("tempdir");
        let reg = Registry::durable(datboi_index::Db::open(dir.path()).expect("db"), 1_000)
            .expect("open");
        let mem_id = reg.create(&[staged("live.gba", 1)], 1_100);

        // "The CLI": a second connection writes a finished scrub row.
        let cli_db = datboi_index::Db::open(dir.path()).expect("second conn");
        let cli_id = cli_db
            .insert_finished_job(
                LedgerKind::Scrub,
                "cli: scrub — 100% sample",
                LedgerState::Done,
                1_200,
                1_300,
            )
            .expect("cli row");

        let rows = reg.list();
        let cli_row = rows
            .iter()
            .find(|j| j.id == cli_id)
            .expect("cli row surfaced");
        assert_eq!(cli_row.kind, JobKind::Scrub);
        assert_eq!(cli_row.state, JobRunState::Done);
        assert!(rows.iter().any(|j| j.id == mem_id), "live job still listed");
        assert!(rows[0].id > rows[1].id, "newest first");
        let detail = reg.detail(cli_id).expect("detail from ledger fallback");
        assert_eq!(detail.job.name, "cli: scrub — 100% sample");
        assert_eq!(*detail.job.finished_at, Some(1_300));
    }

    #[test]
    fn tokens_spend_all_or_nothing() {
        let reg = Registry::ephemeral();
        reg.stage("a".into(), staged("one.gba", 1));
        reg.stage("b".into(), staged("two.gba", 2));
        // Unknown token: nothing consumed, offender named.
        let err = reg
            .take(&["a".into(), "ghost".into()])
            .expect_err("unknown token");
        assert_eq!(err, "ghost");
        // The earlier attempt spent nothing.
        let got = reg.take(&["a".into(), "b".into()]).expect("both live");
        assert_eq!(got.len(), 2);
        // Spent means spent.
        assert_eq!(reg.take(&["a".into()]).expect_err("gone"), "a");
    }

    #[test]
    fn progress_is_byte_weighted_and_caps_at_99_while_running() {
        let reg = Registry::ephemeral();
        let files = [staged("big.zip", 900), staged("small.gba", 100)];
        let id = reg.create(&files, 1_000);
        assert_eq!(reg.list()[0].progress, 0);
        reg.file_done(id, 900, IngestReportBody::default());
        assert_eq!(reg.list()[0].progress, 90);
        // All bytes done but not finished: pipeline tail (rollups) is
        // still running — 99, not a lying 100.
        reg.file_done(id, 100, IngestReportBody::default());
        assert_eq!(reg.list()[0].progress, 99);
        reg.set_matched(
            id,
            vec![MatchedEntry {
                name: "Mario Kart DS (USA)".into(),
                source: "no-intro/nds".into(),
            }],
            201,
        );
        reg.finish(id, 2_000);
        let row = &reg.list()[0];
        assert_eq!((row.progress, row.state), (100, JobRunState::Done));
        let detail = reg.detail(id).expect("detail");
        assert_eq!(*detail.job.finished_at, Some(2_000));
        // Matched rides the detail; the total may exceed the capped list.
        assert_eq!(detail.matched[0].name, "Mario Kart DS (USA)");
        assert_eq!(detail.matched_total, 201);
        // Zero-byte totals must not divide by zero.
        let id = reg.create(&[staged("empty.txt", 0)], 3_000);
        assert_eq!(reg.detail(id).expect("detail").job.progress, 0);
    }

    #[test]
    fn history_keeps_running_jobs_and_a_bounded_finished_tail() {
        let reg = Registry::ephemeral();
        let runner = reg.create(&[staged("slow.zip", 1)], 0);
        let mut finished = Vec::new();
        for i in 0..KEEP_FINISHED + 5 {
            let id = reg.create(&[staged(&format!("f{i}"), 1)], i as i64);
            reg.finish(id, i as i64);
            finished.push(id);
        }
        let rows = reg.list();
        assert_eq!(rows.len(), KEEP_FINISHED + 1, "runner + kept tail");
        assert!(rows.iter().any(|j| j.id == runner), "running never pruned");
        assert!(
            reg.detail(finished[0]).is_none(),
            "oldest finished job pruned"
        );
        assert!(
            reg.detail(*finished.last().expect("some")).is_some(),
            "newest finished job kept"
        );
    }

    #[test]
    fn translation_swaps_staging_paths_for_client_names() {
        let s = staged("roms/pack.zip", 10);
        let report = IngestReport {
            files_scanned: 1,
            errors: vec![(s.path.clone(), "boom".into())],
            member_skips: vec![(s.path.clone(), "inner.bin".into(), "zip64".into())],
            ..Default::default()
        };
        let body = translate(report, &s);
        assert_eq!(body.errors[0].path, "roms/pack.zip");
        assert_eq!(body.member_skips[0].path, "roms/pack.zip");
        assert_eq!(body.member_skips[0].member, "inner.bin");
    }
}
