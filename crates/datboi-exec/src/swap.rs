//! The D91 affine piece-swap: pieces over container, one sealed pack
//! per decomposition. This is a MAINTENANCE PHASE (plan-time SQL, runs
//! alongside license/mark/evict), never an analyzer — D47's split
//! stays intact and the sweep queues never hear about it.
//!
//! Per candidate (a resident container with an affine builtin-assemble
//! rebuild route), in order:
//!
//! 0. **Leaves** (D116): the swap packs the route graph's GROUNDING
//!    LEAVES, not the candidate's direct inputs. An absent input with a
//!    DOWNWARD route — a non-failed route that is not a view (no
//!    non-generated input at least as large as its output, D112's
//!    test) — is an intermediate: it is never packed, its downward
//!    route is walked instead, and the route is licensed before the
//!    container evicts. An absent input with no downward route is a
//!    leaf. On a one-level decomposition (NDS, Xbox, GameCube) every
//!    input is a leaf and nothing changes; on a Wii disc the encrypted
//!    body (rebuilt by `encrypt`) and the plaintext (rebuilt by an
//!    assemble) are intermediates, and the leaves are the files.
//! 1. **Predicate** (never eager, D112): the bytes the swap RECLAIMS —
//!    the container's size minus what would have to be packed (absent,
//!    single-claimed, non-generated leaves) — must clear the molten
//!    floor (`swap:reclaim-min-bytes`, 4 MiB). Resident pieces, pieces
//!    claimed by ≥2 decompositions (D91's pair-breaking heuristic: the
//!    first variant's pack is the second's sharing), GENERATED inputs
//!    (a zero-input route — the D111 filler stream; never packed), and
//!    fill bytes all count as reclaim. A lone ROM whose only saving is
//!    a few KiB of pad never trips it; anything at disc scale does.
//! 2. **Headroom** (D56): absent piece bytes + slack must fit before
//!    anything is written — the swap is transiently double-resident by
//!    design.
//! 3. **Pack**: absent leaves stream through the executor (their
//!    derive routes ground in the still-resident container) into ONE
//!    sealed pack, walk order, every member verified on the way in.
//!    Residency flips to Resident per member after the pack publishes
//!    (bytes first, rows second — recovery's direction).
//! 4. **License**: every intermediate's downward route is licensed
//!    bottom-up WITHOUT materializing it ([`Executor::license`] — a
//!    replay would write disc-sized intermediates into the store),
//!    then the rebuild route replays if it hasn't (D25 — the drop
//!    needs ReplayedLocal, not just Verified).
//! 5. **Evict** the container through the ordinary planner path (D21
//!    grounding counterfactual, D49 outboard, D27 protections). The
//!    caller holds the D72 singleton guard across this step.
//!
//! Crash safety is compositional: a pack without residency rows is
//! bytes-are-truth (re-swap re-packs the identical member set to the
//! identical pack hash — an idempotent rename); a licensed-but-not-
//! evicted container is just an eviction candidate; nothing here has a
//! state the next ambient cycle can't finish or redo.

use std::collections::HashSet;

use datboi_core::hash::Blake3;
use datboi_index::{Db, RebuildInput, Residency, SwapCandidate, VerifyState};
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
        let reclaim_min = policy::swap_reclaim_min_bytes(db)?;
        for candidate in db.swap_candidates()? {
            match self.swap_one(db, &candidate, reclaim_min, &mut report) {
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
        reclaim_min: u64,
        report: &mut SwapReport,
    ) -> Result<(), SwapSkip> {
        let Some(container_size) = candidate.size else {
            return Err(SwapSkip::Other("container has no recorded size".into()));
        };
        // The grounding leaves under the route (D116), and the
        // intermediates' routes to license, bottom-up.
        let leaves = collect_leaves(db, candidate.recipe_id).map_err(SwapSkip::Other)?;
        if leaves.direct_inputs == 0 {
            return Err(SwapSkip::Other("rebuild route has no inputs".into()));
        }
        let inputs = leaves.inputs;
        // What must be written: absent, single-claimed, non-generated
        // leaves (deduped by hash). Everything else in the container is
        // reclaim — resident or shared pieces, generated streams, fills,
        // and every intermediate.
        let mut must_pack = 0u64;
        let mut packable = 0u64;
        let mut seen_hash = std::collections::HashSet::new();
        for input in &inputs {
            let Some(size) = input.size else {
                return Err(SwapSkip::Other(format!(
                    "piece {} has no recorded size",
                    input.hash
                )));
            };
            if input.generated || !seen_hash.insert(input.hash) {
                continue;
            }
            if input.residency != Residency::Resident {
                packable += size;
                if input.covering_claims < 2 {
                    must_pack += size;
                }
            }
        }
        // The predicate — never eager (D91/D112). Nothing to pack means
        // nothing to gain (an all-literal or all-generated rebuild).
        let reclaim = container_size.saturating_sub(must_pack);
        if packable == 0 || reclaim < reclaim_min {
            return Err(SwapSkip::BelowThreshold);
        }

        // Absent pieces to materialize, deduped, coverage order. A
        // generated input is never packed: its route regenerates it.
        let mut seen = std::collections::HashSet::new();
        let mut to_pack: Vec<PackMember> = Vec::new();
        let mut piece_ids: Vec<(i64, Blake3)> = Vec::new();
        for input in &inputs {
            if input.residency == Residency::Resident || input.generated || !seen.insert(input.hash)
            {
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
            // pack is durable. Nothing to bless: the pack carries every
            // member's outboard by construction (D105 — the tree is a
            // byproduct of put_pack's own verification), so the evicted
            // container's first served range verifies with no lazy
            // stall and no loose sidecars.
            for (blob_id, _) in &piece_ids {
                db.set_residency(*blob_id, Residency::Resident)
                    .map_err(|e| SwapSkip::Other(e.to_string()))?;
            }
        }

        // License every intermediate's downward route, bottom-up and
        // without materializing (D116), then the rebuild itself if
        // Verified-only (D25).
        for route in &leaves.routes {
            let row = db
                .recipe_by_id(*route)
                .map_err(|e| SwapSkip::Other(e.to_string()))?;
            if row.verify != VerifyState::ReplayedLocal {
                self.license(db, *route)
                    .map_err(|e| SwapSkip::Other(format!("licensing route {route}: {e}")))?;
            }
        }
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

/// The D116 leaf walk's result.
struct Leaves {
    /// Grounding leaves under the route, in walk order (coverage order
    /// of each route, depth first), deduped by hash: what the swap
    /// weighs and packs.
    inputs: Vec<RebuildInput>,
    /// Intermediates' downward routes, post-order — each licensed
    /// before the container evicts.
    routes: Vec<i64>,
    /// The top route's input count (a route with none is not a swap).
    direct_inputs: usize,
}

/// Walk a rebuild route down to its grounding leaves (D116). An absent,
/// non-generated input is an INTERMEDIATE when it has a downward route
/// — a non-failed route of its own that is not a view (D112: no
/// non-generated input at least as large as its output) — and a LEAF
/// otherwise. Resident and generated inputs end the walk where they
/// are (reclaim, nothing to pack or prove); a blob on the current path
/// is never re-entered (a route back into the path is the derive
/// direction, never a way down).
fn collect_leaves(db: &Db, recipe_id: i64) -> Result<Leaves, String> {
    let mut out = Leaves {
        inputs: Vec::new(),
        routes: Vec::new(),
        direct_inputs: 0,
    };
    let mut seen: HashSet<Blake3> = HashSet::new();
    let mut path: Vec<Blake3> = Vec::new();
    walk(db, recipe_id, &mut seen, &mut path, &mut out, true)?;
    Ok(out)
}

fn walk(
    db: &Db,
    recipe_id: i64,
    seen: &mut HashSet<Blake3>,
    path: &mut Vec<Blake3>,
    out: &mut Leaves,
    top: bool,
) -> Result<(), String> {
    let inputs = db.rebuild_inputs(recipe_id).map_err(|e| e.to_string())?;
    if top {
        out.direct_inputs = inputs.len();
    }
    for input in inputs {
        if input.generated || !seen.insert(input.hash) {
            continue;
        }
        if input.residency == Residency::Resident {
            // Reclaim as it stands; the predicate still wants its size.
            out.inputs.push(input);
            continue;
        }
        if path.contains(&input.hash) {
            continue;
        }
        match downward_route(db, &input)? {
            Some(route) => {
                path.push(input.hash);
                walk(db, route, seen, path, out, false)?;
                path.pop();
                out.routes.push(route);
            }
            None => out.inputs.push(input),
        }
    }
    Ok(())
}

/// The first non-failed route of `input` that rebuilds it from smaller
/// parts — `None` when every route is a view (a slice of a whole, a
/// decrypt of a larger ciphertext) or there is none: a grounding leaf.
fn downward_route(db: &Db, input: &RebuildInput) -> Result<Option<i64>, String> {
    let Some(size) = input.size else {
        return Ok(None);
    };
    for route in db
        .recipes_for_output(input.blob_id)
        .map_err(|e| e.to_string())?
    {
        if route.verify == VerifyState::Failed {
            continue;
        }
        let ins = db
            .rebuild_inputs(route.recipe_id)
            .map_err(|e| e.to_string())?;
        if ins.is_empty() {
            continue;
        }
        let view = ins
            .iter()
            .any(|i| !i.generated && i.size.is_some_and(|s| s >= size));
        if !view {
            return Ok(Some(route.recipe_id));
        }
    }
    Ok(None)
}

#[derive(Debug, Default)]
pub struct ChunkPackReport {
    /// Chunk sets whose loose pieces were consolidated into a pack.
    pub sets_packed: usize,
    /// Pieces moved from loose files into packs.
    pub members: usize,
    /// Bytes consolidated.
    pub bytes_packed: u64,
    /// Redundant loose copies swept from a prior interrupted run.
    pub swept_loose: usize,
    /// (output, why) — sets skipped (headroom, missing sizes).
    pub skipped: Vec<(Blake3, String)>,
}

impl<'s> Executor<'s> {
    /// Pack-per-chunking (D91's named follow-on / D59's small-blob
    /// flood): consolidate a chunk set's LOOSE grounding-leaf pieces into
    /// one sealed pack, the same inode win the swap buys for
    /// decomposition pieces. Unlike the swap, chunks are born RESIDENT
    /// (the analyzer writes them loose), so there is nothing to
    /// materialize — the pieces stream out of their own loose files into
    /// the pack, which carries their outboards by construction (D105),
    /// and BOTH redundant loose files drop (`.data` and `.obao4`) —
    /// verified serving reads the tree out of the pack's section.
    /// A piece shared across sets packs with the FIRST set (first-packer-
    /// wins, like the swap); the rest see it already packed and skip it,
    /// so cross-set dedup is preserved — the global pack map resolves it
    /// for every consumer. Runs as a maintenance phase under the caller's
    /// D72 guard.
    ///
    /// # Errors
    /// Index/store failures abort; per-set problems land in the report.
    pub fn pack_chunk_sets(&self, db: &mut Db) -> Result<ChunkPackReport, ExecError> {
        let mut report = ChunkPackReport::default();
        if !policy::chunk_pack_enabled(db)? {
            return Ok(report);
        }
        let min_members = policy::chunk_pack_min_members(db)?;
        // swap_candidates is exactly the population: affine builtin-
        // assemble routes in trusted verify state. A chunk set's original
        // is one; its inputs are the chunks.
        for candidate in db.swap_candidates()? {
            match self.pack_one_set(db, candidate.recipe_id, min_members, &mut report) {
                Ok(()) | Err(SwapSkip::BelowThreshold) => {}
                Err(SwapSkip::Other(why)) => report.skipped.push((candidate.hash, why)),
            }
        }
        Ok(report)
    }

    fn pack_one_set(
        &self,
        db: &mut Db,
        recipe_id: i64,
        min_members: usize,
        report: &mut ChunkPackReport,
    ) -> Result<(), SwapSkip> {
        let inputs = db
            .rebuild_inputs(recipe_id)
            .map_err(|e| SwapSkip::Other(e.to_string()))?;
        let mut seen = std::collections::HashSet::new();
        let mut to_pack: Vec<PackMember> = Vec::new();
        for input in &inputs {
            if input.residency != Residency::Resident {
                continue;
            }
            if self.store_ref().is_packed(&input.hash) {
                // Already packed. Self-heal an interrupted prior run that
                // packed the bytes but never dropped the loose copies
                // (data + sidecar — the pack carries the tree, D105).
                if self.store_ref().has_loose(StoreNs::Data, &input.hash)
                    && self
                        .store_ref()
                        .remove_blob(StoreNs::Data, &input.hash)
                        .map_err(|e| SwapSkip::Other(e.to_string()))?
                {
                    report.swept_loose += 1;
                }
                continue;
            }
            if !self.store_ref().has_loose(StoreNs::Data, &input.hash) {
                continue;
            }
            let Some(len) = input.size else { continue };
            if seen.insert(input.hash) {
                to_pack.push(PackMember {
                    hash: input.hash,
                    len,
                });
            }
        }
        if to_pack.len() < min_members {
            return Err(SwapSkip::BelowThreshold);
        }
        // The pack is a transient second copy until the loose files drop.
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
        // Stream every loose piece out of its own file into one pack.
        self.store_ref()
            .put_pack(&to_pack, |ix| {
                let reader = self
                    .store_ref()
                    .get(StoreNs::Data, &to_pack[ix].hash)
                    .map_err(std::io::Error::other)?
                    .ok_or_else(|| std::io::Error::other("loose piece vanished mid-pack"))?;
                Ok(Box::new(reader))
            })
            .map_err(|e| SwapSkip::Other(format!("pack write: {e}")))?;
        report.sets_packed += 1;
        // The pack carries every member's outboard (D105), so both loose
        // files are redundant now — drop `.data` AND `.obao4`, completing
        // the inode win. Residency stays Resident — a packed piece is
        // still resident.
        for member in &to_pack {
            self.store_ref()
                .remove_blob(StoreNs::Data, &member.hash)
                .map_err(|e| SwapSkip::Other(format!("loose drop: {e}")))?;
            report.members += 1;
            report.bytes_packed += member.len;
        }
        Ok(())
    }
}

// The empty-blob edge: zero-length pieces are grounded by the empty
// literal at decomposition time and arrive Resident, so they never
// reach `to_pack` — asserted by the e2e rather than special-cased here.
