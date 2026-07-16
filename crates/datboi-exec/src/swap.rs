//! The D91 affine piece-swap: pieces over container, one sealed pack
//! per decomposition. This is a MAINTENANCE PHASE (plan-time SQL, runs
//! alongside license/mark/evict), never an analyzer — D47's split
//! stays intact and the sweep queues never hear about it.
//!
//! Per candidate (a resident container with an affine builtin-assemble
//! rebuild route), in order:
//!
//! 1. **Predicate** (never eager): the fraction of the rebuild's input
//!    bytes that are shared (claimed by ≥2 decompositions) or already
//!    resident must clear the molten threshold
//!    (`swap:share-min-pct`). A lone ROM's swap buys pad savings and
//!    pays an inode-and-IO bill, so it never trips.
//! 2. **Headroom** (D56): absent piece bytes + slack must fit before
//!    anything is written — the swap is transiently double-resident by
//!    design.
//! 3. **Pack**: absent pieces stream through the executor (their
//!    derive routes ground in the still-resident container) into ONE
//!    sealed pack, coverage order, every member verified on the way
//!    in. Residency flips to Resident per member after the pack
//!    publishes (bytes first, rows second — recovery's direction).
//! 4. **License**: the rebuild route replays if it hasn't (D25 — the
//!    drop needs ReplayedLocal, not just Verified).
//! 5. **Evict** the container through the ordinary planner path (D21
//!    grounding counterfactual, D49 outboard, D27 protections). The
//!    caller holds the D72 singleton guard across this step.
//!
//! Crash safety is compositional: a pack without residency rows is
//! bytes-are-truth (re-swap re-packs the identical member set to the
//! identical pack hash — an idempotent rename); a licensed-but-not-
//! evicted container is just an eviction candidate; nothing here has a
//! state the next ambient cycle can't finish or redo.

use datboi_core::hash::Blake3;
use datboi_index::{Db, Residency, SwapCandidate, VerifyState};
use datboi_store_fs::{Namespace as StoreNs, PackMember};

use crate::evict::EvictOutcome;
use crate::{ExecError, Executor, policy};

/// Fixed safety margin over the summed absent piece bytes (mirrors the
/// materialize guard's posture).
const SWAP_SLACK: u64 = 256 * 1024 * 1024;

#[derive(Debug, Default)]
pub struct SwapReport {
    /// Containers fully swapped (packed + licensed + evicted).
    pub swapped: usize,
    /// Packs written this run.
    pub packs: usize,
    /// Bytes newly materialized into packs.
    pub bytes_packed: u64,
    /// Bytes reclaimed by container evictions.
    pub bytes_reclaimed: u64,
    /// Candidates whose sharing fraction did not clear the threshold.
    pub below_threshold: usize,
    /// (container, why) — candidates skipped for other reasons
    /// (headroom, missing sizes, eviction blocked); retried next cycle.
    pub skipped: Vec<(Blake3, String)>,
}

impl<'s> Executor<'s> {
    /// Run the D91 swap phase over every candidate. The CALLER holds
    /// the D72 gc guard: the evict step computes grounding
    /// counterfactuals, and two planners interleaving those can strand
    /// a mutually-inverse pair.
    ///
    /// # Errors
    /// Index/store failures abort the phase; per-candidate problems
    /// land in the report and retry next cycle.
    pub fn swap_covered(&self, db: &mut Db) -> Result<SwapReport, ExecError> {
        let mut report = SwapReport::default();
        if !policy::swap_enabled(db)? {
            return Ok(report);
        }
        let threshold = u64::from(policy::swap_share_min_pct(db)?);
        for candidate in db.swap_candidates()? {
            match self.swap_one(db, &candidate, threshold, &mut report) {
                Ok(()) => {}
                Err(SwapSkip::BelowThreshold) => report.below_threshold += 1,
                Err(SwapSkip::Other(why)) => report.skipped.push((candidate.hash, why)),
            }
        }
        Ok(report)
    }

    fn swap_one(
        &self,
        db: &mut Db,
        candidate: &SwapCandidate,
        threshold_pct: u64,
        report: &mut SwapReport,
    ) -> Result<(), SwapSkip> {
        let inputs = db
            .rebuild_inputs(candidate.recipe_id)
            .map_err(|e| SwapSkip::Other(e.to_string()))?;
        if inputs.is_empty() {
            return Err(SwapSkip::Other("rebuild route has no inputs".into()));
        }
        let mut total = 0u64;
        let mut shared = 0u64;
        for input in &inputs {
            let Some(size) = input.size else {
                return Err(SwapSkip::Other(format!(
                    "piece {} has no recorded size",
                    input.hash
                )));
            };
            total += size;
            // Resident pieces cost nothing more; multi-claimed pieces
            // are the cross-variant dedup evidence.
            if input.residency == Residency::Resident || input.covering_claims >= 2 {
                shared += size;
            }
        }
        // The predicate — never eager (D91). total == 0 (all-literal
        // rebuilds) has nothing to pack and nothing to gain.
        if total == 0 || shared * 100 < total * threshold_pct {
            return Err(SwapSkip::BelowThreshold);
        }

        // Absent pieces to materialize, deduped, coverage order.
        let mut seen = std::collections::HashSet::new();
        let mut to_pack: Vec<PackMember> = Vec::new();
        let mut piece_ids: Vec<(i64, Blake3)> = Vec::new();
        for input in &inputs {
            if input.residency == Residency::Resident || !seen.insert(input.hash) {
                continue;
            }
            to_pack.push(PackMember {
                hash: input.hash,
                len: input.size.expect("checked above"),
            });
            piece_ids.push((input.blob_id, input.hash));
        }

        if !to_pack.is_empty() {
            // D56 headroom: the swap is transiently double-resident.
            let need: u64 = to_pack
                .iter()
                .map(|m| m.len)
                .sum::<u64>()
                .saturating_add(SWAP_SLACK);
            if let Some(have) = self
                .store_ref()
                .available_bytes()
                .map_err(|e| SwapSkip::Other(e.to_string()))?
                && have < need
            {
                return Err(SwapSkip::Other(format!(
                    "insufficient headroom: need ~{need} bytes, have {have}"
                )));
            }
            // Stream every absent piece out of the still-resident
            // container into one sealed pack.
            let pack_bytes: u64 = to_pack.iter().map(|m| m.len).sum();
            self.store_ref()
                .put_pack(&to_pack, |ix| {
                    let reader = self
                        .open_stream(&*db, &to_pack[ix].hash)
                        .map_err(std::io::Error::other)?;
                    Ok(reader)
                })
                .map_err(|e| SwapSkip::Other(format!("pack write: {e}")))?;
            report.packs += 1;
            report.bytes_packed += pack_bytes;
            // Bytes first, rows second: flip residency now that the
            // pack is durable.
            for (blob_id, _) in &piece_ids {
                db.set_residency(*blob_id, Residency::Resident)
                    .map_err(|e| SwapSkip::Other(e.to_string()))?;
            }
            // Bless each packed piece's obao over its window NOW, while
            // the bytes are warm from the pack write (D91 amendment). The
            // very next step evicts the container; its first served range
            // replays through these pieces via the D63 carve-out, which
            // would otherwise ensure_obao them lazily ON the serving
            // thread — a stall proportional to the whole decomposition.
            // Sidecars live beside the member (`data/…/<hex>.obao`),
            // never inside the immutable pack, so this "upgrades the
            // window" exactly as the landing note promised.
            for member in &to_pack {
                self.store_ref()
                    .ensure_obao(StoreNs::Data, &member.hash)
                    .map_err(|e| SwapSkip::Other(format!("obao bless: {e}")))?;
            }
        }

        // License the rebuild if Verified-only (D25).
        let row = db
            .recipe_by_id(candidate.recipe_id)
            .map_err(|e| SwapSkip::Other(e.to_string()))?;
        if row.verify != VerifyState::ReplayedLocal {
            self.replay(db, candidate.recipe_id)
                .map_err(|e| SwapSkip::Other(format!("licensing replay: {e}")))?;
        }

        // Evict through the one blessed path (grounding, outboard,
        // protections all enforced there).
        match self
            .evict(db, &candidate.hash)
            .map_err(|e| SwapSkip::Other(e.to_string()))?
        {
            EvictOutcome::Evicted { bytes_reclaimed } => {
                report.swapped += 1;
                report.bytes_reclaimed += bytes_reclaimed;
                Ok(())
            }
            EvictOutcome::Blocked(why) => Err(SwapSkip::Other(format!(
                "container eviction blocked: {why:?}"
            ))),
        }
    }
}

enum SwapSkip {
    BelowThreshold,
    Other(String),
}

// The empty-blob edge: zero-length pieces are grounded by the empty
// literal at decomposition time and arrive Resident, so they never
// reach `to_pack` — asserted by the e2e rather than special-cased here.
