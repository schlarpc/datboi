//! Residency planner + eviction (D21/D25/D27/D49) — the first code path
//! allowed to destroy bytes, so every rule is enforced HERE, at the last
//! moment before the drop:
//!
//! - **D25 drop safety**: the blob must be reconstructible through
//!   recipes that have REPLAYED ON THIS HOST (`ReplayedLocal`), grounded
//!   in literals that remain after the drop ([`Db::is_evictable`], the
//!   D21 fixpoint — mutually-inverse recipe pairs can never justify
//!   dropping both ends).
//! - **D49 rule 1**: the outboard sidecar is (built if needed and) kept,
//!   so recipe-served range reads stay verifiable forever.
//! - **D27**: policy default is keep-both-under-high-water; the
//!   [`plan`] helper picks recipe-covered literals, biggest first, until
//!   the target is met. The opaque-route/pinned-view refinement has no
//!   subject yet (view snapshots are M4) — when views land, the planner
//!   must skip literals whose only route is opaque while a pinned view
//!   references them.
//!
//! Eviction is a planner decision, never a side effect: nothing else in
//! the codebase calls [`Store::evict_literal`].

use datboi_core::hash::Blake3;
use datboi_index::{Db, Residency, VerifyState};
use datboi_store_fs::Namespace as StoreNs;

use crate::{ExecError, Executor};

/// Why a specific blob may not be evicted right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Blocked {
    /// Not present / not resident — nothing to do.
    NotResident,
    /// No recipe route replayed-on-this-host grounds this blob in
    /// retained literals (D25/D21).
    NotGrounded,
    /// The blob has a covering recipe, but it has not replayed locally
    /// yet — run [`Executor::replay`] first (that is the license).
    NeedsReplay,
}

#[derive(Debug)]
pub enum EvictOutcome {
    /// Bytes dropped; outboard retained; residency now `EvictedCovered`.
    Evicted {
        bytes_reclaimed: u64,
    },
    Blocked(Blocked),
}

#[derive(Debug, Default)]
pub struct EvictReport {
    pub evicted: usize,
    pub bytes_reclaimed: u64,
    pub blocked: Vec<(Blake3, Blocked)>,
    /// Replays performed to license drops (when `license` was set).
    pub replays: usize,
}

impl<'s> Executor<'s> {
    /// Evict one literal if every safety rule passes. Never replays —
    /// callers wanting auto-licensing use [`Executor::evict_covered`].
    ///
    /// # Errors
    /// Index/store failures; the outboard build failing (nothing is
    /// deleted in that case).
    pub fn evict(&self, db: &Db, hash: &Blake3) -> Result<EvictOutcome, ExecError> {
        let Some(row) = db.blob_by_hash(hash)? else {
            return Ok(EvictOutcome::Blocked(Blocked::NotResident));
        };
        if row.residency != Residency::Resident || !self.store.has(StoreNs::Data, hash) {
            return Ok(EvictOutcome::Blocked(Blocked::NotResident));
        }
        if !db.is_evictable(row.blob_id)? {
            // Distinguish "needs a replay" from "structurally impossible"
            // for the report: is there any non-failed covering recipe?
            let has_route = db
                .recipes_for_output(row.blob_id)?
                .iter()
                .any(|r| r.verify != VerifyState::Failed);
            return Ok(EvictOutcome::Blocked(if has_route {
                Blocked::NeedsReplay
            } else {
                Blocked::NotGrounded
            }));
        }

        // D49 rule 1: the tree must exist before the bytes may go.
        if !self.store.ensure_obao(StoreNs::Data, hash)? {
            return Ok(EvictOutcome::Blocked(Blocked::NotResident));
        }
        let bytes = row
            .size
            .or(self.store.len(StoreNs::Data, hash)?)
            .unwrap_or(0);

        // The point of no return. Residency flips after the file drop:
        // a crash between the two leaves a row claiming Resident with no
        // file — recovery's store scan reconciles exactly that direction
        // (bytes are truth, the DB is a cache).
        self.store.evict_literal(StoreNs::Data, hash)?;
        db.set_residency(row.blob_id, Residency::EvictedCovered)?;
        Ok(EvictOutcome::Evicted {
            bytes_reclaimed: bytes,
        })
    }

    /// The high-water planner (D27): evict recipe-covered literals,
    /// biggest first, until at most `target_resident_bytes` of resident
    /// data remain (0 = evict everything evictable). With `license` set,
    /// candidates whose recipes are only `Verified` get a licensing
    /// replay (D25) first — that is CPU-for-bytes, the planner's trade.
    ///
    /// # Errors
    /// Index/store failures abort planning; per-blob replay failures are
    /// reported as blocked and planning continues.
    pub fn evict_covered(
        &self,
        db: &Db,
        target_resident_bytes: u64,
        license: bool,
    ) -> Result<EvictReport, ExecError> {
        let mut resident: u64 = resident_data_bytes(db)?;
        let mut report = EvictReport::default();
        if resident <= target_resident_bytes {
            return Ok(report);
        }
        for candidate in db.list_eviction_candidates()? {
            if resident <= target_resident_bytes {
                break;
            }
            match self.evict(db, &candidate.hash)? {
                EvictOutcome::Evicted { bytes_reclaimed } => {
                    report.evicted += 1;
                    report.bytes_reclaimed += bytes_reclaimed;
                    resident = resident.saturating_sub(bytes_reclaimed);
                }
                EvictOutcome::Blocked(why) => report.blocked.push((candidate.hash, why)),
            }
        }
        if !license || resident <= target_resident_bytes {
            return Ok(report);
        }
        // Second pass: license Verified-only routes by replaying them,
        // then evict. Candidate pool: resident blobs with non-failed
        // recipes that the first pass couldn't take.
        for (blob_id, hash) in verified_only_candidates(db)? {
            if resident <= target_resident_bytes {
                break;
            }
            let mut licensed = false;
            for recipe in db.recipes_for_output(blob_id)? {
                if recipe.verify != VerifyState::Verified {
                    continue;
                }
                match self.replay(db, recipe.recipe_id) {
                    Ok(_) => {
                        report.replays += 1;
                        licensed = true;
                        break;
                    }
                    Err(e) if e.is_claim_failure() => {} // poisoned; try next
                    Err(e) => return Err(e),
                }
            }
            if !licensed {
                continue;
            }
            if let EvictOutcome::Evicted { bytes_reclaimed } = self.evict(db, &hash)? {
                report.evicted += 1;
                report.bytes_reclaimed += bytes_reclaimed;
                resident = resident.saturating_sub(bytes_reclaimed);
            }
        }
        Ok(report)
    }
}

fn resident_data_bytes(db: &Db) -> Result<u64, ExecError> {
    let n: i64 = db
        .cache()
        .query_row(
            "SELECT COALESCE(SUM(size), 0) FROM blob WHERE namespace = 0 AND residency = 0",
            [],
            |row| row.get(0),
        )
        .map_err(datboi_index::IndexError::from)?;
    Ok(u64::try_from(n).unwrap_or(0))
}

/// Resident data blobs whose covering recipes are Verified but not yet
/// ReplayedLocal — the license-then-evict pool, biggest first.
fn verified_only_candidates(db: &Db) -> Result<Vec<(i64, Blake3)>, ExecError> {
    let mut stmt = db
        .cache()
        .prepare_cached(
            "SELECT DISTINCT b.blob_id, b.hash
             FROM blob b
             JOIN recipe_output ro ON ro.blob_id = b.blob_id
             JOIN recipe r ON r.recipe_id = ro.recipe_id
             WHERE b.namespace = 0 AND b.residency = 0 AND r.verify = 1
             ORDER BY b.size DESC, b.hash",
        )
        .map_err(datboi_index::IndexError::from)?;
    let rows = stmt
        .query_map([], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, [u8; 32]>(1)?))
        })
        .map_err(datboi_index::IndexError::from)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(datboi_index::IndexError::from)?;
    Ok(rows.into_iter().map(|(id, h)| (id, Blake3(h))).collect())
}
