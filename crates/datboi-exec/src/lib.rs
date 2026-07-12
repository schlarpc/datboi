//! Streaming recipe executor (docs/70-recipes.md Execution, D25/D46/D51).
//!
//! The executor turns "I want the bytes of X" into a pull-based operator
//! tree over resident literals: O(chunk) memory per node, spill to a
//! bounded temp file only where random access is demanded of a
//! non-seekable node (the spill rule). Two consumption modes:
//!
//! - [`Executor::replay`] — execute one recipe and materialize ALL its
//!   claimed outputs into the store, verifying every hash and building
//!   bao outboards in the same pass ([`Store::put_with_obao`]). Success
//!   advances the recipe to `ReplayedLocal` — the D25 licensing event
//!   that permits literal drops. Claim-level failures (wrong bytes,
//!   guest trap, guest error) poison the recipe to `Failed`;
//!   infrastructure failures (missing inputs, I/O) leave state alone.
//! - [`Executor::open_stream`] — a sequential reader over a route,
//!   storing nothing (serving surfaces, spills, analyzers).
//!
//! Wasm composition (the D51 accepted cost): each streaming guest runs on
//! its own thread, connected through bounded [`pipe`]s; backpressure is
//! thread suspension the guest cannot observe. Output bytes are a pure
//! function of input bytes for every node kind, so scheduling cannot
//! affect results — determinism needs no cooperative scheduler.

pub mod evict;
pub mod policy;
pub mod random;

pub use datboi_runtime::pipe;

use std::collections::HashMap;
use std::io::{self, Cursor, Read, Seek, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use datboi_core::assemble::AssembleParams;
use datboi_core::hash::Blake3;
use datboi_core::params::{DeflateWindow, ExtractorParams};
use datboi_core::recipe::{Op, Recipe, World};
use datboi_index::{
    Db, IndexError, RecipeSource, Residency, SeekClass, VerifyAdvance, VerifyState,
};
use datboi_runtime::extractor::{ExtractorComponent, ExtractorHost};
use datboi_runtime::stream::{
    RangeRead, SequentialInput, StreamHost, StreamInput, StreamTransform,
};
use datboi_runtime::{Limits, RuntimeError, TransformHost};
use datboi_store_fs::{Namespace as StoreNs, PutOutcome, Store, StoreError};

use crate::random::{AssembleRandom, FileRandom, SeqOverRandom, VerifiedRandom, WindowSeq};

/// D56 headroom guard: fixed safety margin on top of the claimed size
/// (+ obao overhead) before materialize-on-demand may write.
const MATERIALIZE_SLACK: u64 = 64 << 20;

#[derive(Debug, thiserror::Error)]
pub enum ExecError {
    #[error("no materializable route to {0}")]
    NoRoute(Blake3),
    #[error("recipe {0} is poisoned (Failed); it will not be re-run")]
    Poisoned(i64),
    #[error("operator tree exceeds depth limit")]
    Depth,
    #[error("recipe cycle detected at {0}")]
    Cycle(Blake3),
    #[error("unsupported op: {0}")]
    UnsupportedOp(String),
    #[error("invalid recipe structure: {0}")]
    Malformed(String),
    #[error("wasm input exceeds whole-buffer cap ({size} > {cap})")]
    BufferCap { size: u64, cap: u64 },
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Index(#[from] IndexError),
    #[error(transparent)]
    Runtime(#[from] RuntimeError),
    #[error("i/o during execution: {0}")]
    Io(#[from] io::Error),
    #[error("no outboard for {0}: recipe-served range reads require one (D49)")]
    MissingOutboard(Blake3),
    #[error(
        "insufficient disk headroom to materialize {hash}: need ~{need} bytes, {have} available (D56)"
    )]
    InsufficientHeadroom { hash: Blake3, need: u64, have: u64 },
    #[error("range verification failed for {hash}: {detail}")]
    RangeVerifyFailed { hash: Blake3, detail: String },
}

impl ExecError {
    /// Does this failure indict the *claim* (recipe poisoned, D25) rather
    /// than the environment (retryable)? Fuel exhaustion is neither —
    /// it's a policy outcome (budget too small), so it stays retryable:
    /// poisoning would make a fuel-policy retune unable to rescue the
    /// recipe.
    #[must_use]
    pub fn is_claim_failure(&self) -> bool {
        match self {
            Self::Store(StoreError::HashMismatch { .. }) | Self::Malformed(_) => true,
            Self::Runtime(e @ RuntimeError::Trap(_)) => !e.is_fuel_exhaustion(),
            Self::Runtime(RuntimeError::Transform(_)) => true,
            // Instantiation/link failures are host wiring (a linker
            // regression, a component/world mismatch), not disproof of
            // the data claim — retryable by construction.
            Self::Runtime(RuntimeError::Instantiate(_)) => false,
            // A sequential input ending short of its declared length
            // indicts the CHILD claim that fed it, never the recipe
            // under replay.
            Self::Runtime(RuntimeError::InputLengthMismatch { .. }) => false,
            _ => false,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ExecConfig {
    /// Per-run wasm resource ceilings.
    pub limits: Limits,
    /// Whole-buffer cap for @1 inputs/outputs (the profile buffers whole
    /// blobs by design — D41; anything bigger belongs in @2).
    pub max_buffer: u64,
    /// Operator-tree depth guard (docs/70-recipes.md safety).
    pub max_depth: usize,
    /// Where spill files live; defaults to the OS temp dir.
    pub spill_dir: Option<PathBuf>,
}

impl Default for ExecConfig {
    fn default() -> Self {
        Self {
            limits: Limits::default(),
            max_buffer: 256 << 20,
            max_depth: 1024,
            spill_dir: None,
        }
    }
}

/// Report of one recipe replay.
#[derive(Debug)]
pub struct ReplayReport {
    pub recipe_id: i64,
    /// (output hash, whether this replay published it or it already
    /// existed) in recipe order.
    pub outputs: Vec<(Blake3, PutOutcome)>,
}

/// A resolved, executable route to one output. Holds data only (no DB
/// borrows), so readers built from it are `'static + Send`.
enum Plan {
    Literal { hash: Blake3, len: u64 },
    Op(Box<OpPlan>),
}

struct OpPlan {
    op: OpImpl,
    /// Which claimed output this plan produces.
    output_ix: usize,
    outputs: Vec<(Blake3, u64)>,
    children: Vec<Plan>,
    /// The recipe row this node executes (None for synthetic top-level
    /// nodes built inside `execute_to_store`, whose id the caller holds).
    /// A successful parent replay licenses these children too (D25): they
    /// ran on this host and the parent's claim check transitively pinned
    /// their outputs.
    recipe_id: Option<i64>,
}

impl Plan {
    fn collect_recipe_ids(&self, out: &mut Vec<i64>) {
        if let Self::Op(op) = self {
            if let Some(id) = op.recipe_id {
                out.push(id);
            }
            for child in &op.children {
                child.collect_recipe_ids(out);
            }
        }
    }
}

enum OpImpl {
    Assemble(AssembleParams),
    /// deflate window over input 0.
    Deflate {
        offset: u64,
        len: u64,
    },
    Wasm2 {
        transform: Arc<StreamTransform>,
        component: Blake3,
        op: String,
        params: Vec<u8>,
        seek: datboi_runtime::SeekClass,
        random_access_inputs: Vec<u32>,
    },
    Wasm1 {
        component: Vec<u8>,
        op: String,
        params: Vec<u8>,
    },
    /// Container→member extraction through a `datboi:extractor@1` component
    /// (D58). Input 0 is the archive container (random-access); the single
    /// output is member `member_ix` (opaque — the whole member decodes as a
    /// unit, so range reads spill).
    Extractor {
        component: Arc<ExtractorComponent>,
        member_ix: u32,
    },
}

impl Plan {
    fn len(&self) -> u64 {
        match self {
            Self::Literal { len, .. } => *len,
            Self::Op(op) => op.outputs[op.output_ix].1,
        }
    }
}

pub struct Executor<'s> {
    store: &'s Store,
    stream_host: Arc<StreamHost>,
    v1_host: TransformHost,
    extractor_host: Arc<ExtractorHost>,
    config: ExecConfig,
    /// Compiled @2 components by hash — the D51 load/run split.
    components: Mutex<HashMap<Blake3, Arc<StreamTransform>>>,
    /// Compiled extractor components by hash (same load/run split).
    extractor_components: Mutex<HashMap<Blake3, Arc<ExtractorComponent>>>,
}

impl<'s> Executor<'s> {
    /// # Errors
    /// If wasmtime rejects the deterministic configuration.
    pub fn new(store: &'s Store, config: ExecConfig) -> Result<Self, ExecError> {
        Ok(Self {
            store,
            stream_host: Arc::new(StreamHost::new(config.limits)?),
            v1_host: TransformHost::new(config.limits)?,
            extractor_host: Arc::new(ExtractorHost::new(config.limits)?),
            config,
            components: Mutex::new(HashMap::new()),
            extractor_components: Mutex::new(HashMap::new()),
        })
    }

    // ---- public entry points ----

    /// Replay one recipe end to end (the D25 licensing pass): all claimed
    /// outputs are materialized into the store with outboards and their
    /// hashes verified. Advances the verify state on success; poisons the
    /// recipe on claim-level failure.
    ///
    /// # Errors
    /// Claim failures, missing inputs ([`ExecError::NoRoute`]), or I/O.
    pub fn replay(&self, db: &Db, recipe_id: i64) -> Result<ReplayReport, ExecError> {
        let row = db.recipe_by_id(recipe_id)?;
        if row.verify == VerifyState::Failed {
            return Err(ExecError::Poisoned(recipe_id));
        }
        let recipe = self.load_recipe(db, recipe_id)?;
        let mut participants = Vec::new();
        let result = self.execute_to_store(db, &recipe, &mut participants);
        match result {
            Ok(outputs) => {
                if row.verify != VerifyState::ReplayedLocal {
                    db.set_verify_state(recipe_id, VerifyAdvance::ReplayedLocal, now_unix())?;
                }
                // License the recipes that executed inside this replay
                // (D25): each ran on this host, and the top-level claim
                // check transitively verified their outputs — a child
                // producing wrong bytes cannot yield the parent's hash.
                for child_id in participants {
                    if child_id == recipe_id {
                        continue;
                    }
                    let child = db.recipe_by_id(child_id)?;
                    if child.verify == VerifyState::Verified {
                        db.set_verify_state(child_id, VerifyAdvance::ReplayedLocal, now_unix())?;
                    }
                }
                for (output, (hash, _)) in recipe.outputs.iter().zip(&outputs) {
                    let id = db.upsert_blob(
                        hash,
                        Some(output.size),
                        datboi_index::Namespace::Data,
                        Residency::Resident,
                    )?;
                    db.set_verified(id, now_unix())?;
                }
                Ok(ReplayReport { recipe_id, outputs })
            }
            Err(e) => {
                if e.is_claim_failure() {
                    db.set_verify_state(
                        recipe_id,
                        VerifyAdvance::Failed {
                            error: &e.to_string(),
                            peer: None,
                        },
                        now_unix(),
                    )?;
                }
                Err(e)
            }
        }
    }

    /// Attempt to rehabilitate one poisoned recipe: re-execute it, and
    /// if every claimed output verifies, clear the poison to
    /// `ReplayedLocal`. The escape hatch for wrong poisonings (host
    /// bugs, since-repaired environments) — `Failed` stays terminal for
    /// every other path. A failing re-execution leaves the row Failed.
    ///
    /// # Errors
    /// Execution failures (the recipe stays poisoned); index/store I/O.
    pub fn rehabilitate(&self, db: &Db, recipe_id: i64) -> Result<ReplayReport, ExecError> {
        let row = db.recipe_by_id(recipe_id)?;
        if row.verify != VerifyState::Failed {
            // Nothing to rehabilitate; treat as an ordinary replay.
            return self.replay(db, recipe_id);
        }
        let recipe = self.load_recipe(db, recipe_id)?;
        let mut participants = Vec::new();
        let outputs = self.execute_to_store(db, &recipe, &mut participants)?;
        db.rehabilitate_recipe(recipe_id, now_unix())?;
        for child_id in participants {
            if child_id == recipe_id {
                continue;
            }
            let child = db.recipe_by_id(child_id)?;
            if child.verify == VerifyState::Verified {
                db.set_verify_state(child_id, VerifyAdvance::ReplayedLocal, now_unix())?;
            }
        }
        for (output, (hash, _)) in recipe.outputs.iter().zip(&outputs) {
            let id = db.upsert_blob(
                hash,
                Some(output.size),
                datboi_index::Namespace::Data,
                Residency::Resident,
            )?;
            db.set_verified(id, now_unix())?;
        }
        Ok(ReplayReport { recipe_id, outputs })
    }

    /// Materialize `hash` into the store if it isn't already resident:
    /// picks a route and replays the covering recipe (which stores every
    /// sibling output too — one execution verifies all, docs/70-recipes.md).
    ///
    /// Guarded by the D56 disk-headroom check: materialize-on-demand
    /// refuses cleanly (nothing written) when the store filesystem
    /// lacks room for the claimed bytes + outboard + slack, instead of
    /// hitting ENOSPC mid-replay.
    ///
    /// # Errors
    /// [`ExecError::NoRoute`] when no non-poisoned recipe chain grounds
    /// out in resident literals; [`ExecError::InsufficientHeadroom`]
    /// when the guard refuses.
    pub fn materialize(&self, db: &Db, hash: &Blake3) -> Result<(), ExecError> {
        if self.is_resident(db, hash)? {
            return Ok(());
        }
        let Some(row) = db.blob_by_hash(hash)? else {
            return Err(ExecError::NoRoute(*hash));
        };
        if let (Some(size), Some(have)) = (row.size, self.store.available_bytes()?) {
            // Claimed bytes + obao (~size/256) + a fixed safety margin;
            // a replay may also store sibling outputs, which the margin
            // absorbs for realistic recipes.
            let need = size
                .saturating_add(size / 256)
                .saturating_add(MATERIALIZE_SLACK);
            if have < need {
                return Err(ExecError::InsufficientHeadroom {
                    hash: *hash,
                    need,
                    have,
                });
            }
        }
        let mut last_err = None;
        for recipe in db.recipes_for_output(row.blob_id)? {
            if recipe.verify == VerifyState::Failed {
                continue;
            }
            match self.replay(db, recipe.recipe_id) {
                Ok(_) => return Ok(()),
                Err(e) => last_err = Some(e),
            }
        }
        Err(last_err.unwrap_or(ExecError::NoRoute(*hash)))
    }

    /// A sequential verified-source reader over the best route to `hash`,
    /// storing nothing. (Range serving with D49 output-bao verification
    /// is `serve_range`.)
    ///
    /// # Errors
    /// [`ExecError::NoRoute`] when nothing resolves.
    pub fn open_stream(&self, db: &Db, hash: &Blake3) -> Result<Box<dyn Read + Send>, ExecError> {
        let plan = self.plan(db, hash, 0, &mut Vec::new())?;
        self.open_sequential(&plan)
    }

    /// Serve `offset..offset+len` (clamped) of `hash` — the D49 range
    /// path. Resident literals with an outboard get verified reads;
    /// recipe-served ranges are ALWAYS verified against the output's
    /// outboard, whatever produced them:
    ///
    /// - affine builtin routes translate arithmetically (no
    ///   materialization);
    /// - declared-seekable, non-quarantined wasm components serve through
    ///   `serve-range`;
    /// - everything else (opaque routes, quarantined components) spills
    ///   through the known-good sequential path first.
    ///
    /// A verification mismatch never returns bytes. If the producer was a
    /// wasm seek path, the component's seekability is quarantined (rule
    /// 3) so the next read takes the sequential route.
    ///
    /// Routes without a sidecar take the **D63 affine carve-out** when
    /// they qualify (locally-minted + pure-builtin assemble + affine +
    /// verified inputs, [`Self::affine_carveout`]): every served byte is
    /// then verified input bytes (each leaf re-validated against its own
    /// bao tree) or executor-generated fill. When a sidecar exists it is
    /// always preferred — the carve-out is a floor, not a ceiling.
    ///
    /// # Errors
    /// [`ExecError::MissingOutboard`] when no sidecar exists and the
    /// carve-out does not apply,
    /// [`ExecError::RangeVerifyFailed`] (the EIO class) on mismatch.
    pub fn serve_range(
        &self,
        db: &Db,
        hash: &Blake3,
        offset: u64,
        len: u64,
    ) -> Result<Vec<u8>, ExecError> {
        use datboi_store_fs::obao;

        // Resident literal: verified read when the tree exists, plain
        // read otherwise (D4's cheap default governs literals; D49's
        // mandate is about recipe-served bytes).
        if self.is_resident(db, hash)? {
            if self.store.get_obao(StoreNs::Data, hash)?.is_some() {
                return Ok(self
                    .store
                    .read_range_verified(StoreNs::Data, hash, offset, len)?);
            }
            let file = self
                .store
                .get(StoreNs::Data, hash)?
                .ok_or(ExecError::NoRoute(*hash))?;
            let mut src = FileRandom::new(file)?;
            let total = src.len();
            let start = offset.min(total);
            let end = offset.saturating_add(len).min(total);
            let mut buf = vec![0u8; usize::try_from(end - start).expect("range fits memory")];
            random::read_at_exact(&mut src, start, &mut buf)?;
            return Ok(buf);
        }

        let row = db.blob_by_hash(hash)?.ok_or(ExecError::NoRoute(*hash))?;
        let total = row
            .size
            .ok_or_else(|| ExecError::Malformed("blob size unknown".into()))?;
        let start = offset.min(total);
        let end = offset.saturating_add(len).min(total);
        let plan = self.plan(db, hash, 0, &mut Vec::new())?;

        // Evicted small blobs (≤ one chunk group) have an empty outboard
        // by construction; the store can only infer that from a resident
        // file, so decide from the indexed size here.
        let sidecar = if obao::outboard_size(total) == 0 {
            Some(Vec::new())
        } else {
            self.store.get_obao(StoreNs::Data, hash)?
        };
        let Some(sidecar) = sidecar else {
            if self.affine_carveout(db, &plan)? {
                let mut src = self.open_random_verified(&plan)?;
                let mut buf = vec![0u8; usize::try_from(end - start).expect("range fits memory")];
                random::read_at_exact(src.as_mut(), start, &mut buf).map_err(|e| {
                    ExecError::RangeVerifyFailed {
                        hash: *hash,
                        detail: format!("affine carve-out (D63): {e}"),
                    }
                })?;
                return Ok(buf);
            }
            return Err(ExecError::MissingOutboard(*hash));
        };
        // Group-aligned window: bao validates whole 16 KiB groups.
        let astart = start - start % obao::GROUP_BYTES;
        let aend = end
            .checked_next_multiple_of(obao::GROUP_BYTES)
            .unwrap_or(u64::MAX)
            .min(total);
        let (window, via_component) = self.produce_range(db, &plan, astart, aend)?;
        if window.len() as u64 != aend - astart {
            let detail = format!(
                "producer yielded {} bytes for a {}-byte window",
                window.len(),
                aend - astart
            );
            let detail = self.attribute_seek_failure(db, &plan, via_component, &detail)?;
            return Err(ExecError::RangeVerifyFailed {
                hash: *hash,
                detail,
            });
        }
        match obao::verify_window(total, hash, &sidecar, astart, &window) {
            Ok(()) => {
                let lo = usize::try_from(start - astart).expect("window fits memory");
                let n = usize::try_from(end - start).expect("window fits memory");
                Ok(window[lo..lo + n].to_vec())
            }
            Err(e) => {
                let detail =
                    self.attribute_seek_failure(db, &plan, via_component, &e.to_string())?;
                Err(ExecError::RangeVerifyFailed {
                    hash: *hash,
                    detail,
                })
            }
        }
    }

    /// Attribution for a window-verify failure (D49 rule 3, refined): a
    /// mismatch through a component's seek path only indicts the
    /// component when its route inputs are CLEAN — so re-hash every
    /// literal grounding the implicated node before writing the
    /// quarantine row. Corrupt inputs would fail any producer; defaming
    /// the component for them would silently degrade every future read
    /// to the spill path. Returns the (possibly annotated) detail.
    fn attribute_seek_failure(
        &self,
        db: &Db,
        plan: &Plan,
        via_component: Option<Blake3>,
        detail: &str,
    ) -> Result<String, ExecError> {
        let Some(component) = via_component else {
            return Ok(detail.to_string());
        };
        let mut dirty: Vec<Blake3> = Vec::new();
        if let Some(children) = find_wasm2_children(plan, &component) {
            let mut leaves = Vec::new();
            for child in children {
                collect_literal_leaves(child, &mut leaves);
            }
            for (leaf_hash, _) in leaves {
                if !self.literal_matches_address(&leaf_hash)? {
                    dirty.push(leaf_hash);
                }
            }
        }
        if dirty.is_empty() {
            // Inputs clean: the seek path itself lied. Quarantine (the
            // claim stays trusted — sequential replay proved it).
            db.quarantine_seek(&component, now_unix(), detail)?;
            Ok(detail.to_string())
        } else {
            let dirty_list = dirty
                .iter()
                .map(Blake3::to_hex)
                .collect::<Vec<_>>()
                .join(", ");
            Ok(format!(
                "{detail}; corrupt route input(s) [{dirty_list}] — component not quarantined"
            ))
        }
    }

    /// Slow, certain integrity check reserved for the mismatch path:
    /// stream the resident literal and compare against its address.
    fn literal_matches_address(&self, hash: &Blake3) -> Result<bool, ExecError> {
        let Some(mut file) = self.store.get(StoreNs::Data, hash)? else {
            return Ok(false); // vanished mid-read: certainly not clean
        };
        let mut hasher = blake3::Hasher::new();
        io::copy(&mut file, &mut hasher)?;
        Ok(Blake3(*hasher.finalize().as_bytes()) == *hash)
    }

    /// Produce blob bytes `astart..aend` from a plan, reporting which
    /// wasm component's seek path produced them (None = sequential /
    /// arithmetic route).
    fn produce_range(
        &self,
        db: &Db,
        plan: &Plan,
        astart: u64,
        aend: u64,
    ) -> Result<(Vec<u8>, Option<Blake3>), ExecError> {
        let read_via = |src: &mut dyn RangeRead| -> Result<Vec<u8>, ExecError> {
            let mut buf = vec![0u8; usize::try_from(aend - astart).expect("window fits memory")];
            random::read_at_exact(src, astart, &mut buf)?;
            Ok(buf)
        };
        if let Plan::Op(op_plan) = plan {
            match &op_plan.op {
                OpImpl::Assemble(params) => {
                    let children = self.open_children_random(&op_plan.children)?;
                    let mut node = AssembleRandom::new(params.clone(), children)
                        .map_err(ExecError::Malformed)?;
                    return Ok((read_via(&mut node)?, None));
                }
                OpImpl::Wasm2 {
                    transform,
                    component,
                    op,
                    params,
                    seek,
                    ..
                } if *seek != datboi_runtime::SeekClass::Opaque
                    && !db.is_seek_quarantined(component)? =>
                {
                    // serve-range contract: ALL inputs random-access.
                    let mut inputs: Vec<Box<dyn RangeRead>> =
                        Vec::with_capacity(op_plan.children.len());
                    for child in &op_plan.children {
                        inputs.push(self.open_random(child)?);
                    }
                    let sink = VecSink::default();
                    self.stream_host.serve_range_fueled(
                        transform,
                        op,
                        params,
                        inputs,
                        datboi_runtime::stream::RangeRequest {
                            output_ix: u32::try_from(op_plan.output_ix)
                                .expect("output count fits u32"),
                            offset: astart,
                            len: aend - astart,
                        },
                        Box::new(sink.clone()),
                        Some(fuel_budget(&op_plan.children, &op_plan.outputs)),
                    )?;
                    // Length is checked by the caller before verification
                    // (a short window is a seek-path failure too).
                    return Ok((sink.take(), Some(*component)));
                }
                _ => {}
            }
        }
        // Sequential fallback: spill, then window out of the spill.
        let mut spilled = self.spill(plan)?;
        Ok((read_via(spilled.as_mut())?, None))
    }

    // ---- planning ----

    fn is_resident(&self, db: &Db, hash: &Blake3) -> Result<bool, ExecError> {
        Ok(match db.blob_by_hash(hash)? {
            Some(row) => {
                row.residency == Residency::Resident && self.store.has(StoreNs::Data, hash)
            }
            // Not indexed: the store may still hold it (recovery windows).
            None => self.store.has(StoreNs::Data, hash),
        })
    }

    fn plan(
        &self,
        db: &Db,
        hash: &Blake3,
        depth: usize,
        visiting: &mut Vec<Blake3>,
    ) -> Result<Plan, ExecError> {
        if depth > self.config.max_depth {
            return Err(ExecError::Depth);
        }
        if visiting.contains(hash) {
            return Err(ExecError::Cycle(*hash));
        }
        if let Some(len) = self.store.len(StoreNs::Data, hash)? {
            return Ok(Plan::Literal { hash: *hash, len });
        }
        let Some(row) = db.blob_by_hash(hash)? else {
            return Err(ExecError::NoRoute(*hash));
        };
        visiting.push(*hash);
        let mut last_err = None;
        for recipe_row in db.recipes_for_output(row.blob_id)? {
            if recipe_row.verify == VerifyState::Failed {
                continue;
            }
            match self.plan_recipe(db, recipe_row.recipe_id, hash, depth, visiting) {
                Ok(plan) => {
                    visiting.pop();
                    return Ok(plan);
                }
                Err(e) => last_err = Some(e),
            }
        }
        visiting.pop();
        Err(last_err.unwrap_or(ExecError::NoRoute(*hash)))
    }

    fn plan_recipe(
        &self,
        db: &Db,
        recipe_id: i64,
        target: &Blake3,
        depth: usize,
        visiting: &mut Vec<Blake3>,
    ) -> Result<Plan, ExecError> {
        let recipe = self.load_recipe(db, recipe_id)?;
        let output_ix = recipe
            .outputs
            .iter()
            .position(|o| o.hash == *target)
            .ok_or_else(|| ExecError::Malformed("recipe row does not claim target".into()))?;
        let op = self.resolve_op(&recipe)?;
        let mut children = Vec::with_capacity(recipe.inputs.len());
        for input in &recipe.inputs {
            children.push(self.plan(db, &input.hash, depth + 1, visiting)?);
        }
        Ok(Plan::Op(Box::new(OpPlan {
            op,
            output_ix,
            outputs: recipe.outputs.iter().map(|o| (o.hash, o.size)).collect(),
            children,
            recipe_id: Some(recipe_id),
        })))
    }

    fn load_recipe(&self, db: &Db, recipe_id: i64) -> Result<Recipe, ExecError> {
        let hash = db.recipe_object_hash(recipe_id)?;
        let mut bytes = Vec::new();
        self.store
            .get(StoreNs::Meta, &hash)?
            .ok_or(ExecError::NoRoute(hash))?
            .read_to_end(&mut bytes)?;
        Recipe::decode(&bytes).map_err(|e| ExecError::Malformed(e.to_string()))
    }

    fn resolve_op(&self, recipe: &Recipe) -> Result<OpImpl, ExecError> {
        match &recipe.op {
            Op::Builtin { name, major } => match (name.as_str(), major) {
                ("assemble", 1) => Ok(OpImpl::Assemble(
                    AssembleParams::decode(&recipe.params)
                        .map_err(|e| ExecError::Malformed(e.to_string()))?,
                )),
                ("deflate-decompress", 1) => {
                    let window = DeflateWindow::decode(&recipe.params)
                        .map_err(|e| ExecError::Malformed(e.to_string()))?;
                    Ok(OpImpl::Deflate {
                        offset: window.offset,
                        len: window.len,
                    })
                }
                (name, major) => Err(ExecError::UnsupportedOp(format!("{name}@{major}"))),
            },
            Op::Wasm {
                component,
                world,
                export,
            } => match world {
                World::Transform2 => {
                    let transform = self.load_component(component)?;
                    let descriptor = self.stream_host.describe(&transform, export)?;
                    Ok(OpImpl::Wasm2 {
                        transform,
                        component: *component,
                        op: export.clone(),
                        params: recipe.params.clone(),
                        seek: descriptor.seek,
                        random_access_inputs: descriptor.random_access_inputs,
                    })
                }
                World::Transform1 => {
                    let mut bytes = Vec::new();
                    self.store
                        .get(StoreNs::Data, component)?
                        .ok_or(ExecError::NoRoute(*component))?
                        .read_to_end(&mut bytes)?;
                    Ok(OpImpl::Wasm1 {
                        component: bytes,
                        op: export.clone(),
                        params: recipe.params.clone(),
                    })
                }
                World::Extractor1 => {
                    // The world fixes its one export: any other string is
                    // a skewed recipe identity that would silently run the
                    // same code — refuse it, don't execute it (and don't
                    // poison: the claim was never tested).
                    let required = world
                        .required_export()
                        .expect("extractor world fixes its export");
                    if export != required {
                        return Err(ExecError::UnsupportedOp(format!(
                            "{}#{export} (world exports only {required})",
                            world.as_str()
                        )));
                    }
                    // Params pin the member index.
                    let member_ix = ExtractorParams::decode(&recipe.params)
                        .map_err(|e| ExecError::Malformed(e.to_string()))?
                        .member_ix;
                    let compiled = self.load_extractor_component(component)?;
                    Ok(OpImpl::Extractor {
                        component: compiled,
                        member_ix,
                    })
                }
                // Unknown world (a recipe from the future, a foreign
                // family): refusable, never poisonable — UnsupportedOp
                // is not a claim failure.
                World::Other(w) => Err(ExecError::UnsupportedOp(w.clone())),
            },
        }
    }

    fn load_component(&self, hash: &Blake3) -> Result<Arc<StreamTransform>, ExecError> {
        if let Some(t) = self.components.lock().expect("component cache").get(hash) {
            return Ok(Arc::clone(t));
        }
        let mut bytes = Vec::new();
        self.store
            .get(StoreNs::Data, hash)?
            .ok_or(ExecError::NoRoute(*hash))?
            .read_to_end(&mut bytes)?;
        let compiled = Arc::new(self.stream_host.load(&bytes)?);
        self.components
            .lock()
            .expect("component cache")
            .insert(*hash, Arc::clone(&compiled));
        Ok(compiled)
    }

    fn load_extractor_component(
        &self,
        hash: &Blake3,
    ) -> Result<Arc<ExtractorComponent>, ExecError> {
        if let Some(t) = self
            .extractor_components
            .lock()
            .expect("extractor cache")
            .get(hash)
        {
            return Ok(Arc::clone(t));
        }
        let mut bytes = Vec::new();
        self.store
            .get(StoreNs::Data, hash)?
            .ok_or(ExecError::NoRoute(*hash))?
            .read_to_end(&mut bytes)?;
        let compiled = Arc::new(self.extractor_host.load(&bytes)?);
        self.extractor_components
            .lock()
            .expect("extractor cache")
            .insert(*hash, Arc::clone(&compiled));
        Ok(compiled)
    }

    // ---- opening plans as streams ----

    fn open_sequential(&self, plan: &Plan) -> Result<Box<dyn Read + Send>, ExecError> {
        match plan {
            Plan::Literal { hash, .. } => {
                let file = self
                    .store
                    .get(StoreNs::Data, hash)?
                    .ok_or(ExecError::NoRoute(*hash))?;
                Ok(Box::new(io::BufReader::new(file)))
            }
            Plan::Op(op_plan) => match &op_plan.op {
                OpImpl::Assemble(_) | OpImpl::Deflate { .. } | OpImpl::Wasm1 { .. } => {
                    self.open_builtin_or_v1_sequential(op_plan)
                }
                OpImpl::Extractor {
                    component,
                    member_ix,
                } => {
                    // The container (input 0) arrives random-access — the
                    // extractor seeks its headers. The single output member
                    // streams out through a pipe (opaque: whole member).
                    let container = self.open_random(&op_plan.children[0])?;
                    let (w, r, h) = pipe::pipe();
                    let host = Arc::clone(&self.extractor_host);
                    let component = component.clone();
                    let member_ix = *member_ix;
                    let fuel = fuel_budget(&op_plan.children, &op_plan.outputs);
                    std::thread::spawn(move || {
                        let _finished = h.finish_on_drop();
                        if let Err(e) = host.extract_fueled(
                            &component,
                            container,
                            member_ix,
                            Box::new(w),
                            Some(fuel),
                        ) {
                            h.fail(format!("extractor failed: {e}"));
                        }
                    });
                    Ok(Box::new(r))
                }
                OpImpl::Wasm2 {
                    transform,
                    op,
                    params,
                    random_access_inputs,
                    ..
                } => {
                    let inputs = self.open_wasm2_inputs(&op_plan.children, random_access_inputs)?;
                    let n_outputs = op_plan.outputs.len();
                    let mut sinks: Vec<Box<dyn Write + Send>> = Vec::with_capacity(n_outputs);
                    let mut reader = None;
                    let mut handle = None;
                    for ix in 0..n_outputs {
                        if ix == op_plan.output_ix {
                            let (w, r, h) = pipe::pipe();
                            sinks.push(Box::new(w));
                            reader = Some(r);
                            handle = Some(h);
                        } else {
                            // Unconsumed sibling outputs of a mid-tree node
                            // are discarded; verification happens at the
                            // consuming materialization (D4).
                            sinks.push(Box::new(io::sink()));
                        }
                    }
                    let (reader, handle) = (
                        reader.expect("output_ix in range"),
                        handle.expect("output_ix in range"),
                    );
                    let host = Arc::clone(&self.stream_host);
                    let transform = Arc::clone(transform);
                    let (op, params) = (op.clone(), params.clone());
                    let fuel = fuel_budget(&op_plan.children, &op_plan.outputs);
                    std::thread::spawn(move || {
                        let _finished = handle.finish_on_drop();
                        if let Err(e) =
                            host.run_fueled(&transform, &op, &params, inputs, sinks, Some(fuel))
                        {
                            handle.fail(format!("streaming transform failed: {e}"));
                        }
                    });
                    Ok(Box::new(reader))
                }
            },
        }
    }

    fn open_builtin_or_v1_sequential(
        &self,
        op_plan: &OpPlan,
    ) -> Result<Box<dyn Read + Send>, ExecError> {
        match &op_plan.op {
            OpImpl::Assemble(params) => {
                let children = self.open_children_random(&op_plan.children)?;
                let node =
                    AssembleRandom::new(params.clone(), children).map_err(ExecError::Malformed)?;
                Ok(Box::new(SeqOverRandom::new(Box::new(node))))
            }
            OpImpl::Deflate { offset, len } => {
                let container = self.open_random(&op_plan.children[0])?;
                Ok(Box::new(flate2::read::DeflateDecoder::new(WindowSeq::new(
                    container, *offset, *len,
                ))))
            }
            OpImpl::Wasm1 {
                component,
                op,
                params,
            } => {
                let outputs = self.run_wasm1(op_plan, component, op, params)?;
                Ok(Box::new(Cursor::new(
                    outputs.into_iter().nth(op_plan.output_ix).expect("checked"),
                )))
            }
            OpImpl::Wasm2 { .. } | OpImpl::Extractor { .. } => {
                unreachable!("handled by open_sequential")
            }
        }
    }

    fn open_children_random(
        &self,
        children: &[Plan],
    ) -> Result<Vec<Box<dyn RangeRead>>, ExecError> {
        children.iter().map(|c| self.open_random(c)).collect()
    }

    fn open_wasm2_inputs(
        &self,
        children: &[Plan],
        random_access_inputs: &[u32],
    ) -> Result<Vec<StreamInput>, ExecError> {
        let mut inputs = Vec::with_capacity(children.len());
        for (ix, child) in children.iter().enumerate() {
            let ix32 = u32::try_from(ix).expect("input count fits u32");
            if random_access_inputs.contains(&ix32) {
                inputs.push(StreamInput::RandomAccess(self.open_random(child)?));
            } else {
                inputs.push(StreamInput::Sequential(SequentialInput {
                    reader: self.open_sequential(child)?,
                    len: child.len(),
                }));
            }
        }
        Ok(inputs)
    }

    /// Random access over a plan node. Affine nodes translate; everything
    /// else spills to a temp file first (the spill rule: correctness
    /// first, the planner treats spills as cost).
    fn open_random(&self, plan: &Plan) -> Result<Box<dyn RangeRead>, ExecError> {
        match plan {
            Plan::Literal { hash, .. } => {
                let file = self
                    .store
                    .get(StoreNs::Data, hash)?
                    .ok_or(ExecError::NoRoute(*hash))?;
                Ok(Box::new(FileRandom::new(file)?))
            }
            Plan::Op(op_plan) => match &op_plan.op {
                OpImpl::Assemble(params) => {
                    let children = self.open_children_random(&op_plan.children)?;
                    Ok(Box::new(
                        AssembleRandom::new(params.clone(), children)
                            .map_err(ExecError::Malformed)?,
                    ))
                }
                _ => self.spill(plan),
            },
        }
    }

    /// The D63 carve-out predicate — tight, in code: every op node is a
    /// recipe-backed builtin assemble (nothing computed: deflate and
    /// wasm never qualify) whose row is locally-minted, affine, and
    /// verified; every leaf is a resident, store-verified literal.
    fn affine_carveout(&self, db: &Db, plan: &Plan) -> Result<bool, ExecError> {
        match plan {
            Plan::Literal { hash, .. } => {
                let Some(row) = db.blob_by_hash(hash)? else {
                    return Ok(false);
                };
                Ok(row.residency == Residency::Resident
                    && self.store.has(StoreNs::Data, hash)
                    && db.blob_verified_at(row.blob_id)?.is_some())
            }
            Plan::Op(op_plan) => {
                if !matches!(op_plan.op, OpImpl::Assemble(_)) {
                    return Ok(false);
                }
                // Synthetic nodes (no recipe row) never qualify.
                let Some(recipe_id) = op_plan.recipe_id else {
                    return Ok(false);
                };
                let row = db.recipe_by_id(recipe_id)?;
                if row.source != RecipeSource::LocalIngest
                    || row.seek_class != SeekClass::Affine
                    || !matches!(
                        row.verify,
                        VerifyState::Verified | VerifyState::ReplayedLocal
                    )
                {
                    return Ok(false);
                }
                for child in &op_plan.children {
                    if !self.affine_carveout(db, child)? {
                        return Ok(false);
                    }
                }
                Ok(true)
            }
        }
    }

    /// Random access over a carve-out-qualified plan: interior nodes are
    /// pure assemble arithmetic, leaves re-validate every read against
    /// their own bao tree ([`VerifiedRandom`]). Only called after
    /// [`Self::affine_carveout`] said yes.
    fn open_random_verified(&self, plan: &Plan) -> Result<Box<dyn RangeRead>, ExecError> {
        use datboi_store_fs::obao;
        match plan {
            Plan::Literal { hash, len } => {
                let sidecar = if obao::outboard_size(*len) == 0 {
                    Vec::new()
                } else {
                    // A resident-verified literal may predate its sidecar
                    // (only eviction guaranteed one until now); building
                    // it is cheap and one-time.
                    self.store.ensure_obao(StoreNs::Data, hash)?;
                    self.store
                        .get_obao(StoreNs::Data, hash)?
                        .ok_or(ExecError::MissingOutboard(*hash))?
                };
                let file = self
                    .store
                    .get(StoreNs::Data, hash)?
                    .ok_or(ExecError::NoRoute(*hash))?;
                Ok(Box::new(VerifiedRandom::new(file, *len, *hash, sidecar)))
            }
            Plan::Op(op_plan) => match &op_plan.op {
                OpImpl::Assemble(params) => {
                    let children = op_plan
                        .children
                        .iter()
                        .map(|c| self.open_random_verified(c))
                        .collect::<Result<Vec<_>, _>>()?;
                    Ok(Box::new(
                        AssembleRandom::new(params.clone(), children)
                            .map_err(ExecError::Malformed)?,
                    ))
                }
                _ => Err(ExecError::Malformed(
                    "carve-out plan contains a non-assemble node".into(),
                )),
            },
        }
    }

    /// The D63 blessing pass: materialize-to-null through the sequential
    /// route, computing the output obao in the same pass, and cache the
    /// sidecar — promotes a carved-out route to full D49. Returns
    /// `false` when the sidecar already existed.
    ///
    /// # Errors
    /// [`ExecError::RangeVerifyFailed`] if the route's bytes do not hash
    /// to the claim (nothing is stored); route/planning errors as usual.
    pub fn bless_output(&self, db: &Db, hash: &Blake3) -> Result<bool, ExecError> {
        if self.store.get_obao(StoreNs::Data, hash)?.is_some() {
            return Ok(false);
        }
        if self.is_resident(db, hash)? {
            return Ok(self.store.ensure_obao(StoreNs::Data, hash)?);
        }
        let plan = self.plan(db, hash, 0, &mut Vec::new())?;
        let reader = self.open_sequential(&plan)?;
        let (root, sidecar) = datboi_store_fs::obao::compute(reader, plan.len())
            .map_err(|e| ExecError::Malformed(format!("blessing pass: {e}")))?;
        if root != *hash {
            return Err(ExecError::RangeVerifyFailed {
                hash: *hash,
                detail: format!("blessing pass produced {root}, not the claimed output"),
            });
        }
        self.store.put_obao(StoreNs::Data, hash, &sidecar)?;
        Ok(true)
    }

    fn spill(&self, plan: &Plan) -> Result<Box<dyn RangeRead>, ExecError> {
        let mut file = match &self.config.spill_dir {
            Some(dir) => tempfile::tempfile_in(dir)?,
            None => tempfile::tempfile()?,
        };
        let mut reader = self.open_sequential(plan)?;
        let written = io::copy(&mut reader, &mut file)?;
        if written != plan.len() {
            return Err(ExecError::Malformed(format!(
                "spill produced {written} bytes, claim says {}",
                plan.len()
            )));
        }
        file.rewind()?;
        Ok(Box::new(FileRandom::new(file)?))
    }

    // ---- materializing executions ----

    /// Execute `recipe` and stream every claimed output into the store
    /// (hash-verified, outboard built — [`Store::put_with_obao`]).
    fn execute_to_store(
        &self,
        db: &Db,
        recipe: &Recipe,
        participants: &mut Vec<i64>,
    ) -> Result<Vec<(Blake3, PutOutcome)>, ExecError> {
        let op = self.resolve_op(recipe)?;
        let mut children = Vec::with_capacity(recipe.inputs.len());
        for input in &recipe.inputs {
            let child = self.plan(db, &input.hash, 1, &mut Vec::new())?;
            child.collect_recipe_ids(participants);
            children.push(child);
        }
        let outputs: Vec<(Blake3, u64)> = recipe.outputs.iter().map(|o| (o.hash, o.size)).collect();
        match &op {
            OpImpl::Assemble(_) | OpImpl::Deflate { .. } => {
                if outputs.len() != 1 {
                    return Err(ExecError::Malformed(
                        "single-output builtin claims multiple outputs".into(),
                    ));
                }
                let plan = OpPlan {
                    op,
                    output_ix: 0,
                    outputs: outputs.clone(),
                    children,
                    recipe_id: None,
                };
                let reader = self.open_builtin_or_v1_sequential(&plan)?;
                let outcome =
                    self.store
                        .put_with_obao(StoreNs::Data, outputs[0].0, outputs[0].1, reader)?;
                Ok(vec![(outputs[0].0, outcome)])
            }
            OpImpl::Extractor {
                component,
                member_ix,
            } => {
                // Single opaque output (one member); the container (input
                // 0) arrives random-access — the extractor seeks its
                // headers. The guest's typed verdict is joined HERE (the
                // Wasm2 scoped-thread pattern) so a trap or guest error
                // classifies as a claim failure instead of dissolving
                // into pipe I/O on the consumer side.
                if outputs.len() != 1 {
                    return Err(ExecError::Malformed(
                        "extractor recipe must claim exactly one member output".into(),
                    ));
                }
                let container = self.open_random(&children[0])?;
                let fuel = fuel_budget(&children, &outputs);
                let host = Arc::clone(&self.extractor_host);
                let component = Arc::clone(component);
                let member_ix = *member_ix;
                let (w, r, h) = pipe::pipe();
                std::thread::scope(|scope| {
                    let guest = scope.spawn(move || {
                        host.extract_fueled(
                            &component,
                            container,
                            member_ix,
                            Box::new(w),
                            Some(fuel),
                        )
                    });
                    let consumer = scope.spawn(|| {
                        self.store
                            .put_with_obao(StoreNs::Data, outputs[0].0, outputs[0].1, r)
                    });
                    let guest_result = guest.join().expect("guest thread never panics");
                    // Verdict first, then finish: a consumer blocked at
                    // channel-disconnect waits for it (pipe race fix).
                    if let Err(e) = &guest_result {
                        h.fail(format!("extractor failed: {e}"));
                    }
                    h.finish();
                    let stored = consumer.join().expect("consumer thread never panics");
                    // The guest's own error explains a consumer failure
                    // better than the downstream hash mismatch does.
                    if let Err(e) = guest_result {
                        return Err(e.into());
                    }
                    Ok(vec![(outputs[0].0, stored?)])
                })
            }
            OpImpl::Wasm1 {
                component,
                op: opname,
                params,
            } => {
                let plan = OpPlan {
                    op: OpImpl::Wasm1 {
                        component: component.clone(),
                        op: opname.clone(),
                        params: params.clone(),
                    },
                    output_ix: 0,
                    outputs: outputs.clone(),
                    children,
                    recipe_id: None,
                };
                let blobs = self.run_wasm1(&plan, component, opname, params)?;
                let mut results = Vec::with_capacity(outputs.len());
                for ((hash, size), bytes) in outputs.iter().zip(blobs) {
                    let outcome = self.store.put_with_obao(
                        StoreNs::Data,
                        *hash,
                        *size,
                        Cursor::new(bytes),
                    )?;
                    results.push((*hash, outcome));
                }
                Ok(results)
            }
            OpImpl::Wasm2 {
                transform,
                op: opname,
                params,
                random_access_inputs,
                ..
            } => {
                let inputs = self.open_wasm2_inputs(&children, random_access_inputs)?;
                let mut sinks: Vec<Box<dyn Write + Send>> = Vec::with_capacity(outputs.len());
                let mut readers = Vec::with_capacity(outputs.len());
                for _ in &outputs {
                    let (w, r, h) = pipe::pipe();
                    sinks.push(Box::new(w));
                    readers.push((r, h));
                }
                let host = Arc::clone(&self.stream_host);
                let transform = Arc::clone(transform);
                let (opname, params) = (opname.clone(), params.clone());
                let fuel = fuel_budget(&children, &outputs);
                std::thread::scope(|scope| {
                    let handles: Vec<pipe::PipeHandle> =
                        readers.iter().map(|(_, h)| h.clone()).collect();
                    let guest = scope.spawn(move || {
                        host.run_fueled(&transform, &opname, &params, inputs, sinks, Some(fuel))
                    });
                    let consumers: Vec<_> = readers
                        .into_iter()
                        .zip(&outputs)
                        .map(|((reader, _), (hash, size))| {
                            let (hash, size) = (*hash, *size);
                            scope.spawn(move || {
                                self.store.put_with_obao(StoreNs::Data, hash, size, reader)
                            })
                        })
                        .collect();
                    let guest_result = guest.join().expect("guest thread never panics");
                    for h in &handles {
                        // Verdict first, then finish: consumers blocked at
                        // channel-disconnect wait for it (pipe race fix).
                        if let Err(e) = &guest_result {
                            h.fail(format!("streaming transform failed: {e}"));
                        }
                        h.finish();
                    }
                    let mut results = Vec::with_capacity(outputs.len());
                    let mut first_err: Option<ExecError> = None;
                    for (consumer, (hash, _)) in consumers.into_iter().zip(&outputs) {
                        match consumer.join().expect("consumer thread never panics") {
                            Ok(outcome) => results.push((*hash, outcome)),
                            Err(e) => {
                                first_err.get_or_insert(e.into());
                            }
                        }
                    }
                    // The guest's own error explains a consumer failure
                    // better than the downstream hash mismatch does.
                    if let Err(e) = guest_result {
                        return Err(e.into());
                    }
                    match first_err {
                        Some(e) => Err(e),
                        None => Ok(results),
                    }
                })
            }
        }
    }

    fn run_wasm1(
        &self,
        op_plan: &OpPlan,
        component: &[u8],
        op: &str,
        params: &[u8],
    ) -> Result<Vec<Vec<u8>>, ExecError> {
        let mut buffers = Vec::with_capacity(op_plan.children.len());
        let mut total = 0u64;
        for child in &op_plan.children {
            let size = child.len();
            total = total.saturating_add(size);
            if size > self.config.max_buffer || total > self.config.max_buffer {
                return Err(ExecError::BufferCap {
                    size: size.max(total),
                    cap: self.config.max_buffer,
                });
            }
            let mut buf = Vec::with_capacity(usize::try_from(size).expect("capped"));
            self.open_sequential(child)?.read_to_end(&mut buf)?;
            buffers.push(buf);
        }
        let outputs = self.v1_host.run(component, op, params, &buffers)?;
        if outputs.len() != op_plan.outputs.len() {
            return Err(ExecError::Malformed(format!(
                "@1 transform produced {} outputs, recipe claims {}",
                outputs.len(),
                op_plan.outputs.len()
            )));
        }
        Ok(outputs)
    }
}

/// Shared Vec sink: the host consumes it as `Box<dyn Write + Send>`, the
/// caller keeps a handle to collect the bytes.
#[derive(Clone, Default)]
struct VecSink(Arc<Mutex<Vec<u8>>>);

impl VecSink {
    fn take(&self) -> Vec<u8> {
        std::mem::take(&mut self.0.lock().expect("sink mutex"))
    }
}

impl Write for VecSink {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().expect("sink mutex").extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Fuel budget for one wasm2 execution, scaled with the recipe's
/// declared byte sizes. Measured on xf-preflate recreate: ~52
/// fuel/plaintext byte on ordinary text/binary, but ~875/byte on a
/// match-dense high-entropy tracker module from a real corpus — the
/// hash-chain walk dominates and varies by an order of magnitude.
/// 4096/byte gives ~4.7x headroom over the worst observed case; fuel
/// exists only to kill runaways, so generosity here costs nothing.
/// Deterministic — a pure function of recipe claims.
const FUEL_BASE: u64 = 1 << 24;
const FUEL_PER_BYTE: u64 = 4096;

fn fuel_budget(children: &[Plan], outputs: &[(Blake3, u64)]) -> u64 {
    let bytes = children
        .iter()
        .map(Plan::len)
        .fold(0u64, u64::saturating_add)
        .saturating_add(
            outputs
                .iter()
                .map(|(_, s)| *s)
                .fold(0u64, u64::saturating_add),
        );
    FUEL_BASE.saturating_add(bytes.saturating_mul(FUEL_PER_BYTE))
}

/// DFS for the wasm2 node running `component`; returns its children —
/// the inputs whose integrity decides quarantine attribution.
fn find_wasm2_children<'p>(plan: &'p Plan, component: &Blake3) -> Option<&'p [Plan]> {
    let Plan::Op(op_plan) = plan else {
        return None;
    };
    if let OpImpl::Wasm2 { component: c, .. } = &op_plan.op
        && c == component
    {
        return Some(&op_plan.children);
    }
    op_plan
        .children
        .iter()
        .find_map(|child| find_wasm2_children(child, component))
}

/// Literal leaves grounding a plan subtree.
fn collect_literal_leaves(plan: &Plan, out: &mut Vec<(Blake3, u64)>) {
    match plan {
        Plan::Literal { hash, len } => out.push((*hash, *len)),
        Plan::Op(op_plan) => {
            for child in &op_plan.children {
                collect_literal_leaves(child, out);
            }
        }
    }
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The classification the replay path applies to a joined guest
    /// verdict (extractor and Wasm2 alike): traps and guest errors
    /// indict the claim; fuel exhaustion is a policy outcome and must
    /// stay retryable.
    #[test]
    fn guest_verdicts_classify_against_the_claim() {
        let trap = ExecError::Runtime(RuntimeError::Trap(
            wasmtime::Trap::UnreachableCodeReached.into(),
        ));
        assert!(trap.is_claim_failure());
        let guest_error = ExecError::Runtime(RuntimeError::Transform("bad member".into()));
        assert!(guest_error.is_claim_failure());
        let fuel = ExecError::Runtime(RuntimeError::Trap(wasmtime::Trap::OutOfFuel.into()));
        assert!(!fuel.is_claim_failure());
        let wiring = ExecError::Runtime(RuntimeError::Instantiate(wasmtime::Error::msg(
            "linker regression",
        )));
        assert!(!wiring.is_claim_failure());
        let child_lie = ExecError::Runtime(RuntimeError::InputLengthMismatch {
            input_ix: 0,
            claimed: 2000,
            actual: 1000,
        });
        assert!(!child_lie.is_claim_failure());
    }
}
