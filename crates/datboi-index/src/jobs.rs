//! The durable job ledger (D74): terminal snapshots of background
//! work, surviving daemon restarts. The in-memory registry
//! (datboi-server jobs.rs) remains the LIVE surface — rows here are
//! inserted at job creation (state `running`), finalized exactly once
//! at the end, and a row still `running` at daemon startup is crash
//! evidence (the interruption sweep marks it so the tray can say
//! "died with the process" instead of forgetting it existed).
//!
//! state.db by the session precedent: authoritative but truncatable,
//! excluded from CAS snapshots — history is worth surviving a
//! restart, not worth carrying in the recovery root.
//!
//! Kind and state vocabularies are [`JobKind`]/[`JobState`]
//! (types.rs); rows decode fallibly like every other coded column, so
//! a code this build doesn't know surfaces as [`IndexError::Decode`],
//! never a silently misfiled row.

use rusqlite::params;

use crate::types::{JobKind, JobState};
use crate::{Db, IndexError};

/// Finished rows kept in the ledger — deeper than any tray so a
/// future activity feed has something to read. Molten. Every writer
/// prunes to this after inserting.
pub const LEDGER_KEEP: usize = 500;

/// One ledger row, as hydration and history read it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobRow {
    pub job_id: i64,
    pub kind: JobKind,
    pub name: String,
    pub state: JobState,
    pub started_at: i64,
    pub finished_at: Option<i64>,
    /// Wire `JobDetail` JSON frozen at finalize; absent for
    /// interrupted rows (they never finalized) and unparseable for
    /// rows written by a future shape — callers render a stub from
    /// the columns in both cases.
    pub detail: Option<Vec<u8>>,
}

/// The raw column tuple a job SELECT yields; decoded outside the
/// rusqlite closure so coded columns fail as [`IndexError::Decode`].
type RawJobRow = (i64, i64, String, i64, i64, Option<i64>, Option<Vec<u8>>);

fn read_raw(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawJobRow> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
    ))
}

fn decode(raw: RawJobRow) -> Result<JobRow, IndexError> {
    let (job_id, kind, name, state, started_at, finished_at, detail) = raw;
    Ok(JobRow {
        job_id,
        kind: JobKind::from_code(kind)?,
        name,
        state: JobState::from_code(state)?,
        started_at,
        finished_at,
        detail,
    })
}

impl Db {
    /// Open a job's ledger row (state `running`); answers the id that
    /// IS the job id everywhere — db-assigned, so ids stay unique
    /// across daemon restarts (an in-memory counter would collide with
    /// history).
    pub fn insert_job(
        &self,
        kind: JobKind,
        name: &str,
        started_at: i64,
    ) -> Result<i64, IndexError> {
        self.state().execute(
            "INSERT INTO job (kind, name, state, started_at) VALUES (?1, ?2, ?3, ?4)",
            params![kind.code(), name, JobState::Running.code(), started_at],
        )?;
        Ok(self.state().last_insert_rowid())
    }

    /// Finalize exactly once: terminal state + the frozen detail JSON.
    pub fn finalize_job(
        &self,
        job_id: i64,
        state: JobState,
        finished_at: i64,
        detail: Option<&[u8]>,
    ) -> Result<(), IndexError> {
        self.state().execute(
            "UPDATE job SET state = ?2, finished_at = ?3, detail = ?4
             WHERE job_id = ?1 AND state = ?5",
            params![
                job_id,
                state.code(),
                finished_at,
                detail,
                JobState::Running.code()
            ],
        )?;
        Ok(())
    }

    /// One-shot TERMINAL row — the CLI's write shape (D74): a CLI job
    /// never records `running`, because the daemon's interruption
    /// sweep would falsely tombstone a row whose process is alive in
    /// another terminal. Crash evidence for CLI work is worthless
    /// anyway — the human was watching it die.
    pub fn insert_finished_job(
        &self,
        kind: JobKind,
        name: &str,
        state: JobState,
        started_at: i64,
        finished_at: i64,
    ) -> Result<i64, IndexError> {
        self.state().execute(
            "INSERT INTO job (kind, name, state, started_at, finished_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![kind.code(), name, state.code(), started_at, finished_at],
        )?;
        Ok(self.state().last_insert_rowid())
    }

    /// One ledger row by id (the tray's detail fallback for rows not
    /// hydrated into memory — CLI-written history).
    pub fn job_by_id(&self, job_id: i64) -> Result<Option<JobRow>, IndexError> {
        use rusqlite::OptionalExtension as _;
        self.state()
            .query_row(
                "SELECT job_id, kind, name, state, started_at, finished_at, detail
                 FROM job WHERE job_id = ?1",
                [job_id],
                read_raw,
            )
            .optional()?
            .map(decode)
            .transpose()
    }

    /// Newest FINISHED row of one kind — the last-scrub readout
    /// (/v1/storage) and any future per-kind "last run" surface.
    pub fn latest_finished_job_of_kind(&self, kind: JobKind) -> Result<Option<JobRow>, IndexError> {
        use rusqlite::OptionalExtension as _;
        self.state()
            .query_row(
                "SELECT job_id, kind, name, state, started_at, finished_at, detail
                 FROM job WHERE kind = ?1 AND state != ?2
                 ORDER BY job_id DESC LIMIT 1",
                params![kind.code(), JobState::Running.code()],
                read_raw,
            )
            .optional()?
            .map(decode)
            .transpose()
    }

    /// The startup sweep: any row still `running` belonged to a dead
    /// process (one daemon per db-dir). Returns rows marked.
    pub fn interrupt_running_jobs(&self, now_unix: i64) -> Result<usize, IndexError> {
        Ok(self.state().execute(
            "UPDATE job SET state = ?1, finished_at = ?2 WHERE state = ?3",
            params![
                JobState::Interrupted.code(),
                now_unix,
                JobState::Running.code()
            ],
        )?)
    }

    /// Newest-last recent history (the hydration read).
    pub fn recent_jobs(&self, limit: usize) -> Result<Vec<JobRow>, IndexError> {
        let mut stmt = self.state().prepare_cached(
            "SELECT job_id, kind, name, state, started_at, finished_at, detail
             FROM (SELECT * FROM job ORDER BY job_id DESC LIMIT ?1)
             ORDER BY job_id",
        )?;
        let raw = stmt
            .query_map([i64::try_from(limit).unwrap_or(i64::MAX)], read_raw)?
            .collect::<Result<Vec<_>, _>>()?;
        raw.into_iter().map(decode).collect()
    }

    /// Bound the ledger: keep the newest `keep` FINISHED rows (running
    /// rows are never pruned, however old).
    pub fn prune_jobs(&self, keep: usize) -> Result<usize, IndexError> {
        Ok(self.state().execute(
            "DELETE FROM job WHERE state != ?1 AND job_id NOT IN (
               SELECT job_id FROM job WHERE state != ?1
               ORDER BY job_id DESC LIMIT ?2)",
            params![
                JobState::Running.code(),
                i64::try_from(keep).unwrap_or(i64::MAX)
            ],
        )?)
    }
}
