//! The in-memory job registry + staged-upload table (web ingest).
//!
//! This is the minimal in-daemon jobs surface the M5 scope ruling
//! deferred to (docs/open-questions.md § "Jobs tray backend"): enough
//! state for `/v1/jobs` to render truthfully while an ingest runs.
//! Deliberately NOT durable — a daemon restart forgets tokens and job
//! history (staged bytes are swept with the store's tmp/); a real job
//! table with durable reports remains the recorded open question.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::Mutex;

use datboi_api::{
    IngestErrorItem, IngestMemberSkipItem, IngestReportBody, Job, JobDetail, JobKind, JobRunState,
};
use datboi_ingest::IngestReport;

/// Finished jobs kept for the tray/history after they complete;
/// running jobs are never pruned.
const KEEP_FINISHED: usize = 20;

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
    files_total: u64,
    files_done: u64,
    bytes_total: u64,
    bytes_done: u64,
    current: Option<String>,
    state: JobRunState,
    report: IngestReportBody,
    error: Option<String>,
    started_at: i64,
    finished_at: Option<i64>,
}

impl JobState {
    /// Byte-weighted at file granularity (the pipeline reports no
    /// intra-file progress), capped at 99 while running so only a
    /// finished job reads 100.
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
            name: format!(
                "ingest — {} file{}",
                self.files_total,
                if self.files_total == 1 { "" } else { "s" }
            ),
            progress: self.progress(),
            kind: JobKind::Ingest,
            state: self.state,
        }
    }
}

pub(crate) struct Registry {
    uploads: Mutex<HashMap<String, StagedUpload>>,
    jobs: Mutex<JobsInner>,
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
    pub(crate) fn new() -> Self {
        Self {
            uploads: Mutex::new(HashMap::new()),
            jobs: Mutex::new(JobsInner {
                next_id: 1,
                jobs: VecDeque::new(),
            }),
        }
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

    /// Create a running job over the staged set; answers the id.
    pub(crate) fn create(&self, files: &[StagedUpload], now: i64) -> i64 {
        let mut inner = lock(&self.jobs);
        let id = inner.next_id;
        inner.next_id += 1;
        inner.jobs.push_back(JobState {
            id,
            files_total: files.len() as u64,
            files_done: 0,
            bytes_total: files.iter().map(|f| f.bytes).sum(),
            bytes_done: 0,
            current: None,
            state: JobRunState::Running,
            report: IngestReportBody::default(),
            error: None,
            started_at: now,
            finished_at: None,
        });
        id
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

    pub(crate) fn finish(&self, id: i64, now: i64) {
        let mut inner = lock(&self.jobs);
        if let Some(j) = inner.jobs.iter_mut().find(|j| j.id == id) {
            j.state = JobRunState::Done;
            j.current = None;
            j.finished_at = Some(now);
        }
        Self::prune(&mut inner);
    }

    /// Infrastructure failure (not a per-file refusal) — kept for
    /// honesty even though the ingest loop itself never throws.
    #[allow(dead_code)]
    pub(crate) fn fail(&self, id: i64, error: &str, now: i64) {
        let mut inner = lock(&self.jobs);
        if let Some(j) = inner.jobs.iter_mut().find(|j| j.id == id) {
            j.state = JobRunState::Failed;
            j.current = None;
            j.error = Some(error.to_owned());
            j.finished_at = Some(now);
        }
        Self::prune(&mut inner);
    }

    /// Tray rows, newest first.
    pub(crate) fn list(&self) -> Vec<Job> {
        lock(&self.jobs)
            .jobs
            .iter()
            .rev()
            .map(JobState::row)
            .collect()
    }

    pub(crate) fn detail(&self, id: i64) -> Option<JobDetail> {
        let inner = lock(&self.jobs);
        let j = inner.jobs.iter().find(|j| j.id == id)?;
        Some(JobDetail {
            job: j.row(),
            files_total: j.files_total,
            files_done: j.files_done,
            bytes_total: j.bytes_total,
            bytes_done: j.bytes_done,
            current: j.current.clone(),
            started_at: j.started_at,
            finished_at: j.finished_at,
            report: j.report.clone(),
            error: j.error.clone(),
        })
    }
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

    #[test]
    fn tokens_spend_all_or_nothing() {
        let reg = Registry::new();
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
        let reg = Registry::new();
        let files = [staged("big.zip", 900), staged("small.gba", 100)];
        let id = reg.create(&files, 1_000);
        assert_eq!(reg.list()[0].progress, 0);
        reg.file_done(id, 900, IngestReportBody::default());
        assert_eq!(reg.list()[0].progress, 90);
        // All bytes done but not finished: pipeline tail (rollups) is
        // still running — 99, not a lying 100.
        reg.file_done(id, 100, IngestReportBody::default());
        assert_eq!(reg.list()[0].progress, 99);
        reg.finish(id, 2_000);
        let row = &reg.list()[0];
        assert_eq!((row.progress, row.state), (100, JobRunState::Done));
        let detail = reg.detail(id).expect("detail");
        assert_eq!(detail.finished_at, Some(2_000));
        // Zero-byte totals must not divide by zero.
        let id = reg.create(&[staged("empty.txt", 0)], 3_000);
        assert_eq!(reg.detail(id).expect("detail").job.progress, 0);
    }

    #[test]
    fn history_keeps_running_jobs_and_a_bounded_finished_tail() {
        let reg = Registry::new();
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
