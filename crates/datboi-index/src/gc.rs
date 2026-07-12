//! The GC-family index surface (D72/D73): the eviction singleton
//! guard and orphan candidate marks.
//!
//! Two lease kinds live in this database and they are NOT the same
//! thing: sweep leases (analysis.rs) are dedup — losing one costs a
//! duplicated pure function. The [`gc_guard`] here is a CORRECTNESS
//! gate: two eviction runs computing the D21 grounding fixpoint
//! concurrently can each approve dropping one half of a
//! mutually-inverse recipe pair and jointly strand both. Every drop
//! critical section (watermark eviction, orphan apply, CLI evict)
//! must hold this guard.
//!
//! Orphan marks are cache-grade review state, never authority: the
//! sweep re-derives them, any sweep clears a mark the moment
//! something roots the blob, and apply re-verifies reachability at
//! delete time — the mark only ever says "surfaced for review since
//! `marked_at`".

use datboi_core::hash::Blake3;
use rusqlite::{OptionalExtension, params};

use crate::{Db, IndexError};

/// Opaque holder identity for the singleton guard: random bytes minted
/// once per would-be holder (process/worker), never persisted anywhere
/// else. Two holders with the same bytes would alias — mint from a
/// real entropy source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GuardHolder(pub [u8; 16]);

impl Db {
    /// Try to take (or re-take) the eviction singleton guard until
    /// `now_unix + ttl_secs`. Atomic under WAL: exactly one holder can
    /// win an expired or free guard. Re-claiming while already the
    /// holder renews — claim and renew are the same statement, so
    /// there is no separate renewal race.
    pub fn claim_gc_guard(
        &self,
        holder: &GuardHolder,
        now_unix: i64,
        ttl_secs: i64,
    ) -> Result<bool, IndexError> {
        let n = self.cache().execute(
            "UPDATE gc_guard SET holder = ?1, expires_at = ?2
             WHERE guard_id = 1
               AND (holder IS NULL OR expires_at <= ?3 OR holder = ?1)",
            params![
                holder.0.as_slice(),
                now_unix.saturating_add(ttl_secs),
                now_unix
            ],
        )?;
        Ok(n == 1)
    }

    /// Release the guard iff we still hold it (a lapsed-and-stolen
    /// guard must not be released out from under the new holder).
    pub fn release_gc_guard(&self, holder: &GuardHolder) -> Result<(), IndexError> {
        self.cache().execute(
            "UPDATE gc_guard SET holder = NULL, expires_at = 0 WHERE holder = ?1",
            params![holder.0.as_slice()],
        )?;
        Ok(())
    }

    /// One orphan-mark sweep (D73), atomically: clear every mark whose
    /// blob anything now roots, then mark every currently-unrooted
    /// candidate that isn't already marked (`INSERT OR IGNORE`
    /// preserves the original `marked_at` — first-observed-unreferenced
    /// is the grace clock). `extra_roots` carries the roots this layer
    /// cannot compute itself (tag→snapshot closure needs store reads).
    ///
    /// A candidate is a resident data blob that is NOT: referenced by
    /// any recipe (either direction, any verify state), a dat revision
    /// or detector blob, catalog-named (identity ∩ rom_claim), pinned,
    /// queued for any analyzer (its references may not exist YET), or
    /// in `extra_roots`.
    ///
    /// Returns `(marked, cleared)`.
    pub fn sweep_orphan_marks(
        &mut self,
        extra_roots: &[i64],
        now_unix: i64,
    ) -> Result<(usize, usize), IndexError> {
        let tx = self.cache.transaction()?;
        tx.execute_batch(
            "CREATE TEMP TABLE IF NOT EXISTS gc_extra_roots (blob_id INTEGER PRIMARY KEY);
             DELETE FROM gc_extra_roots;",
        )?;
        {
            let mut insert =
                tx.prepare_cached("INSERT OR IGNORE INTO gc_extra_roots (blob_id) VALUES (?1)")?;
            for &blob_id in extra_roots {
                insert.execute([blob_id])?;
            }
        }
        // The candidate predicate, shared by both statements below via
        // textual inclusion (SQLite has no CTE-across-statements).
        const CANDIDATE: &str = "b.namespace = 0 AND b.residency = 0
               AND b.pinned_reason IS NULL
               AND NOT EXISTS (SELECT 1 FROM recipe_input  ri WHERE ri.blob_id = b.blob_id)
               AND NOT EXISTS (SELECT 1 FROM recipe_output ro WHERE ro.blob_id = b.blob_id)
               AND NOT EXISTS (SELECT 1 FROM dat_revision  dr WHERE dr.blob_id = b.blob_id)
               AND NOT EXISTS (SELECT 1 FROM detector      de WHERE de.blob_id = b.blob_id)
               AND NOT EXISTS (SELECT 1 FROM identity_blob ib
                               JOIN rom_claim rc ON rc.identity_id = ib.identity_id
                               WHERE ib.blob_id = b.blob_id)
               AND NOT EXISTS (SELECT 1 FROM sweep_queue   sq WHERE sq.blob_id = b.blob_id)
               AND NOT EXISTS (SELECT 1 FROM gc_extra_roots xr WHERE xr.blob_id = b.blob_id)";
        let cleared = tx.execute(
            &format!(
                "DELETE FROM orphan_candidate WHERE blob_id NOT IN (
                   SELECT b.blob_id FROM blob b WHERE {CANDIDATE})"
            ),
            [],
        )?;
        let marked = tx.execute(
            &format!(
                "INSERT OR IGNORE INTO orphan_candidate (blob_id, marked_at)
                 SELECT b.blob_id, ?1 FROM blob b WHERE {CANDIDATE}"
            ),
            [now_unix],
        )?;
        tx.commit()?;
        Ok((marked, cleared))
    }

    /// Marks that have aged past `grace_secs` — the reviewable set,
    /// oldest first, with provenance (any source paths that once
    /// carried these bytes). The caller filters keep-marks (state.db)
    /// and re-verifies at apply time; this is a read model.
    pub fn list_orphan_candidates(
        &self,
        now_unix: i64,
        grace_secs: i64,
    ) -> Result<Vec<OrphanCandidate>, IndexError> {
        let mut stmt = self.cache().prepare_cached(
            "SELECT oc.blob_id, b.hash, b.size, oc.marked_at,
                    (SELECT GROUP_CONCAT(sf.path, x'0a') FROM source_file sf
                     WHERE sf.blob_id = oc.blob_id)
             FROM orphan_candidate oc JOIN blob b ON b.blob_id = oc.blob_id
             WHERE oc.marked_at <= ?1
             ORDER BY oc.marked_at, oc.blob_id",
        )?;
        let rows = stmt
            .query_map([now_unix.saturating_sub(grace_secs)], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, [u8; 32]>(1)?,
                    row.get::<_, Option<i64>>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, Option<String>>(4)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows
            .into_iter()
            .map(|(blob_id, hash, size, marked_at, paths)| OrphanCandidate {
                blob_id,
                hash: Blake3(hash),
                size: size.and_then(|s| u64::try_from(s).ok()),
                marked_at,
                sources: paths
                    .map(|p| p.split('\n').map(str::to_owned).collect())
                    .unwrap_or_default(),
            })
            .collect())
    }

    /// Delete-time re-verification (D73): is this blob STILL a marked,
    /// aged, unrooted candidate right now? The apply path calls this
    /// under the gc guard immediately before destroying bytes — the
    /// mark alone never justifies a delete.
    pub fn orphan_still_deletable(
        &self,
        blob_id: i64,
        extra_roots: &[i64],
        now_unix: i64,
        grace_secs: i64,
    ) -> Result<bool, IndexError> {
        if extra_roots.contains(&blob_id) {
            return Ok(false);
        }
        let aged: Option<i64> = self
            .cache()
            .query_row(
                "SELECT oc.blob_id FROM orphan_candidate oc
                 JOIN blob b ON b.blob_id = oc.blob_id
                 WHERE oc.blob_id = ?1 AND oc.marked_at <= ?2
                   AND b.namespace = 0 AND b.residency = 0
                   AND b.pinned_reason IS NULL
                   AND NOT EXISTS (SELECT 1 FROM recipe_input  ri WHERE ri.blob_id = b.blob_id)
                   AND NOT EXISTS (SELECT 1 FROM recipe_output ro WHERE ro.blob_id = b.blob_id)
                   AND NOT EXISTS (SELECT 1 FROM dat_revision  dr WHERE dr.blob_id = b.blob_id)
                   AND NOT EXISTS (SELECT 1 FROM detector      de WHERE de.blob_id = b.blob_id)
                   AND NOT EXISTS (SELECT 1 FROM identity_blob ib
                                   JOIN rom_claim rc ON rc.identity_id = ib.identity_id
                                   WHERE ib.blob_id = b.blob_id)
                   AND NOT EXISTS (SELECT 1 FROM sweep_queue   sq WHERE sq.blob_id = b.blob_id)",
                params![blob_id, now_unix.saturating_sub(grace_secs)],
                |row| row.get(0),
            )
            .optional()?;
        Ok(aged.is_some())
    }

    /// Remove every cache row for a deleted orphan (children first, FKs
    /// on; `source_file` keeps its path history with the blob link
    /// nulled). Runs AFTER the store unlink in one transaction — a
    /// crash between unlink and this leaves a Resident row with no
    /// file, exactly the direction recovery's store scan reconciles
    /// (bytes are truth).
    pub fn delete_orphan_rows(&mut self, blob_id: i64) -> Result<(), IndexError> {
        let tx = self.cache.transaction()?;
        for sql in [
            "DELETE FROM orphan_candidate WHERE blob_id = ?1",
            "DELETE FROM sweep_queue WHERE blob_id = ?1",
            "DELETE FROM analysis WHERE blob_id = ?1",
            "DELETE FROM peer_have WHERE blob_id = ?1",
            "DELETE FROM identity_blob WHERE blob_id = ?1",
            "DELETE FROM alias WHERE blob_id = ?1",
            "UPDATE source_file SET blob_id = NULL WHERE blob_id = ?1",
            "DELETE FROM blob WHERE blob_id = ?1",
        ] {
            tx.execute(sql, [blob_id])?;
        }
        tx.commit()?;
        Ok(())
    }
}

/// One reviewable orphan (D73): aged past grace, still unrooted as of
/// the last sweep.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrphanCandidate {
    pub blob_id: i64,
    pub hash: Blake3,
    pub size: Option<u64>,
    pub marked_at: i64,
    /// Paths that ever carried these bytes (ingest provenance) — the
    /// context a reviewer needs to say "junk" or "keep".
    pub sources: Vec<String>,
}
