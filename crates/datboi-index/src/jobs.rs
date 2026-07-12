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

use rusqlite::params;

use crate::{Db, IndexError};

/// Job state codes (the wire vocabulary plus crash evidence).
pub const JOB_RUNNING: i64 = 0;
pub const JOB_DONE: i64 = 1;
pub const JOB_FAILED: i64 = 2;
/// Still `running` when a daemon started: the process died under it.
pub const JOB_INTERRUPTED: i64 = 3;

/// Job kind codes — ONE definition for both writers (the daemon's
/// registry maps the wire enum here; the CLI's ledger_stamp match in
/// datboi's main.rs names these directly).
pub const KIND_INGEST: i64 = 0;
pub const KIND_REFINE: i64 = 1;
pub const KIND_GC: i64 = 2;
pub const KIND_SCRUB: i64 = 3;

/// Finished rows kept in the ledger — deeper than any tray so a
/// future activity feed has something to read. Molten. Every writer
/// prunes to this after inserting.
pub const LEDGER_KEEP: usize = 500;

/// One ledger row, as hydration and history read it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobRow {
    pub job_id: i64,
    pub kind: i64,
    pub name: String,
    pub state: i64,
    pub started_at: i64,
    pub finished_at: Option<i64>,
    /// Wire `JobDetail` JSON frozen at finalize; absent for
    /// interrupted rows (they never finalized) and unparseable for
    /// rows written by a future shape — callers render a stub from
    /// the columns in both cases.
    pub detail: Option<Vec<u8>>,
}

impl Db {
    /// Open a job's ledger row (state `running`); answers the id that
    /// IS the job id everywhere — db-assigned, so ids stay unique
    /// across daemon restarts (an in-memory counter would collide with
    /// history).
    pub fn insert_job(
        &self,
        kind: i64,
        name: &str,
        started_at: i64,
    ) -> Result<i64, IndexError> {
        self.state().execute(
            "INSERT INTO job (kind, name, state, started_at) VALUES (?1, ?2, ?3, ?4)",
            params![kind, name, JOB_RUNNING, started_at],
        )?;
        Ok(self.state().last_insert_rowid())
    }

    /// Finalize exactly once: terminal state + the frozen detail JSON.
    pub fn finalize_job(
        &self,
        job_id: i64,
        state: i64,
        finished_at: i64,
        detail: Option<&[u8]>,
    ) -> Result<(), IndexError> {
        self.state().execute(
            "UPDATE job SET state = ?2, finished_at = ?3, detail = ?4
             WHERE job_id = ?1 AND state = ?5",
            params![job_id, state, finished_at, detail, JOB_RUNNING],
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
        kind: i64,
        name: &str,
        state: i64,
        started_at: i64,
        finished_at: i64,
    ) -> Result<i64, IndexError> {
        self.state().execute(
            "INSERT INTO job (kind, name, state, started_at, finished_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![kind, name, state, started_at, finished_at],
        )?;
        Ok(self.state().last_insert_rowid())
    }

    /// One ledger row by id (the tray's detail fallback for rows not
    /// hydrated into memory — CLI-written history).
    pub fn job_by_id(&self, job_id: i64) -> Result<Option<JobRow>, IndexError> {
        use rusqlite::OptionalExtension as _;
        Ok(self
            .state()
            .query_row(
                "SELECT job_id, kind, name, state, started_at, finished_at, detail
                 FROM job WHERE job_id = ?1",
                [job_id],
                |row| {
                    Ok(JobRow {
                        job_id: row.get(0)?,
                        kind: row.get(1)?,
                        name: row.get(2)?,
                        state: row.get(3)?,
                        started_at: row.get(4)?,
                        finished_at: row.get(5)?,
                        detail: row.get(6)?,
                    })
                },
            )
            .optional()?)
    }

    /// The startup sweep: any row still `running` belonged to a dead
    /// process (one daemon per db-dir). Returns rows marked.
    pub fn interrupt_running_jobs(&self, now_unix: i64) -> Result<usize, IndexError> {
        Ok(self.state().execute(
            "UPDATE job SET state = ?1, finished_at = ?2 WHERE state = ?3",
            params![JOB_INTERRUPTED, now_unix, JOB_RUNNING],
        )?)
    }

    /// Newest-last recent history (the hydration read).
    pub fn recent_jobs(&self, limit: usize) -> Result<Vec<JobRow>, IndexError> {
        let mut stmt = self.state().prepare_cached(
            "SELECT job_id, kind, name, state, started_at, finished_at, detail
             FROM (SELECT * FROM job ORDER BY job_id DESC LIMIT ?1)
             ORDER BY job_id",
        )?;
        let rows = stmt
            .query_map([i64::try_from(limit).unwrap_or(i64::MAX)], |row| {
                Ok(JobRow {
                    job_id: row.get(0)?,
                    kind: row.get(1)?,
                    name: row.get(2)?,
                    state: row.get(3)?,
                    started_at: row.get(4)?,
                    finished_at: row.get(5)?,
                    detail: row.get(6)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Bound the ledger: keep the newest `keep` FINISHED rows (running
    /// rows are never pruned, however old).
    pub fn prune_jobs(&self, keep: usize) -> Result<usize, IndexError> {
        Ok(self.state().execute(
            "DELETE FROM job WHERE state != ?1 AND job_id NOT IN (
               SELECT job_id FROM job WHERE state != ?1
               ORDER BY job_id DESC LIMIT ?2)",
            params![JOB_RUNNING, i64::try_from(keep).unwrap_or(i64::MAX)],
        )?)
    }
}
