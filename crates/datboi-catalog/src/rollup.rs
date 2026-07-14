//! Audit rollups (docs/schema.md §3): two recomputed stages, never
//! triggers. Stage 1 `identity_status` scores each identity's best
//! availability; stage 2 `entry_audit` aggregates per entry.
//!
//! Note on the entry_audit column layout (frozen with SCHEMA_VERSION 1):
//! there is no `probable` column — the schema's own sketch folds
//! `probable` into `missing` at the rollup layer, and [`crate::audit`]
//! splits it back out live from identity_status when building reports
//! (D39's six states are a *report* contract, not a rollup-table one).

use datboi_index::{Db, GroundingMode};
use rusqlite::params;

use crate::CatalogError;

/// identity_status.state codes (schema.md §3).
pub const STATE_HAVE_VERIFIED: i64 = 4;
pub const STATE_HAVE_CLAIMED: i64 = 3;
pub const STATE_PEER: i64 = 2;
pub const STATE_PROBABLE: i64 = 1;
pub const STATE_MISSING: i64 = 0;

/// Recompute both rollup stages for all materialized revisions.
pub fn refresh_rollups(db: &mut Db, computed_at: i64) -> Result<(), CatalogError> {
    // Grounded sets under the two audit trust rules (D4 language; the
    // eviction rule stays D25-only). Computed via the index's fixpoint,
    // then handed to SQL as temp tables.
    let verified = db.grounded_set_with(GroundingMode::AuditVerified)?;
    let claimed = db.grounded_set_with(GroundingMode::AuditClaimed)?;

    let conn = db.cache();
    let tx = conn.unchecked_transaction()?;
    tx.execute_batch(
        "DROP TABLE IF EXISTS temp.g_verified;
         DROP TABLE IF EXISTS temp.g_claimed;
         CREATE TEMP TABLE g_verified (blob_id INTEGER PRIMARY KEY);
         CREATE TEMP TABLE g_claimed (blob_id INTEGER PRIMARY KEY);",
    )?;
    {
        let mut ins_v = tx.prepare("INSERT INTO temp.g_verified VALUES (?1)")?;
        for id in &verified {
            ins_v.execute([id])?;
        }
        let mut ins_c = tx.prepare("INSERT INTO temp.g_claimed VALUES (?1)")?;
        for id in &claimed {
            ins_c.execute([id])?;
        }
    }

    // Stage 1: best availability per identity. Availability of a blob =
    // literal residency or grounding; evidence quality = identity_blob
    // basis (md5-or-better = strong, crc+size = probable).
    tx.execute_batch("DELETE FROM identity_status;")?;
    tx.execute(
        "INSERT INTO identity_status (identity_id, state)
         SELECT ib.identity_id,
                MAX(CASE
                    WHEN ib.basis >= 1 AND (b.residency = 0
                         OR b.blob_id IN (SELECT blob_id FROM temp.g_verified))
                      THEN 4
                    WHEN ib.basis >= 1
                         AND b.blob_id IN (SELECT blob_id FROM temp.g_claimed)
                      THEN 3
                    WHEN ph.blob_id IS NOT NULL
                      THEN 2
                    WHEN ib.basis <= 0 AND (b.residency = 0
                         OR b.blob_id IN (SELECT blob_id FROM temp.g_verified))
                      THEN 1
                    ELSE 0
                END)
         FROM identity_blob ib
         JOIN blob b ON b.blob_id = ib.blob_id
         LEFT JOIN peer_have ph ON ph.blob_id = ib.blob_id
         GROUP BY ib.identity_id",
        [],
    )?;

    // Stage 2: per-entry aggregation over materialized revisions.
    // required excludes nodump (forcenodump default: can never be
    // satisfied) and optional claims; mia stays required (it is surfaced,
    // not excused). probable folds into `missing` here (see module docs).
    tx.execute_batch("DELETE FROM entry_audit;")?;
    tx.execute(
        "INSERT INTO entry_audit
           (entry_id, required, have_verified, have_claimed, peer_avail,
            missing, computed_at)
         SELECT rc.entry_id,
                COALESCE(SUM(rc.status != 2 AND NOT rc.optional), 0),
                COALESCE(SUM(s.state = 4 AND rc.status != 2 AND NOT rc.optional), 0),
                COALESCE(SUM(s.state = 3 AND rc.status != 2 AND NOT rc.optional), 0),
                COALESCE(SUM(s.state = 2 AND rc.status != 2 AND NOT rc.optional), 0),
                COALESCE(SUM((s.state IS NULL OR s.state <= 1)
                             AND rc.status != 2 AND NOT rc.optional), 0),
                ?1
         FROM rom_claim rc
         JOIN entry e ON e.entry_id = rc.entry_id
         JOIN dat_revision dr ON dr.revision_id = e.revision_id
         LEFT JOIN identity_status s ON s.identity_id = rc.identity_id
         WHERE dr.materialized = 1
         GROUP BY rc.entry_id",
        params![computed_at],
    )?;

    tx.execute_batch(
        "DROP TABLE IF EXISTS temp.g_verified;
         DROP TABLE IF EXISTS temp.g_claimed;",
    )?;
    tx.commit()?;
    Ok(())
}
