//! Analyzer provenance and the refinement sweep queue (D45/D47/D48).
//!
//! Provenance rows are pure functions of bytes × analyzer identity —
//! cache-grade (D37), batched into signed snapshots so bare-NAS recovery
//! doesn't re-pay expensive negatives. The sweep queue is scheduling
//! state only: candidate selection is dat-blind (every data blob is a
//! candidate for every analyzer — D47's hard rule), while *ordering* may
//! consult the catalog (`bump_dat_matched_priorities`).

use datboi_core::hash::Blake3;
use datboi_core::snapshot::AnalysisRow;
use rusqlite::{OptionalExtension, params};

use crate::{Db, IndexError};

/// One analyzer's conclusion about one blob's bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnalysisOutcome {
    /// Analyzed; nothing was discovered. Recording this is the point:
    /// sweeps never silently retry known negatives (D45/D24).
    Negative,
    /// Discovery: recipes or claims were minted (they live as ordinary
    /// CAS objects; this row is the provenance that says why).
    Positive,
}

impl AnalysisOutcome {
    fn code(self) -> i64 {
        match self {
            Self::Negative => 0,
            Self::Positive => 1,
        }
    }

    fn from_code(code: i64) -> Result<Self, IndexError> {
        match code {
            0 => Ok(Self::Negative),
            1 => Ok(Self::Positive),
            _ => Err(IndexError::Decode {
                what: "AnalysisOutcome",
                code,
            }),
        }
    }
}

/// A pending sweep queue item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SweepItem {
    pub blob_id: i64,
    pub hash: Blake3,
    pub size: Option<u64>,
    pub priority: i64,
}

// Sweep-queue priority TIERS (D47: ordering may be anything; membership
// stays dat-blind). Higher runs first.
/// The corpus backlog — everything ends up here eventually.
pub const PRIORITY_AMBIENT: i64 = 0;
/// Queued blobs whose identity matches a catalog claim
/// ([`Db::bump_dat_matched_priorities`]).
pub const PRIORITY_DAT_MATCHED: i64 = 1;
/// Just-ingested content ([`Db::enqueue_fresh`]): the narrow slice the
/// user is looking at right now refines before either backlog tier.
pub const PRIORITY_FRESH: i64 = 2;

/// D92 eagerness policy for absent-blob analysis — MOLTEN (state.db KV
/// `refine:absent:mode`, riding snapshots like every config row). The
/// posture (analyzers consume the logical CAS) is ruled; how eagerly
/// the claim gate admits non-resident items is policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AbsentMode {
    /// Never admit absent blobs (the pre-D92 posture, kept reachable).
    Off,
    /// Grounded absents whose identity a dat names (the default):
    /// bounds the spill bill to bytes someone actually cares about,
    /// while the wild-member flood stays queued-but-unclaimed.
    DatNamed,
    /// Every grounded absent.
    All,
}

impl Db {
    /// Record what `analyzer` concluded about `blob_id`'s bytes. Replaces
    /// any prior row for the pair (a re-run under the same analyzer
    /// identity is the same pure function — results can't differ, but
    /// crash-replay may legitimately rewrite).
    pub fn record_analysis(
        &self,
        blob_id: i64,
        analyzer: &Blake3,
        outcome: AnalysisOutcome,
        detail: Option<&str>,
        at_unix: i64,
    ) -> Result<(), IndexError> {
        self.cache().execute(
            "INSERT INTO analysis (blob_id, analyzer, outcome, detail, analyzed_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(blob_id, analyzer) DO UPDATE SET
               outcome = excluded.outcome,
               detail = excluded.detail,
               analyzed_at = excluded.analyzed_at",
            params![
                blob_id,
                analyzer.0.as_slice(),
                outcome.code(),
                detail,
                at_unix
            ],
        )?;
        Ok(())
    }

    /// What `analyzer` concluded about `blob_id`, if it ever ran.
    pub fn analysis_outcome(
        &self,
        blob_id: i64,
        analyzer: &Blake3,
    ) -> Result<Option<AnalysisOutcome>, IndexError> {
        let code = self
            .cache()
            .query_row(
                "SELECT outcome FROM analysis WHERE blob_id = ?1 AND analyzer = ?2",
                params![blob_id, analyzer.0.as_slice()],
                |row| row.get::<_, i64>(0),
            )
            .optional()?;
        code.map(AnalysisOutcome::from_code).transpose()
    }

    /// Every provenance row joined to its blob hash, ordered by
    /// (blob hash, analyzer) — the snapshot batch source (D48). Data
    /// namespace only, mirroring the alias-batch rule: meta objects are
    /// never analysis subjects.
    pub fn list_analysis_rows(&self) -> Result<Vec<AnalysisRow>, IndexError> {
        let mut stmt = self.cache().prepare_cached(
            "SELECT b.hash, a.analyzer, a.outcome, a.detail
             FROM analysis a JOIN blob b ON b.blob_id = a.blob_id
             WHERE b.namespace = 0
             ORDER BY b.hash, a.analyzer",
        )?;
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, [u8; 32]>(0)?,
                    row.get::<_, [u8; 32]>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, Option<String>>(3)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        rows.into_iter()
            .map(|(blob, analyzer, outcome, detail)| {
                Ok(AnalysisRow {
                    blob: Blake3(blob),
                    analyzer: Blake3(analyzer),
                    positive: AnalysisOutcome::from_code(outcome)? == AnalysisOutcome::Positive,
                    detail,
                })
            })
            .collect()
    }

    /// Restore one provenance row from a snapshot batch during recovery
    /// (D48: recovery must not re-pay analysis). Unknown blobs are
    /// skipped — a batch can reference bytes this store no longer holds.
    pub fn restore_analysis_row(
        &self,
        row: &AnalysisRow,
        at_unix: i64,
    ) -> Result<bool, IndexError> {
        let Some(blob_id) = self.get_blob_id(&row.blob)? else {
            return Ok(false);
        };
        self.record_analysis(
            blob_id,
            &row.analyzer,
            if row.positive {
                AnalysisOutcome::Positive
            } else {
                AnalysisOutcome::Negative
            },
            row.detail.as_deref(),
            at_unix,
        )?;
        Ok(true)
    }

    /// Enqueue every data blob that `analyzer` has not yet analyzed and
    /// that isn't already queued. Candidate selection is DAT-BLIND (D47):
    /// what gets analyzed is a function of the bytes we hold, never of
    /// which dats are loaded. Returns how many rows were enqueued.
    pub fn enqueue_unanalyzed(&self, analyzer: &Blake3, at_unix: i64) -> Result<usize, IndexError> {
        let n = self.cache().execute(
            "INSERT OR IGNORE INTO sweep_queue (blob_id, analyzer, priority, enqueued_at)
             SELECT b.blob_id, ?1, 0, ?2 FROM blob b
             WHERE b.namespace = 0
               AND NOT EXISTS (
                 SELECT 1 FROM analysis a
                 WHERE a.blob_id = b.blob_id AND a.analyzer = ?1)",
            params![analyzer.0.as_slice(), at_unix],
        )?;
        Ok(n)
    }

    /// Fresh-content ORDERING (D47/D71): just-ingested blobs jump to the
    /// [`PRIORITY_FRESH`] tier for `analyzer`, so the slice the user is
    /// looking at refines before the corpus backlog. Membership stays
    /// dat-blind — the same `NOT EXISTS(analysis)` guard as
    /// [`Db::enqueue_unanalyzed`], so re-ingesting known content never
    /// requeues a settled conclusion. Returns rows inserted or promoted.
    pub fn enqueue_fresh(
        &mut self,
        analyzer: &Blake3,
        blob_ids: &[i64],
        at_unix: i64,
    ) -> Result<usize, IndexError> {
        let tx = self.cache.transaction()?;
        let mut touched = 0;
        {
            let mut insert = tx.prepare_cached(
                "INSERT OR IGNORE INTO sweep_queue
                   (blob_id, analyzer, priority, enqueued_at)
                 SELECT ?1, ?2, ?3, ?4
                 WHERE NOT EXISTS (
                   SELECT 1 FROM analysis a
                   WHERE a.blob_id = ?1 AND a.analyzer = ?2)",
            )?;
            let mut promote = tx.prepare_cached(
                "UPDATE sweep_queue SET priority = ?3
                 WHERE blob_id = ?1 AND analyzer = ?2 AND priority < ?3",
            )?;
            for &blob_id in blob_ids {
                let inserted = insert.execute(params![
                    blob_id,
                    analyzer.0.as_slice(),
                    PRIORITY_FRESH,
                    at_unix
                ])?;
                touched += if inserted == 0 {
                    promote.execute(params![blob_id, analyzer.0.as_slice(), PRIORITY_FRESH])?
                } else {
                    inserted
                };
            }
        }
        tx.commit()?;
        Ok(touched)
    }

    /// Dat-aware ORDERING (allowed by D47): queued blobs whose identity
    /// matches any catalog claim jump ahead of unmatched junk. Ordering
    /// only — the queue's membership is untouched, and the fresh tier
    /// is never demoted (`priority <` guard).
    pub fn bump_dat_matched_priorities(&self) -> Result<usize, IndexError> {
        let n = self.cache().execute(
            "UPDATE sweep_queue SET priority = ?1
             WHERE priority < ?1 AND blob_id IN (
               SELECT ib.blob_id
               FROM identity_blob ib
               JOIN rom_claim rc ON rc.identity_id = ib.identity_id)",
            [PRIORITY_DAT_MATCHED],
        )?;
        Ok(n)
    }

    /// Claim the next `limit` queue items for `analyzer`, highest
    /// priority first, leasing each until `now_unix + lease_secs` in the
    /// same transaction — a claimed item is invisible to every other
    /// worker (the daemon's refine thread, a concurrent CLI sweep) until
    /// the lease expires, so expensive analyses never run twice at once.
    /// The lease is dedup, not a correctness gate: a crashed holder's
    /// items reappear on expiry and the analyzer re-runs a pure function.
    ///
    /// Non-resident blobs are picked only when the D92 admission table
    /// ([`Db::refresh_absent_eligibility`]) says their bytes are
    /// obtainable — grounded through trusted claims, within the molten
    /// eagerness policy. Ungrounded claims (peer-advertised hashes,
    /// members of refused containers) stay QUEUED but unclaimed:
    /// erroring on each every sweep is noise, and a later grounding or
    /// rematerialization admits them automatically.
    pub fn claim_sweep_items(
        &mut self,
        analyzer: &Blake3,
        limit: usize,
        now_unix: i64,
        lease_secs: i64,
    ) -> Result<Vec<SweepItem>, IndexError> {
        // IMMEDIATE, not deferred (D93): this transaction reads then
        // writes, and a deferred read→write upgrade against a
        // concurrent claimant returns SQLITE_BUSY without consulting
        // the busy handler (waiting can't refresh a stale snapshot).
        // Taking the write lock up front makes concurrent claimants
        // queue politely instead — the whole multi-worker design
        // hangs on this one statement ordering.
        let tx = self
            .cache
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let rows = {
            // Predicate + ORDER BY mirror sweep_by_priority (analyzer,
            // priority DESC, enqueued_at, blob_id): the claim is an
            // ordered index walk that stops at LIMIT, not a sort of the
            // whole blobs × analyzers queue. Keep them in lockstep.
            let mut stmt = tx.prepare_cached(
                "SELECT q.blob_id, b.hash, b.size, q.priority
                 FROM sweep_queue q JOIN blob b ON b.blob_id = q.blob_id
                 WHERE q.analyzer = ?1
                   AND (b.residency = 0 OR EXISTS (
                     SELECT 1 FROM sweep_absent_eligible e
                     WHERE e.blob_id = b.blob_id))
                   AND q.leased_until <= ?2
                 ORDER BY q.priority DESC, q.enqueued_at, q.blob_id
                 LIMIT ?3",
            )?;
            stmt.query_map(
                params![
                    analyzer.0.as_slice(),
                    now_unix,
                    i64::try_from(limit).unwrap_or(i64::MAX)
                ],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, [u8; 32]>(1)?,
                        row.get::<_, Option<i64>>(2)?,
                        row.get::<_, i64>(3)?,
                    ))
                },
            )?
            .collect::<Result<Vec<_>, _>>()?
        };
        {
            let mut lease = tx.prepare_cached(
                "UPDATE sweep_queue SET leased_until = ?3
                 WHERE blob_id = ?1 AND analyzer = ?2",
            )?;
            for (blob_id, _, _, _) in &rows {
                lease.execute(params![
                    blob_id,
                    analyzer.0.as_slice(),
                    now_unix.saturating_add(lease_secs)
                ])?;
            }
        }
        tx.commit()?;
        Ok(rows
            .into_iter()
            .map(|(blob_id, hash, size, priority)| SweepItem {
                blob_id,
                hash: Blake3(hash),
                size: size.map(|s| u64::try_from(s).expect("sizes stored non-negative")),
                priority,
            })
            .collect())
    }

    /// Return one item to the pool early — an analyzer ERROR (transient
    /// environment failure, not a conclusion) should not sideline the
    /// item for the rest of its lease.
    pub fn release_sweep_lease(&self, blob_id: i64, analyzer: &Blake3) -> Result<(), IndexError> {
        self.cache().execute(
            "UPDATE sweep_queue SET leased_until = 0
             WHERE blob_id = ?1 AND analyzer = ?2",
            params![blob_id, analyzer.0.as_slice()],
        )?;
        Ok(())
    }

    /// A renewal handle over its OWN connection to cache.db, so a
    /// long-running analyzer can re-stamp its lease from inside
    /// `analyze` while the main connection is mutably borrowed
    /// (progress-gated heartbeat, D71). WAL + busy_timeout arbitrate,
    /// same as every other second connection.
    pub fn lease_keeper(&self) -> Result<SweepLeaseKeeper, IndexError> {
        let conn = rusqlite::Connection::open(self.cache_path())?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        Ok(SweepLeaseKeeper { conn })
    }

    /// Clear every lease. The daemon's refine worker runs this at
    /// startup: any lease in this db-dir belongs to a dead process (one
    /// daemon per db-dir), and waiting out stale leases would stall the
    /// fresh-ingest path for no reason. A CLI sweep racing this loses
    /// only dedup, never correctness.
    pub fn clear_sweep_leases(&self) -> Result<usize, IndexError> {
        Ok(self.cache().execute(
            "UPDATE sweep_queue SET leased_until = 0 WHERE leased_until != 0",
            [],
        )?)
    }

    /// Finish one sweep item: provenance row written and queue row removed
    /// in one transaction, so a crash re-runs the item (idempotent — the
    /// analyzer is a pure function) rather than losing it.
    pub fn complete_sweep_item(
        &mut self,
        blob_id: i64,
        analyzer: &Blake3,
        outcome: AnalysisOutcome,
        detail: Option<&str>,
        at_unix: i64,
    ) -> Result<(), IndexError> {
        let tx = self.cache.transaction()?;
        tx.execute(
            "INSERT INTO analysis (blob_id, analyzer, outcome, detail, analyzed_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(blob_id, analyzer) DO UPDATE SET
               outcome = excluded.outcome,
               detail = excluded.detail,
               analyzed_at = excluded.analyzed_at",
            params![
                blob_id,
                analyzer.0.as_slice(),
                outcome.code(),
                detail,
                at_unix
            ],
        )?;
        tx.execute(
            "DELETE FROM sweep_queue WHERE blob_id = ?1 AND analyzer = ?2",
            params![blob_id, analyzer.0.as_slice()],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// The configured [`AbsentMode`] (`refine:absent:mode` in the
    /// state.db config KV). Absent or unparsable falls back to the D92
    /// default, dat-named — a typo'd value must fail toward the bounded
    /// posture, never toward `all`.
    pub fn absent_mode(&self) -> Result<AbsentMode, IndexError> {
        Ok(match self.config_get("refine:absent:mode")?.as_deref() {
            Some(b"off") => AbsentMode::Off,
            Some(b"all") => AbsentMode::All,
            _ => AbsentMode::DatNamed,
        })
    }

    /// Set (or clear, with `None`) the absent-analysis eagerness mode.
    pub fn set_absent_mode(&self, mode: Option<AbsentMode>) -> Result<(), IndexError> {
        self.config_set(
            "refine:absent:mode",
            match mode {
                Some(AbsentMode::Off) => b"off",
                Some(AbsentMode::All) => b"all",
                Some(AbsentMode::DatNamed) => b"dat-named",
                None => b"",
            },
        )
    }

    /// Recompute the D92 claim-gate admission table: non-resident blobs
    /// whose bytes the executor can actually produce (grounded under the
    /// audit-verified rule — trusted claims over retained literals),
    /// filtered by the molten eagerness policy. Called once per sweep
    /// wake (the refresh-queue beat), so the grounding fixpoint is paid
    /// per wake, never per claim.
    ///
    /// EvictedCovered blobs are admitted UNCONDITIONALLY under any
    /// non-off mode: they were resident once and were dropped exactly
    /// because a licensed route covers them — a re-shipped analyzer
    /// version must re-cover them without a dat having to name them.
    /// The dat-named gate exists for the other population: the flood of
    /// absent member claims from wild containers.
    ///
    /// Returns rows admitted.
    pub fn refresh_absent_eligibility(&self) -> Result<usize, IndexError> {
        let mode = self.absent_mode()?;
        // Fixpoint first, outside the write transaction: grounded() owns
        // its own temp-table lifecycle on this connection.
        let grounded = if mode == AbsentMode::Off {
            std::collections::HashSet::new()
        } else {
            self.grounded_set_with(crate::GroundingMode::AuditVerified)?
        };
        let conn = self.cache();
        let tx = conn.unchecked_transaction()?;
        tx.execute("DELETE FROM sweep_absent_eligible", [])?;
        if mode == AbsentMode::Off {
            tx.commit()?;
            return Ok(0);
        }
        tx.execute_batch(
            "CREATE TEMP TABLE IF NOT EXISTS d92_grounded (blob_id INTEGER PRIMARY KEY);
             DELETE FROM d92_grounded;",
        )?;
        {
            let mut ins = tx.prepare_cached("INSERT OR IGNORE INTO d92_grounded VALUES (?1)")?;
            for id in &grounded {
                ins.execute([id])?;
            }
        }
        let dat_gate = match mode {
            AbsentMode::All => "",
            AbsentMode::DatNamed => {
                "AND (b.residency = 1 OR EXISTS (
                   SELECT 1 FROM identity_blob ib
                   JOIN rom_claim rc ON rc.identity_id = ib.identity_id
                   WHERE ib.blob_id = b.blob_id))"
            }
            AbsentMode::Off => unreachable!("handled above"),
        };
        let n = tx.execute(
            &format!(
                "INSERT INTO sweep_absent_eligible (blob_id)
                 SELECT b.blob_id FROM blob b
                 JOIN d92_grounded g ON g.blob_id = b.blob_id
                 WHERE b.namespace = 0 AND b.residency != 0 {dat_gate}"
            ),
            [],
        )?;
        tx.execute_batch("DROP TABLE IF EXISTS temp.d92_grounded")?;
        tx.commit()?;
        Ok(n)
    }

    /// Total (positive, negative) provenance rows for one analyzer —
    /// the D93 tray note takes a delta across a drain, so the numbers
    /// reflect the whole worker fleet's outcomes, not one worker's
    /// private counters.
    pub fn analysis_counts(&self, analyzer: &Blake3) -> Result<(u64, u64), IndexError> {
        let (positive, negative): (i64, i64) = self.cache().query_row(
            "SELECT COALESCE(SUM(outcome = 1), 0), COALESCE(SUM(outcome = 0), 0)
             FROM analysis WHERE analyzer = ?1",
            params![analyzer.0.as_slice()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        Ok((
            u64::try_from(positive).unwrap_or(0),
            u64::try_from(negative).unwrap_or(0),
        ))
    }

    /// Queue depth for one analyzer (status surface).
    pub fn sweep_queue_len(&self, analyzer: &Blake3) -> Result<u64, IndexError> {
        let n: i64 = self.cache().query_row(
            "SELECT COUNT(*) FROM sweep_queue WHERE analyzer = ?1",
            params![analyzer.0.as_slice()],
            |row| row.get(0),
        )?;
        Ok(u64::try_from(n).unwrap_or(0))
    }
}

/// See [`Db::lease_keeper`]. Renewal is unconditional by design: with
/// no holder column, a worker whose lease lapsed and was re-claimed
/// cannot tell the thief's lease from its own — extending it is
/// harmless (leases are dedup, both holders finishing is idempotent),
/// and a holder column would buy nothing but that distinction.
/// Renewing a completed item no-ops (its queue row is gone).
pub struct SweepLeaseKeeper {
    conn: rusqlite::Connection,
}

impl SweepLeaseKeeper {
    /// Re-stamp one item's lease to `now_unix + lease_secs`.
    ///
    /// # Errors
    /// Index I/O — callers on the heartbeat path should treat a
    /// failure as "renewal missed" (the lease lapses and another
    /// worker may duplicate the item), never as fatal to the analysis.
    pub fn renew(
        &self,
        blob_id: i64,
        analyzer: &Blake3,
        now_unix: i64,
        lease_secs: i64,
    ) -> Result<(), IndexError> {
        self.conn.execute(
            "UPDATE sweep_queue SET leased_until = ?3
             WHERE blob_id = ?1 AND analyzer = ?2",
            params![
                blob_id,
                analyzer.0.as_slice(),
                now_unix.saturating_add(lease_secs)
            ],
        )?;
        Ok(())
    }
}
