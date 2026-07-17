//! Scrub — the corpus integrity walk (D61/D74/D91), descended from the
//! CLI (D96) so `datboi scrub` and the daemon's on-demand `POST /v1/scrub`
//! run ONE implementation. Three passes, in order:
//!
//! 1. **Loose walk** — a deterministic hash-prefix sample of every loose
//!    blob (Data + Meta). Each read re-hashes AND recomputes the full
//!    alias tuple, so scrub doubles as fast-recovery's alias back-fill:
//!    rows the metadata-only recovery walk indexed get their aliases +
//!    `verified_at` here (docs open-questions, fast-recovery note).
//! 2. **Pack scrub** (D91) — sealed packs are invisible to the loose walk,
//!    so whenever sampling is active every pack is re-hashed whole (one
//!    read certifies every member; the filename IS the identity). Packs
//!    are O(decompositions), so they are scrubbed in full, not sampled.
//! 3. **Rehabilitation** (D61, opt-in) — re-execute poisoned recipes; a
//!    verified re-replay is the one sanctioned exit from `Failed`.
//!
//! Deterministic conclusions about bytes are `corrupt`/`missing` entries
//! in the report, never `Err` (D81 — `Err` is environmental only).

use datboi_index::types::Namespace as IndexNs;
use datboi_index::{Db, Residency};
use datboi_store_fs::{Namespace as StoreNs, StoreError, VerifyOutcome};

use crate::{ExecError, Executor};

/// Outcome of one scrub run — printed by the CLI, folded into a job note
/// by the daemon. Counts and hex-hash lists only; no store/DB borrows.
#[derive(Debug, Default)]
pub struct ScrubReport {
    /// Blobs (loose + packed members) read this run.
    pub checked: u64,
    /// Index rows whose `verified_at` (and aliases) were refreshed.
    pub refreshed: u64,
    /// Hex hashes whose bytes did not match their name (packs prefixed
    /// `pack <hash>` when the whole file rotted).
    pub corrupt: Vec<String>,
    /// Hex hashes the index claims resident but the store lacks.
    pub missing: Vec<String>,
    /// Recipe ids returned to service by rehabilitation.
    pub rehabilitated: Vec<i64>,
    /// (recipe id, error) — recipes still poisoned after a rehab attempt.
    pub still_failed: Vec<(i64, String)>,
}

impl ScrubReport {
    /// A clean run found no corrupt or missing bytes.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.corrupt.is_empty() && self.missing.is_empty()
    }
}

impl Executor<'_> {
    /// Walk the corpus verifying bytes against their names (the D96
    /// shared scrub primitive). `sample_pct` selects a deterministic
    /// hash-prefix subset of loose blobs — `0` skips the loose walk AND
    /// the pack pass. `rehabilitate` additionally re-runs poisoned
    /// recipes. `now` stamps every refreshed `verified_at`.
    ///
    /// # Errors
    /// Environmental store/index failures only — a byte disproof is a
    /// `corrupt`/`missing` entry in the report, never an `Err` (D81).
    pub fn scrub(
        &self,
        db: &Db,
        sample_pct: u8,
        rehabilitate: bool,
        now: i64,
    ) -> Result<ScrubReport, ExecError> {
        let pct = sample_pct.min(100);
        let mut report = ScrubReport::default();
        for ns in [StoreNs::Data, StoreNs::Meta] {
            for item in self.store.list(ns) {
                let (hash, size) = match item {
                    Ok(pair) => pair,
                    Err(StoreError::Foreign { .. }) => continue,
                    Err(e) => return Err(e.into()),
                };
                // Deterministic sampling by hash prefix: no RNG, so the
                // same subset is checked every run at a given percentage.
                if u32::from(hash.0[0]) * 100 >= u32::from(pct) * 256 {
                    continue;
                }
                report.checked += 1;
                // The same read computes the full alias tuple, so scrub is
                // also fast-recovery's back-fill: blobs the metadata-only
                // walk indexed get their aliases + verified_at here.
                match self.store.verify_with_aliases(ns, &hash)? {
                    (VerifyOutcome::Valid, aliases) => {
                        let ns_row = match ns {
                            StoreNs::Data => IndexNs::Data,
                            StoreNs::Meta => IndexNs::Meta,
                        };
                        let blob_id =
                            db.upsert_blob(&hash, Some(size), ns_row, Residency::Resident)?;
                        if let Some(aliases) = &aliases {
                            db.insert_aliases(blob_id, aliases)?;
                        }
                        db.set_verified(blob_id, now)?;
                        report.refreshed += 1;
                    }
                    (VerifyOutcome::Corrupt { .. }, _) => report.corrupt.push(hash.to_hex()),
                    (VerifyOutcome::Missing, _) => report.missing.push(hash.to_hex()),
                }
            }
        }
        // Sealed packs (D91): the loose walk above never sees packed
        // members, so a rotting pack was invisible to scrub. Each pack
        // carries its own identity (the filename), so re-hashing the whole
        // file is the strongest and cheapest proof — a match certifies
        // every member. Packs are O(decompositions), so we scrub them ALL
        // whenever sampling is active, not by hash prefix.
        if pct > 0 {
            for pack in self.store.list_packs() {
                let scrub = self.store.scrub_pack(&pack)?;
                report.checked += scrub.members.len() as u64;
                if !scrub.intact {
                    report.corrupt.push(format!("pack {}", pack.to_hex()));
                }
                for member in &scrub.members {
                    match &member.aliases {
                        Some(aliases) => {
                            let blob_id = db.upsert_blob(
                                &member.hash,
                                Some(member.len),
                                IndexNs::Data,
                                Residency::Resident,
                            )?;
                            db.insert_aliases(blob_id, aliases)?;
                            db.set_verified(blob_id, now)?;
                            report.refreshed += 1;
                        }
                        // The member's own bytes did not hash to its
                        // identity (scrub_pack left aliases None): corrupt.
                        None => report.corrupt.push(member.hash.to_hex()),
                    }
                }
            }
        }
        // Rehabilitation (D61, D54-era work item): re-execute poisoned
        // recipes; a verified re-replay is the one sanctioned exit from
        // Failed.
        if rehabilitate {
            for recipe_id in db.list_failed_recipes()? {
                match self.rehabilitate(db, recipe_id) {
                    Ok(_) => report.rehabilitated.push(recipe_id),
                    Err(e) => report.still_failed.push((recipe_id, e.to_string())),
                }
            }
        }
        Ok(report)
    }
}
