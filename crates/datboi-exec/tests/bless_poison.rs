//! D126 through the pass that motivated it: `bless --materialize`.
//!
//! The first cut of D126 unwrapped the deterministic verdict out of
//! `ExecError::Io`, which is the shape `spill` produces — and the
//! blessing pass does not spill. It streams a whole route through
//! `obao::compute`, so a guest trap arrives as
//! `Store(Obao(Io(Deterministic)))` when materializing, and as
//! `Obao(Io(Deterministic))` when only blessing. Neither was matched,
//! `is_claim_failure` fell through to `false`, and a live
//! `bless --materialize --min-size 1M` reported `"poisoned":0` beside
//! the same 77 failures it had reported the run before.
//!
//! These tests drive both shapes. The trap is the committed reference
//! component's `greedy` op (`read(u32::MAX)`, which the host's
//! `MAX_READ` guard traps) standing in for the preflate panic — the
//! same substitution, and the same reasoning, as `datboi-ingest`'s
//! d126 suite.

use datboi_core::hash::Blake3;
use datboi_core::recipe::{InputRef, Op, OutputRef, Recipe, World as WasmWorld};
use datboi_exec::bless::{BlessOptions, BlessReport};
use datboi_exec::{ExecConfig, Executor};
use datboi_index::{
    Db, Namespace as IndexNs, RecipeSource, Residency, SeekClass, VerifyAdvance, VerifyState,
};
use datboi_store_fs::{Namespace as StoreNs, Store};

const COMPONENT: &[u8] =
    include_bytes!("../../datboi-runtime/tests/fixtures/xf_reference_stream.wasm");

/// Comfortably past one bao group, so the blessing floor admits it.
const CLAIM_LEN: u64 = 100_000;

struct World {
    dir: tempfile::TempDir,
    store: Store,
    db: Db,
    /// The claimed-but-absent output whose only route traps.
    output: Blake3,
    recipe_id: i64,
}

/// A store holding a component and a small input, an index claiming an
/// output nothing can produce, and a Verified recipe saying `greedy`
/// produces it.
fn trapping_world() -> World {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Store::open(dir.path().join("store")).expect("store");
    let mut db = Db::open(dir.path()).expect("db");

    let put = |bytes: &[u8]| {
        let hash = Blake3::compute(bytes);
        store.put(StoreNs::Data, hash, bytes).expect("put");
        db.upsert_blob(
            &hash,
            Some(bytes.len() as u64),
            IndexNs::Data,
            Residency::Resident,
        )
        .expect("blob row");
        hash
    };
    let component = put(COMPONENT);
    let input = put(b"a small input the greedy op will over-read");

    let output = Blake3::compute(b"bytes no route can produce");
    db.upsert_blob(&output, Some(CLAIM_LEN), IndexNs::Data, Residency::Absent)
        .expect("output row");

    let recipe = Recipe {
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
            size: CLAIM_LEN,
            name: None,
        }],
        params: Vec::new(),
    };
    let encoded = recipe.encode().expect("recipe");
    let recipe_hash = Blake3::compute(&encoded);
    store
        .put(StoreNs::Meta, recipe_hash, encoded.as_slice())
        .expect("recipe blob");
    let recipe_blob = db
        .upsert_blob(
            &recipe_hash,
            Some(encoded.len() as u64),
            IndexNs::Meta,
            Residency::Resident,
        )
        .expect("recipe blob row");
    let recipe_id = db
        .index_recipe(
            recipe_blob,
            &recipe,
            SeekClass::Opaque,
            RecipeSource::LocalIngest,
        )
        .expect("recipe rows");
    db.set_verify_state(recipe_id, VerifyAdvance::Verified, 1)
        .expect("verified");
    World {
        dir,
        store,
        db,
        output,
        recipe_id,
    }
}

impl World {
    fn bless(&self, opts: &BlessOptions) -> BlessReport {
        Executor::new(
            &self.store,
            ExecConfig {
                spill_dir: Some(self.dir.path().to_owned()),
                ..ExecConfig::default()
            },
        )
        .expect("executor")
        .bless_corpus(&self.db, opts, &mut |_| {})
        .expect("bless pass")
    }

    fn verify(&self) -> VerifyState {
        self.db.recipe_by_id(self.recipe_id).expect("row").verify
    }
}

/// The live shape: `--materialize`, where the trap arrives wrapped in
/// `StoreError::Obao`. One run must poison; the next must not see the
/// candidate at all.
#[test]
fn a_materializing_bless_poisons_the_route_that_traps() {
    let w = trapping_world();
    assert_eq!(w.verify(), VerifyState::Verified, "starts trusted");

    let report = w.bless(&BlessOptions {
        materialize: true,
        parallelism: 1,
        ..BlessOptions::default()
    });

    assert_eq!(report.selected, 1, "the claim was a candidate");
    assert_eq!(report.blessed, 0, "and it could not be blessed");
    assert_eq!(report.failed.len(), 1, "reported: {:?}", report.failed);
    assert_eq!(
        report.poisoned, 1,
        "the trap disproved the route and D126 recorded it — failures were {:?}",
        report.failed
    );
    assert_eq!(w.verify(), VerifyState::Failed, "D25's verdict is written");
    assert!(
        !w.store.has(StoreNs::Data, &w.output),
        "nothing was published"
    );

    // The whole point: the next run does not repeat it. A poisoned
    // route stops being a route, so the candidate predicate drops the
    // blob rather than handing it back to a worker.
    let again = w.bless(&BlessOptions {
        materialize: true,
        parallelism: 1,
        ..BlessOptions::default()
    });
    assert_eq!(again.population, 0, "no longer a candidate");
    assert_eq!(again.selected, 0);
    assert_eq!(again.failed.len(), 0, "{:?}", again.failed);
    assert_eq!(again.poisoned, 0, "nothing left to learn");
}

/// The blessing-only path wraps one layer differently (`ExecError::Obao`
/// with no store write in front of it). It must reach the same verdict —
/// otherwise the fix would depend on which flag the operator typed.
#[test]
fn a_blessing_only_pass_poisons_the_same_route() {
    let w = trapping_world();
    let report = w.bless(&BlessOptions {
        parallelism: 1,
        ..BlessOptions::default()
    });

    assert_eq!(report.failed.len(), 1, "reported: {:?}", report.failed);
    assert_eq!(
        report.poisoned, 1,
        "same verdict without --materialize — failures were {:?}",
        report.failed
    );
    assert_eq!(w.verify(), VerifyState::Failed);
}

/// The other half of D126's line, asked of the classifier directly.
///
/// The blessing pass catches its ENVIRONMENTAL failures at triage — a
/// route whose literals are gone plans as `NoRoute` and lands in
/// `no_route`, never reaching a worker — so an integration test cannot
/// drive one through the same wrapper. The wrapper is what broke, so
/// the wrapper is what this pins: the exact `Store(Obao(Io(..)))`
/// nesting the live failure arrived in, classified three ways.
#[test]
fn the_classifier_reads_through_the_obao_wrapper_and_only_for_a_disproof() {
    use datboi_exec::ExecError;
    use datboi_runtime::pipe::Deterministic;
    use datboi_store_fs::StoreError;
    use datboi_store_fs::obao::ObaoError;
    use std::io;

    let wrap = |inner: io::Error| {
        ExecError::Store(StoreError::Obao {
            path: "/mnt/datboi/store/tmp/x-0.temp".into(),
            source: ObaoError::Io(inner),
        })
    };

    // A guest trap, three layers down: a disproof.
    let trapped = wrap(io::Error::new(
        io::ErrorKind::InvalidData,
        Deterministic("streaming transform failed: transform trapped".into()),
    ));
    assert!(
        trapped.is_claim_failure(),
        "the live shape must classify: {trapped}"
    );

    // A full disk, same nesting: not a disproof.
    let full = wrap(io::Error::new(io::ErrorKind::StorageFull, "no space left"));
    assert!(
        !full.is_claim_failure(),
        "an environmental failure must not poison: {full}"
    );

    // Fuel exhaustion never reaches the wrapper marked deterministic —
    // the producer asks this same predicate before marking, and the
    // Trap arm excludes OutOfFuel — so the unmarked shape is what
    // arrives, and it must stay retryable. A budget is policy.
    let out_of_fuel = ExecError::Runtime(datboi_runtime::RuntimeError::Trap(
        wasmtime::Trap::OutOfFuel.into(),
    ));
    assert!(!out_of_fuel.is_claim_failure(), "a budget is not evidence");
    assert!(!wrap(io::Error::other(out_of_fuel.to_string())).is_claim_failure());

    // The second gap the D126 sweep found: a blessing pass that
    // re-hashes a whole route to something other than its claim is a
    // disproof too, and it used to report `RangeVerifyFailed` — which
    // this predicate refuses ON PURPOSE, because `serve_range` uses that
    // variant for a seekable component's lying window (quarantine the
    // seek claim, not the recipe). Two failures, one variant, and the
    // blessing one was getting the serving one's answer.
    assert!(
        ExecError::ClaimMismatch {
            expected: Blake3::compute(b"claimed"),
            actual: Blake3::compute(b"produced"),
            len: 100,
        }
        .is_claim_failure(),
        "a route that re-hashes wrong disproves itself"
    );
    assert!(
        !ExecError::RangeVerifyFailed {
            hash: Blake3::compute(b"served"),
            detail: "a seekable component's window lied".into(),
        }
        .is_claim_failure(),
        "but a serving range check still indicts the seek claim, not the recipe"
    );
}
