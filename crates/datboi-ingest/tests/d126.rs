//! D126, end to end: a route that DISPROVES ITSELF settles the sweep
//! item, and an environmental failure still retries.
//!
//! The live shape this reproduces: `xf-preflate`'s `recreate` panics
//! inside the guest on some member plaintexts
//! (`preflate-rs-0.7.6/src/tree_predictor.rs:169`), wasmtime traps it,
//! and before D126 the trap reached the sweep as a bare `io::Error`
//! string — indistinguishable from a full disk. The item errored
//! environmentally and came back on every ambient wake, forever, while
//! the `analysis` table recorded nothing at all.
//!
//! A real preflate panic needs a corpus this test does not have, so the
//! TRAP is simulated — by the committed reference component's `greedy`
//! op, whose `read(u32::MAX)` the host's resource-abuse guard traps.
//! That substitution is honest for what is under test: everything from
//! `RuntimeError::Trap` outward is the same code, and the panic's only
//! contribution upstream is that it becomes a trap.

use datboi_core::hash::Blake3;
use datboi_core::recipe::{InputRef, Op, OutputRef, Recipe, World as WasmWorld};
use datboi_exec::{ExecConfig, Executor};
use datboi_index::{
    Db, Namespace as IndexNs, RecipeSource, Residency, SeekClass, VerifyAdvance, VerifyState,
};
use datboi_ingest::analyzers::NdsAnalyzer;
use datboi_ingest::refine::{Analyzer as _, Logical, SweepReport, run_sweep};
use datboi_store_fs::{Namespace as StoreNs, Store};

/// The same committed fixture the runtime gate and the executor tests
/// pin (D51).
const COMPONENT: &[u8] =
    include_bytes!("../../datboi-runtime/tests/fixtures/xf_reference_stream.wasm");

struct World {
    _dir: tempfile::TempDir,
    store: Store,
    db: Db,
}

fn world() -> World {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Store::open(dir.path().join("store")).expect("store");
    let db = Db::open(dir.path()).expect("db");
    // D92: admit every grounded absent, so the sweep actually reaches a
    // blob whose bytes live behind a route.
    db.config_set("refine:absent:mode", b"all").expect("policy");
    World {
        _dir: dir,
        store,
        db,
    }
}

impl World {
    fn put_literal(&mut self, bytes: &[u8]) -> Blake3 {
        let hash = Blake3::compute(bytes);
        self.store.put(StoreNs::Data, hash, bytes).expect("put");
        self.db
            .upsert_blob(
                &hash,
                Some(bytes.len() as u64),
                IndexNs::Data,
                Residency::Resident,
            )
            .expect("blob row");
        hash
    }

    /// An index row for bytes we do not hold — the population D92 made
    /// sweepable and the population D126 is about.
    fn claim_absent(&mut self, seed: &[u8], size: u64) -> (Blake3, i64) {
        let hash = Blake3::compute(seed);
        let id = self
            .db
            .upsert_blob(&hash, Some(size), IndexNs::Data, Residency::Absent)
            .expect("blob row");
        (hash, id)
    }

    fn mint(&mut self, recipe: &Recipe, seek: SeekClass) -> i64 {
        let encoded = recipe.encode().expect("valid recipe");
        let recipe_hash = Blake3::compute(&encoded);
        self.store
            .put(StoreNs::Meta, recipe_hash, encoded.as_slice())
            .expect("recipe blob");
        let recipe_blob_id = self
            .db
            .upsert_blob(
                &recipe_hash,
                Some(encoded.len() as u64),
                IndexNs::Meta,
                Residency::Resident,
            )
            .expect("recipe blob row");
        let id = self
            .db
            .index_recipe(recipe_blob_id, recipe, seek, RecipeSource::LocalIngest)
            .expect("recipe rows");
        self.db
            .set_verify_state(id, VerifyAdvance::Verified, 1)
            .expect("verified");
        id
    }

    fn sweep(&mut self) -> SweepReport {
        let exec = Executor::new(&self.store, ExecConfig::default()).expect("executor");
        let bytes = Logical::new(&self.store, &exec);
        run_sweep(&mut self.db, &self.store, &bytes, &mut NdsAnalyzer, 50).expect("sweep")
    }

    fn queued(&self, analyzer: &Blake3) -> u64 {
        self.db.sweep_queue_len(analyzer).expect("queue len")
    }
}

/// A wasm route that traps: `greedy` asks for `u32::MAX` bytes in one
/// read and the host's `MAX_READ` guard traps before allocating.
fn trapping_route(w: &mut World, seed: &[u8]) -> (Blake3, i64, i64) {
    let component = w.put_literal(COMPONENT);
    let input = w.put_literal(b"a small input the greedy op will over-read");
    let (output, blob_id) = w.claim_absent(seed, 4096);
    let recipe_id = w.mint(
        &Recipe {
            op: Op::Wasm {
                component,
                world: WasmWorld::Transform1,
                export: "greedy".into(),
            },
            inputs: vec![InputRef {
                hash: input,
                role: None,
            }],
            outputs: vec![OutputRef {
                hash: output,
                size: 4096,
                name: None,
            }],
            params: Vec::new(),
        },
        SeekClass::Opaque,
    );
    (output, blob_id, recipe_id)
}

/// The headline. One sweep meets the trap; the route is poisoned, the
/// item leaves the queue WITHOUT an analysis row, and it never comes
/// back — because nothing read the bytes, so there is no conclusion
/// about them to record (D48's row is "what `analyzer` concluded about
/// `blob`'s bytes"), but re-running a pure function that traps cannot
/// help either.
#[test]
fn a_deterministic_trap_settles_and_poisons_its_route() {
    let mut w = world();
    let (_hash, blob_id, recipe_id) = trapping_route(&mut w, b"bytes behind a trapping route");
    let analyzer = NdsAnalyzer.id();

    let first = w.sweep();
    assert_eq!(
        first.errors.len(),
        0,
        "a disproof is not an environmental error: {:?}",
        first.errors
    );
    assert_eq!(first.unobtainable.len(), 1, "the trap was recognised");
    assert_eq!(
        first.deferred, 1,
        "the item waits (D116), it does not error"
    );
    assert!(
        first.unobtainable[0].1.contains("poisoned"),
        "the detail names what was recorded: {}",
        first.unobtainable[0].1
    );

    // D25's verdict, written from the READ path for the first time.
    assert_eq!(
        w.db.recipe_by_id(recipe_id).expect("row").verify,
        VerifyState::Failed,
        "the route claimed these bytes and does not produce them"
    );

    // NOT a conclusion about the bytes: no analysis row, for anyone.
    assert_eq!(
        w.db.analysis_outcome(blob_id, &analyzer).expect("q"),
        None,
        "nothing read the bytes, so nothing concluded about them"
    );
    let rows: i64 =
        w.db.cache()
            .query_row(
                "SELECT COUNT(*) FROM analysis WHERE blob_id = ?1",
                [blob_id],
                |r| r.get(0),
            )
            .expect("count");
    assert_eq!(rows, 0, "no trap row is a verdict row, for any analyzer");

    // Settled for scheduling: out of the queue, and it stays out.
    let queued_for_it: i64 =
        w.db.cache()
            .query_row(
                "SELECT COUNT(*) FROM sweep_queue WHERE blob_id = ?1",
                [blob_id],
                |r| r.get(0),
            )
            .expect("count");
    assert_eq!(queued_for_it, 0, "the queue row is gone");
    let second = w.sweep();
    assert_eq!(second.analyzed, 0, "nothing left to analyze");
    assert_eq!(second.deferred, 0, "and the waiting item did not come back");
    assert_eq!(second.errors.len(), 0);
    assert_eq!(second.enqueued, 0, "not re-enqueued");
    assert_eq!(w.queued(&analyzer), 0);
}

/// The other side of D81's line, which D126 must not blur: a store that
/// lost the bytes a route needs is the ENVIRONMENT failing, not the
/// claim. Nothing is poisoned, and the item keeps its queue row so a
/// repaired store gets another go.
#[test]
fn an_environmental_failure_still_retries() {
    let mut w = world();
    // A CORRECT route — its output really is the first 64 bytes of its
    // input — over a literal the index calls resident and the store
    // does not have: a wiped store dir, an unmounted NAS, a rename that
    // never landed. Correct on purpose, so the retry can succeed and
    // prove nothing was foreclosed.
    let whole: Vec<u8> = (0..128u8).collect();
    let missing = Blake3::compute(&whole);
    w.db.upsert_blob(
        &missing,
        Some(whole.len() as u64),
        IndexNs::Data,
        Residency::Resident,
    )
    .expect("blob row");
    let (hash, blob_id, recipe_id) = {
        let (output, blob_id) = w.claim_absent(&whole[..64], 64);
        let recipe_id = w.mint(
            &Recipe {
                op: Op::Builtin {
                    name: "assemble".into(),
                    major: 1,
                },
                inputs: vec![InputRef {
                    hash: missing,
                    role: None,
                }],
                outputs: vec![OutputRef {
                    hash: output,
                    size: 64,
                    name: None,
                }],
                params: datboi_core::assemble::AssembleParams {
                    segments: vec![datboi_core::assemble::Segment::BlobRange {
                        input_ix: 0,
                        offset: 0,
                        len: 64,
                    }],
                }
                .encode()
                .expect("params"),
            },
            SeekClass::Affine,
        );
        (output, blob_id, recipe_id)
    };
    let analyzer = NdsAnalyzer.id();

    let first = w.sweep();
    assert!(
        first.errors.iter().any(|(h, _)| *h == hash),
        "environmental: reported, not settled — {:?}",
        first.errors
    );
    assert_eq!(first.unobtainable.len(), 0, "nothing was disproved");
    assert_eq!(first.deferred, 0, "an environment failure does not wait");
    assert_eq!(
        w.db.recipe_by_id(recipe_id).expect("row").verify,
        VerifyState::Verified,
        "a route is not poisoned for a failure that is not its fault"
    );
    assert_eq!(
        w.db.analysis_outcome(blob_id, &analyzer).expect("q"),
        None,
        "still no conclusion"
    );

    // The queue row survives (leased until expiry, which is the D71
    // error backoff) — so a repaired store gets another go.
    let queued_for_it: i64 =
        w.db.cache()
            .query_row(
                "SELECT COUNT(*) FROM sweep_queue WHERE blob_id = ?1",
                [blob_id],
                |r| r.get(0),
            )
            .expect("count");
    assert_eq!(queued_for_it, 1, "still queued");

    // Repair the environment — the bytes come back — and the item that
    // D126 refused to settle gets its analysis. This is the whole
    // reason the line exists: a wrong `Unobtainable` here would have
    // foreclosed it forever.
    w.store
        .put(StoreNs::Data, missing, whole.as_slice())
        .expect("restored");
    let missing_id = w.db.get_blob_id(&missing).expect("q").expect("row");
    w.db.set_residency(missing_id, Residency::Resident)
        .expect("resident again");
    w.db.clear_sweep_leases().expect("leases");
    let second = w.sweep();
    assert_eq!(second.unobtainable.len(), 0, "still nothing disproved");
    assert_eq!(
        w.db.analysis_outcome(blob_id, &analyzer).expect("q"),
        Some(datboi_index::AnalysisOutcome::Negative),
        "the retry reached the bytes and concluded about them"
    );
    assert_eq!(
        w.db.recipe_by_id(recipe_id).expect("row").verify,
        VerifyState::Verified,
        "and the route was never poisoned"
    );
    let _ = second;
}
