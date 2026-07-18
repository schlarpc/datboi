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
//!   the target is met. Pinned views protect what they serve
//!   ([`Executor::pinned_protected_set`]): `image/*` tags protect every
//!   input of the image recipe (content + skeleton — evicting one
//!   degrades serving to spill-per-window and kills the D63 carve-out),
//!   and `view/*` tags protect opaque-classed rows (the D27 clause,
//!   applied conservatively: any snapshot row recorded opaque).
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
    /// A pinned view or image tag references this blob (D27): eviction
    /// would degrade pinned serving. Drop or move the tag first.
    PinnedByView,
    /// The blob's local bytes live inside a sealed pack (D91): packs
    /// are immutable, and packed pieces are grounding leaves — freeing
    /// their bytes is a future tombstone-and-repack, never an evict.
    Packed,
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
pub struct LicenseReport {
    /// Blobs whose route reached `ReplayedLocal` this drain.
    pub replayed: usize,
    /// (blob, why) — routes that did not license; retried next drain.
    pub failed: Vec<(Blake3, String)>,
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
        let protected = self.pinned_protected_set(db)?;
        self.evict_with(db, hash, &protected)
    }

    fn evict_with(
        &self,
        db: &Db,
        hash: &Blake3,
        protected: &std::collections::HashSet<i64>,
    ) -> Result<EvictOutcome, ExecError> {
        let Some(row) = db.blob_by_hash(hash)? else {
            return Ok(EvictOutcome::Blocked(Blocked::NotResident));
        };
        if row.residency != Residency::Resident || !self.store.has(StoreNs::Data, hash) {
            return Ok(EvictOutcome::Blocked(Blocked::NotResident));
        }
        if protected.contains(&row.blob_id) {
            return Ok(EvictOutcome::Blocked(Blocked::PinnedByView));
        }
        // D91: a packed blob has no loose file to unlink — flipping its
        // residency would strand index truth away from byte truth.
        if self.store.is_packed(hash) && !self.store.has_loose(StoreNs::Data, hash) {
            return Ok(EvictOutcome::Blocked(Blocked::Packed));
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
        // A blob that was BOTH loose and packed (a re-ingested duplicate
        // of a packed piece) just lost its duplicate, not its bytes:
        // residency stays Resident, serving continues out of the pack.
        db.set_residency(
            row.blob_id,
            if self.store.has(StoreNs::Data, hash) {
                Residency::Resident
            } else {
                Residency::EvictedCovered
            },
        )?;
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
        let protected = self.pinned_protected_set(db)?;
        for candidate in db.list_eviction_candidates()? {
            if resident <= target_resident_bytes {
                break;
            }
            match self.evict_with(db, &candidate.hash, &protected)? {
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
            // Don't burn a licensing replay on a blob the pin check
            // would refuse anyway.
            if protected.contains(&blob_id) {
                report.blocked.push((hash, Blocked::PinnedByView));
                continue;
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
                report.blocked.push((hash, Blocked::NeedsReplay));
                continue;
            }
            match self.evict_with(db, &hash, &protected)? {
                EvictOutcome::Evicted { bytes_reclaimed } => {
                    report.evicted += 1;
                    report.bytes_reclaimed += bytes_reclaimed;
                    resident = resident.saturating_sub(bytes_reclaimed);
                }
                // Licensed and STILL blocked: this must be visible, not
                // silently skipped (a replay ran and nothing happened —
                // exactly the report a human needs to see explained).
                EvictOutcome::Blocked(why) => report.blocked.push((hash, why)),
            }
        }
        Ok(report)
    }

    /// Eager ambient licensing (D72): replay up to `limit` routes from
    /// the verified-only pool — recipes covering CURRENTLY RESIDENT
    /// blobs, the one scope where replay is storage-neutral (outputs
    /// already resident; the content-addressed put no-ops). Blanket
    /// licensing of every Verified recipe would materialize member
    /// CLAIMS into the store — the bytes D35 ruled we never store.
    /// Additive and idempotent, so it needs NO gc guard; each success
    /// commits `ReplayedLocal` per recipe (at-least-once).
    ///
    /// # Errors
    /// Index/store failures abort the drain; per-route failures ride
    /// the report and the pool retries them next drain (poisoned
    /// claim-failures drop out of the pool by verify state).
    pub fn license_covered(&self, db: &Db, limit: usize) -> Result<LicenseReport, ExecError> {
        let mut report = LicenseReport::default();
        for (blob_id, hash) in verified_only_candidates(db)?.into_iter().take(limit) {
            let mut licensed = false;
            let mut last_error = None;
            for recipe in db.recipes_for_output(blob_id)? {
                if recipe.verify != VerifyState::Verified {
                    continue;
                }
                match self.replay(db, recipe.recipe_id) {
                    Ok(_) => {
                        licensed = true;
                        break;
                    }
                    Err(e) if e.is_claim_failure() => {
                        last_error = Some(e.to_string()); // poisoned; try next route
                    }
                    Err(e) => {
                        last_error = Some(e.to_string());
                        break; // infrastructure: report, move to next blob
                    }
                }
            }
            if licensed {
                report.replayed += 1;
            } else {
                report.failed.push((
                    hash,
                    last_error.unwrap_or_else(|| "no Verified route".into()),
                ));
            }
        }
        Ok(report)
    }

    /// How many resident blobs currently sit in the verified-only pool
    /// (would-be licensing work) — the maintenance worker's job-sizing
    /// read.
    ///
    /// # Errors
    /// Index I/O.
    pub fn license_pending(&self, db: &Db) -> Result<usize, ExecError> {
        Ok(verified_only_candidates(db)?.len())
    }

    /// Watermark reclaim (D72): evict licensed literals until roughly
    /// `reclaim_bytes` are freed. Thin wrapper translating "free this
    /// much" into [`Executor::evict_covered`]'s resident-bytes target.
    /// Never licenses inline — D72 licenses eagerly, elsewhere.
    ///
    /// # Errors
    /// See [`Executor::evict_covered`].
    pub fn evict_reclaim(&self, db: &Db, reclaim_bytes: u64) -> Result<EvictReport, ExecError> {
        let resident = resident_data_bytes(db)?;
        self.evict_covered(db, resident.saturating_sub(reclaim_bytes), false)
    }

    /// The tag-closure roots the D73 orphan sweep cannot compute at the
    /// index layer (snapshot decode needs store reads): every tagged
    /// hash itself, plus every data blob any pinned `view/*` snapshot
    /// row references (ALL rows — orphan reachability is broader than
    /// D27's opaque-only eviction protection). `image/*` inputs are
    /// already recipe-rooted; the tagged output rides the tagged-hash
    /// rule.
    ///
    /// # Errors
    /// Index/store failures; a tag naming an undecodable snapshot
    /// errors rather than silently rooting nothing (deletion adjacency
    /// — same strictness rule as [`Executor::evict`]'s pin check).
    pub fn orphan_extra_roots(&self, db: &Db) -> Result<Vec<i64>, ExecError> {
        let mut roots = std::collections::HashSet::new();
        for (name, hash) in db.list_tags()? {
            if let Some(row) = db.blob_by_hash(&hash)? {
                roots.insert(row.blob_id);
            }
            if name.starts_with("view/") {
                use std::io::Read as _;
                let mut bytes = Vec::new();
                self.store
                    .get(StoreNs::Meta, &hash)?
                    .ok_or_else(|| {
                        ExecError::Malformed(format!("pinned snapshot {hash} missing from meta/"))
                    })?
                    .read_to_end(&mut bytes)?;
                let snap = datboi_core::viewsnap::ViewSnapshot::decode(&bytes).map_err(|e| {
                    ExecError::Malformed(format!("pinned snapshot {hash} does not decode: {e}"))
                })?;
                for row in &snap.rows {
                    if let Some(blob) = db.blob_by_hash(&row.hash)? {
                        roots.insert(blob.blob_id);
                    }
                }
            }
        }
        Ok(roots.into_iter().collect())
    }

    /// Blob ids no eviction may touch while their pins stand (D27):
    /// `image/*` tags protect every input of every non-failed recipe
    /// covering the tagged output (content windows + skeleton blobs —
    /// losing one degrades pinned-image serving to spill-per-window and
    /// disqualifies the D63 carve-out); `view/*` tags protect rows the
    /// snapshot recorded as opaque (seek class 2), whose serving would
    /// otherwise re-spill on every read.
    ///
    /// Strictness is the safe direction here: a tag that names a
    /// missing or undecodable snapshot makes this ERROR rather than
    /// silently protect nothing — eviction destroys bytes.
    fn pinned_protected_set(&self, db: &Db) -> Result<std::collections::HashSet<i64>, ExecError> {
        let mut protected = std::collections::HashSet::new();
        for (name, hash) in db.list_tags()? {
            if name.starts_with("image/") {
                let Some(row) = db.blob_by_hash(&hash)? else {
                    continue; // tag outlived its index row; nothing to protect
                };
                for recipe in db.recipes_for_output(row.blob_id)? {
                    if recipe.verify == VerifyState::Failed {
                        continue;
                    }
                    for input in db.recipe_inputs(recipe.recipe_id)? {
                        protected.insert(input.blob_id);
                    }
                }
            } else if name.starts_with("view/") {
                use std::io::Read as _;
                let mut bytes = Vec::new();
                self.store
                    .get(StoreNs::Meta, &hash)?
                    .ok_or_else(|| {
                        ExecError::Malformed(format!("pinned snapshot {hash} missing from meta/"))
                    })?
                    .read_to_end(&mut bytes)?;
                let snap = datboi_core::viewsnap::ViewSnapshot::decode(&bytes).map_err(|e| {
                    ExecError::Malformed(format!("pinned snapshot {hash} does not decode: {e}"))
                })?;
                for row in snap.rows.iter().filter(|r| r.seek == 2) {
                    if let Some(blob) = db.blob_by_hash(&row.hash)? {
                        protected.insert(blob.blob_id);
                    }
                }
            }
        }
        Ok(protected)
    }

    /// Human-readable account of why `hash` won't evict right now: which
    /// routes exist, their license states, and — for licensed routes —
    /// the first input whose own grounding fails. The normie-facing
    /// counterpart of [`Db::is_evictable`]'s bare boolean.
    ///
    /// # Errors
    /// Index failures only; unknown blobs explain themselves.
    pub fn explain_eviction(&self, db: &Db, hash: &Blake3) -> Result<Vec<String>, ExecError> {
        let Some(row) = db.blob_by_hash(hash)? else {
            return Ok(vec!["unknown blob: nothing indexed under this hash".into()]);
        };
        if row.residency != Residency::Resident {
            return Ok(vec![format!(
                "not resident ({:?}): there are no local bytes to evict",
                row.residency
            )]);
        }
        if self.pinned_protected_set(db)?.contains(&row.blob_id) {
            // D27: a view/image tag pins the bytes it serves.
            return Ok(vec![
                "pinned: a view/image tag serves these bytes; drop or move the tag first".into(),
            ]);
        }
        let recipes = db.recipes_for_output(row.blob_id)?;
        if recipes.is_empty() {
            return Ok(vec![
                "no recipe claims these bytes: nothing could rebuild them after a drop".into(),
            ]);
        }
        if db.is_evictable(row.blob_id)? {
            return Ok(vec!["evictable now: a licensed route grounds it".into()]);
        }
        let grounded = db.grounded_set()?;
        let mut lines = Vec::new();
        for recipe in &recipes {
            match recipe.verify {
                VerifyState::Failed => lines.push(format!(
                    "route via {} recipe #{}: poisoned (a replay produced wrong bytes); it will never license",
                    recipe.op_name, recipe.recipe_id
                )),
                VerifyState::Verified => lines.push(format!(
                    "route via {} recipe #{}: not yet licensed — run a replay (evict --license does this) to prove it rebuilds on this host",
                    recipe.op_name, recipe.recipe_id
                )),
                VerifyState::ReplayedLocal => {
                    // Licensed but the chain below it doesn't ground:
                    // name the offending input(s).
                    let mut offenders = Vec::new();
                    for input in db.recipe_inputs(recipe.recipe_id)? {
                        if !grounded.contains(&input.blob_id) {
                            offenders.push(format!(
                                "input {}{} {} ({:?})",
                                input.position,
                                input.role.map(|r| format!(" [{r}]")).unwrap_or_default(),
                                input.hash.to_hex(),
                                input.residency
                            ));
                        }
                    }
                    if offenders.is_empty() {
                        // D21: mutually-dependent recipes can't ground each other.
                        lines.push(format!(
                            "route via {} recipe #{}: licensed, but dropping this literal would break its own grounding (mutually-dependent recipes)",
                            recipe.op_name, recipe.recipe_id
                        ));
                    } else {
                        lines.push(format!(
                            "route via {} recipe #{}: licensed, but these inputs are not rebuildable without the evicted bytes: {}",
                            recipe.op_name,
                            recipe.recipe_id,
                            offenders.join(", ")
                        ));
                    }
                }
                VerifyState::Pending => lines.push(format!(
                    "route via {} recipe #{}: pending — this claim has never been verified; replay it first",
                    recipe.op_name, recipe.recipe_id
                )),
            }
        }
        Ok(lines)
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
