//! Executor integration: replay licensing (D25), streaming composition
//! (D51 threads+pipes), the spill rule, and claim poisoning — over real
//! store + index instances and the committed @2 reference component.

use std::io::Write as _;

use datboi_core::assemble::{AssembleParams, Segment};
use datboi_core::cbor::{self, Value};
use datboi_core::hash::Blake3;
use datboi_core::recipe::{InputRef, Op, OutputRef, Recipe, World as WasmWorld};
use datboi_exec::{ExecConfig, ExecError, Executor};
use datboi_index::recipes::NewRecipe;
use datboi_index::{
    Db, Namespace as IndexNs, OpKind, RecipeSource, Residency, SeekClass, VerifyAdvance,
    VerifyState,
};
use datboi_store_fs::{Namespace as StoreNs, Store, StoreError};
use flate2::Compression;
use flate2::write::DeflateEncoder;

/// The same committed fixture the runtime gate pins (D51).
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
    World {
        _dir: dir,
        store,
        db,
    }
}

fn pattern(len: usize) -> Vec<u8> {
    let mut state: u64 = 0x243F_6A88_85A3_08D3;
    (0..len)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state >> 24) as u8
        })
        .collect()
}

impl World {
    /// Store a literal and index it Resident.
    fn put_literal(&mut self, bytes: &[u8]) -> (Blake3, i64) {
        let hash = Blake3::compute(bytes);
        self.store.put(StoreNs::Data, hash, bytes).expect("put");
        let id = self
            .db
            .upsert_blob(
                &hash,
                Some(bytes.len() as u64),
                IndexNs::Data,
                Residency::Resident,
            )
            .expect("blob row");
        (hash, id)
    }

    /// Index a claimed-but-absent output identity.
    fn claim_absent(&mut self, bytes: &[u8]) -> (Blake3, i64) {
        let hash = Blake3::compute(bytes);
        let id = self
            .db
            .upsert_blob(
                &hash,
                Some(bytes.len() as u64),
                IndexNs::Data,
                Residency::Absent,
            )
            .expect("blob row");
        (hash, id)
    }

    /// Publish a recipe object + index rows (Pending), mirroring what
    /// ingest/analyzers mint.
    fn mint_recipe(
        &mut self,
        recipe: &Recipe,
        op_name: &str,
        op_kind: OpKind,
        seek: SeekClass,
        inputs: &[(u32, i64)],
        outputs: &[(u32, i64, u64)],
    ) -> i64 {
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
        let inputs: Vec<(u32, i64, Option<&str>)> =
            inputs.iter().map(|(p, id)| (*p, *id, None)).collect();
        let outputs: Vec<(u32, i64, u64, Option<&str>)> = outputs
            .iter()
            .map(|(o, id, s)| (*o, *id, *s, None))
            .collect();
        self.db
            .insert_recipe(&NewRecipe {
                blob_id: recipe_blob_id,
                op_kind,
                op_name,
                seek_class: seek,
                source: RecipeSource::LocalIngest,
                inputs: &inputs,
                outputs: &outputs,
            })
            .expect("recipe row")
    }
}

fn deflate_window_params(offset: u64, len: u64) -> Vec<u8> {
    cbor::encode(&Value::Map(vec![
        (1, Value::Uint(offset)),
        (2, Value::Uint(len)),
    ]))
    .expect("params")
}

fn byteswap(bytes: &[u8]) -> Vec<u8> {
    let mut out = bytes.to_vec();
    for pair in out.chunks_exact_mut(2) {
        pair.swap(0, 1);
    }
    out
}

#[test]
fn replays_deflate_window_recipe_and_licenses_drop() {
    let mut w = world();
    let member = pattern(200_000);
    let mut enc = DeflateEncoder::new(Vec::new(), Compression::default());
    enc.write_all(&member).expect("deflate");
    let compressed = enc.finish().expect("deflate finish");
    // Container: junk prefix + deflate stream + junk suffix — the zip
    // member shape (one windowed recipe over the container, D16 window
    // amendment).
    let mut container = b"local header junk".to_vec();
    let offset = container.len() as u64;
    container.extend_from_slice(&compressed);
    container.extend_from_slice(b"trailing central directory junk");

    let (container_hash, container_id) = w.put_literal(&container);
    let (member_hash, member_id) = w.claim_absent(&member);
    let recipe = Recipe {
        op: Op::Builtin {
            name: "deflate-decompress".into(),
            major: 1,
        },
        inputs: vec![InputRef {
            hash: container_hash,
            role: None,
        }],
        outputs: vec![OutputRef {
            hash: member_hash,
            size: member.len() as u64,
            name: Some("member.bin".into()),
        }],
        params: deflate_window_params(offset, compressed.len() as u64),
    };
    let recipe_id = w.mint_recipe(
        &recipe,
        "deflate-decompress@1",
        OpKind::Builtin,
        SeekClass::Opaque,
        &[(0, container_id)],
        &[(0, member_id, member.len() as u64)],
    );

    let exec = Executor::new(&w.store, ExecConfig::default()).expect("executor");
    let report = exec.replay(&w.db, recipe_id).expect("replays");
    assert_eq!(report.outputs.len(), 1);

    // The member is now resident, verified, outboard built.
    assert!(w.store.has(StoreNs::Data, &member_hash));
    assert!(
        w.store
            .get_obao(StoreNs::Data, &member_hash)
            .expect("obao")
            .is_some()
    );
    assert_eq!(
        w.store
            .read_range_verified(StoreNs::Data, &member_hash, 150_000, 1024)
            .expect("verified range"),
        &member[150_000..151_024]
    );
    let row = w.db.recipe_by_id(recipe_id).expect("row");
    assert_eq!(
        row.verify,
        VerifyState::ReplayedLocal,
        "D25 license granted"
    );
    let blob = w.db.blob_by_hash(&member_hash).expect("q").expect("row");
    assert_eq!(blob.residency, Residency::Resident);

    // Replay is idempotent (stream re-verified, store unchanged).
    let report = exec.replay(&w.db, recipe_id).expect("re-replays");
    assert!(matches!(
        report.outputs[0].1,
        datboi_store_fs::PutOutcome::AlreadyPresent
    ));
}

#[test]
fn lying_claim_poisons_the_recipe_and_publishes_nothing() {
    let mut w = world();
    let (container_hash, container_id) = w.put_literal(&pattern(50_000));
    // Claim: bytes 100..2100 of the container hash to something they don't.
    let lie = Blake3::compute(b"this is not what the slice hashes to");
    let lie_id =
        w.db.upsert_blob(&lie, Some(2000), IndexNs::Data, Residency::Absent)
            .expect("row");
    let recipe = Recipe {
        op: Op::Builtin {
            name: "assemble".into(),
            major: 1,
        },
        inputs: vec![InputRef {
            hash: container_hash,
            role: None,
        }],
        outputs: vec![OutputRef {
            hash: lie,
            size: 2000,
            name: None,
        }],
        params: AssembleParams {
            segments: vec![Segment::BlobRange {
                input_ix: 0,
                offset: 100,
                len: 2000,
            }],
        }
        .encode()
        .expect("params"),
    };
    let recipe_id = w.mint_recipe(
        &recipe,
        "assemble@1",
        OpKind::Builtin,
        SeekClass::Affine,
        &[(0, container_id)],
        &[(0, lie_id, 2000)],
    );

    let exec = Executor::new(&w.store, ExecConfig::default()).expect("executor");
    let err = exec.replay(&w.db, recipe_id).expect_err("claim is false");
    assert!(
        err.is_claim_failure(),
        "hash mismatch indicts the claim: {err}"
    );
    assert!(
        matches!(err, ExecError::Store(StoreError::HashMismatch { .. })),
        "{err}"
    );
    assert!(!w.store.has(StoreNs::Data, &lie), "nothing published");
    let row = w.db.recipe_by_id(recipe_id).expect("row");
    assert_eq!(row.verify, VerifyState::Failed, "poisoned (D25)");
    // Poisoned recipes refuse to re-run.
    assert!(matches!(
        exec.replay(&w.db, recipe_id),
        Err(ExecError::Poisoned(_))
    ));
}

#[test]
fn wasm2_recipe_replays_and_streams() {
    let mut w = world();
    let input = pattern(300_000);
    let swapped = byteswap(&input);

    let (component_hash, _) = w.put_literal(COMPONENT);
    let (input_hash, input_id) = w.put_literal(&input);
    let (swapped_hash, swapped_id) = w.claim_absent(&swapped);

    let recipe = Recipe {
        op: Op::Wasm {
            component: component_hash,
            world: WasmWorld::Transform2,
            export: "byteswap".into(),
        },
        inputs: vec![InputRef {
            hash: input_hash,
            role: None,
        }],
        outputs: vec![OutputRef {
            hash: swapped_hash,
            size: swapped.len() as u64,
            name: None,
        }],
        params: Vec::new(),
    };
    let recipe_id = w.mint_recipe(
        &recipe,
        "wasm:byteswap",
        OpKind::Wasm,
        SeekClass::Affine,
        &[(0, input_id)],
        &[(0, swapped_id, swapped.len() as u64)],
    );

    let exec = Executor::new(&w.store, ExecConfig::default()).expect("executor");
    exec.replay(&w.db, recipe_id).expect("replays");
    assert!(w.store.has(StoreNs::Data, &swapped_hash));
    assert_eq!(
        w.db.recipe_by_id(recipe_id).expect("row").verify,
        VerifyState::ReplayedLocal
    );
}

/// The composition shape D51 accepted thread costs for: an assemble
/// node whose child is a streaming wasm output that is NOT resident —
/// the executor streams through the guest (pipe) and spills for the
/// assemble node's random access, storing only the final output.
#[test]
fn composed_route_streams_through_wasm_without_storing_intermediates() {
    let mut w = world();
    let input = pattern(150_000);
    let swapped = byteswap(&input);
    let tail = pattern(5000);
    let mut fin = swapped.clone();
    fin.extend_from_slice(&tail);

    let (component_hash, _) = w.put_literal(COMPONENT);
    let (input_hash, input_id) = w.put_literal(&input);
    let (tail_hash, tail_id) = w.put_literal(&tail);
    let (swapped_hash, swapped_id) = w.claim_absent(&swapped);
    let (final_hash, final_id) = w.claim_absent(&fin);

    let swap_recipe = Recipe {
        op: Op::Wasm {
            component: component_hash,
            world: WasmWorld::Transform2,
            export: "byteswap".into(),
        },
        inputs: vec![InputRef {
            hash: input_hash,
            role: None,
        }],
        outputs: vec![OutputRef {
            hash: swapped_hash,
            size: swapped.len() as u64,
            name: None,
        }],
        params: Vec::new(),
    };
    w.mint_recipe(
        &swap_recipe,
        "wasm:byteswap",
        OpKind::Wasm,
        SeekClass::Affine,
        &[(0, input_id)],
        &[(0, swapped_id, swapped.len() as u64)],
    );

    let concat_recipe = Recipe {
        op: Op::Builtin {
            name: "assemble".into(),
            major: 1,
        },
        inputs: vec![
            InputRef {
                hash: swapped_hash,
                role: None,
            },
            InputRef {
                hash: tail_hash,
                role: None,
            },
        ],
        outputs: vec![OutputRef {
            hash: final_hash,
            size: fin.len() as u64,
            name: None,
        }],
        params: AssembleParams {
            segments: vec![
                Segment::BlobRange {
                    input_ix: 0,
                    offset: 0,
                    len: swapped.len() as u64,
                },
                Segment::BlobRange {
                    input_ix: 1,
                    offset: 0,
                    len: tail.len() as u64,
                },
            ],
        }
        .encode()
        .expect("params"),
    };
    let concat_id = w.mint_recipe(
        &concat_recipe,
        "assemble@1",
        OpKind::Builtin,
        SeekClass::Affine,
        &[(0, swapped_id), (1, tail_id)],
        &[(0, final_id, fin.len() as u64)],
    );

    let exec = Executor::new(&w.store, ExecConfig::default()).expect("executor");
    exec.materialize(&w.db, &final_hash).expect("materializes");
    assert!(w.store.has(StoreNs::Data, &final_hash), "final stored");
    assert!(
        !w.store.has(StoreNs::Data, &swapped_hash),
        "intermediate streamed, never stored"
    );
    assert_eq!(
        w.db.recipe_by_id(concat_id).expect("row").verify,
        VerifyState::ReplayedLocal
    );
    // Read back and compare.
    let mut got = Vec::new();
    std::io::Read::read_to_end(
        &mut w
            .store
            .get(StoreNs::Data, &final_hash)
            .expect("get")
            .expect("present"),
        &mut got,
    )
    .expect("read");
    assert_eq!(got, fin);

    // open_stream serves the still-absent intermediate without storing it.
    let mut streamed = Vec::new();
    std::io::Read::read_to_end(
        &mut exec.open_stream(&w.db, &swapped_hash).expect("route"),
        &mut streamed,
    )
    .expect("read");
    assert_eq!(streamed, swapped);
    assert!(!w.store.has(StoreNs::Data, &swapped_hash));
}

impl World {
    /// Simulate eviction the way M3's planner will do it: literal file
    /// deleted, `.obao` sidecar kept (D49 rule 1), residency flipped.
    fn evict(&self, hash: &Blake3) {
        let path = self
            ._dir
            .path()
            .join("store")
            .join(datboi_store_fs::layout::blob_path(StoreNs::Data, hash));
        std::fs::remove_file(&path).expect("literal existed");
        let id = self.db.get_blob_id(hash).expect("q").expect("indexed");
        self.db
            .set_residency(id, Residency::EvictedCovered)
            .expect("residency");
    }
}

#[test]
fn served_ranges_verify_after_eviction() {
    let mut w = world();
    let input = pattern(120_000);
    let swapped = byteswap(&input);

    let (component_hash, _) = w.put_literal(COMPONENT);
    let (input_hash, input_id) = w.put_literal(&input);
    let (swapped_hash, swapped_id) = w.claim_absent(&swapped);
    let recipe = Recipe {
        op: Op::Wasm {
            component: component_hash,
            world: WasmWorld::Transform2,
            export: "byteswap".into(),
        },
        inputs: vec![InputRef {
            hash: input_hash,
            role: None,
        }],
        outputs: vec![OutputRef {
            hash: swapped_hash,
            size: swapped.len() as u64,
            name: None,
        }],
        params: Vec::new(),
    };
    let recipe_id = w.mint_recipe(
        &recipe,
        "wasm:byteswap",
        OpKind::Wasm,
        SeekClass::Affine,
        &[(0, input_id)],
        &[(0, swapped_id, swapped.len() as u64)],
    );

    let exec = Executor::new(&w.store, ExecConfig::default()).expect("executor");
    exec.replay(&w.db, recipe_id).expect("replay licenses");
    w.evict(&swapped_hash);
    assert!(!w.store.has(StoreNs::Data, &swapped_hash));
    assert!(
        w.store
            .get_obao(StoreNs::Data, &swapped_hash)
            .expect("q")
            .is_some(),
        "outboard survives eviction (D49 rule 1)"
    );

    // Ranges at awkward offsets, all served via the wasm seek path and
    // verified against the output outboard.
    for (offset, len) in [
        (0u64, 100u64),
        (16 * 1024 - 1, 3),
        (99_990, 50),
        (119_999, 10),
    ] {
        let got = exec
            .serve_range(&w.db, &swapped_hash, offset, len)
            .expect("verified serve");
        let start = usize::try_from(offset.min(swapped.len() as u64)).expect("small");
        let end = (start + usize::try_from(len).expect("small")).min(swapped.len());
        assert_eq!(got, &swapped[start..end], "range {offset}+{len}");
    }
    assert!(
        !w.db.is_seek_quarantined(&component_hash).expect("q"),
        "honest component stays trusted"
    );
}

#[test]
fn lying_seek_path_is_quarantined_and_falls_back() {
    let mut w = world();
    let payload = pattern(80_000);
    let swapped = byteswap(&payload);

    let (component_hash, _) = w.put_literal(COMPONENT);
    let (payload_hash, payload_id) = w.put_literal(&payload);
    let (swapped_hash, swapped_id) = w.claim_absent(&swapped);
    let recipe = Recipe {
        op: Op::Wasm {
            component: component_hash,
            world: WasmWorld::Transform2,
            export: "byteswap-lying-range".into(),
        },
        inputs: vec![InputRef {
            hash: payload_hash,
            role: None,
        }],
        outputs: vec![OutputRef {
            hash: swapped_hash,
            size: swapped.len() as u64,
            name: None,
        }],
        params: Vec::new(),
    };
    let recipe_id = w.mint_recipe(
        &recipe,
        "wasm:byteswap-lying-range",
        OpKind::Wasm,
        SeekClass::Affine,
        &[(0, payload_id)],
        &[(0, swapped_id, swapped.len() as u64)],
    );

    let exec = Executor::new(&w.store, ExecConfig::default()).expect("executor");
    // Sequential run is honest: the claim replays and licenses (this is
    // exactly why claim-level verification can't catch seek bugs — D49).
    exec.replay(&w.db, recipe_id)
        .expect("honest sequential replay");
    w.evict(&swapped_hash);

    // First seeked read: the lying window fails output-bao verification;
    // no bytes are surfaced; the component's seekability is quarantined.
    let err = exec
        .serve_range(&w.db, &swapped_hash, 4096, 100)
        .expect_err("lie caught");
    assert!(
        matches!(err, ExecError::RangeVerifyFailed { .. }),
        "EIO class, never bytes: {err}"
    );
    assert!(
        w.db.is_seek_quarantined(&component_hash).expect("q"),
        "seek quarantine recorded (D49 rule 3)"
    );

    // Second read: planner treats the route as opaque — the known-good
    // sequential path (spill) serves it, verified, correct.
    let got = exec
        .serve_range(&w.db, &swapped_hash, 4096, 100)
        .expect("sequential fallback");
    assert_eq!(got, &swapped[4096..4196]);
}

/// Quarantine attribution (D49 rule 3, refined): a window-verify failure
/// caused by a CORRUPT INPUT must not defame the component. Same honest
/// component as `served_ranges_verify_after_eviction`, but the input
/// literal rots on disk after licensing — the seeked read fails, names
/// the corrupt input, and the component stays trusted.
#[test]
fn corrupt_input_mismatch_does_not_quarantine_the_component() {
    let mut w = world();
    let input = pattern(120_000);
    let swapped = byteswap(&input);

    let (component_hash, _) = w.put_literal(COMPONENT);
    let (input_hash, input_id) = w.put_literal(&input);
    let (swapped_hash, swapped_id) = w.claim_absent(&swapped);
    let recipe = Recipe {
        op: Op::Wasm {
            component: component_hash,
            world: WasmWorld::Transform2,
            export: "byteswap".into(),
        },
        inputs: vec![InputRef {
            hash: input_hash,
            role: None,
        }],
        outputs: vec![OutputRef {
            hash: swapped_hash,
            size: swapped.len() as u64,
            name: None,
        }],
        params: Vec::new(),
    };
    let recipe_id = w.mint_recipe(
        &recipe,
        "wasm:byteswap",
        OpKind::Wasm,
        SeekClass::Affine,
        &[(0, input_id)],
        &[(0, swapped_id, swapped.len() as u64)],
    );

    let exec = Executor::new(&w.store, ExecConfig::default()).expect("executor");
    exec.replay(&w.db, recipe_id).expect("replay licenses");
    w.evict(&swapped_hash);

    // Rot the input literal in place (bit flip mid-file). The index and
    // recipes still describe the clean bytes.
    let path = w
        ._dir
        .path()
        .join("store")
        .join(datboi_store_fs::layout::blob_path(
            StoreNs::Data,
            &input_hash,
        ));
    let mut bytes = std::fs::read(&path).expect("read literal");
    // Inside the window the seeked read will pull (affine route reads
    // only what it needs — corruption elsewhere is invisible to it).
    bytes[4_100] ^= 0xFF;
    std::fs::write(&path, &bytes).expect("rot literal");

    let err = exec
        .serve_range(&w.db, &swapped_hash, 4096, 100)
        .expect_err("mismatch surfaces as an error, never bytes");
    match &err {
        ExecError::RangeVerifyFailed { detail, .. } => {
            assert!(
                detail.contains(&input_hash.to_hex()),
                "detail names the corrupt input: {detail}"
            );
            assert!(
                detail.contains("not quarantined"),
                "detail says the component was spared: {detail}"
            );
        }
        other => panic!("expected RangeVerifyFailed, got {other}"),
    }
    assert!(
        !w.db.is_seek_quarantined(&component_hash).expect("q"),
        "corrupt inputs must not indict the component"
    );
}

/// Rehabilitation: a WRONGLY-poisoned recipe (the pipe-race class of
/// host bug) exits `Failed` through exactly one door — a verified
/// re-execution — while a genuinely bad claim stays poisoned.
#[test]
fn rehabilitation_clears_wrong_poison_but_not_bad_claims() {
    let mut w = world();
    let input = pattern(50_000);
    let swapped = byteswap(&input);

    // A perfectly good recipe, poisoned by fiat (simulating the host bug).
    let (component_hash, _) = w.put_literal(COMPONENT);
    let (input_hash, input_id) = w.put_literal(&input);
    let (swapped_hash, swapped_id) = w.claim_absent(&swapped);
    let good = Recipe {
        op: Op::Wasm {
            component: component_hash,
            world: WasmWorld::Transform2,
            export: "byteswap".into(),
        },
        inputs: vec![InputRef {
            hash: input_hash,
            role: None,
        }],
        outputs: vec![OutputRef {
            hash: swapped_hash,
            size: swapped.len() as u64,
            name: None,
        }],
        params: Vec::new(),
    };
    let good_id = w.mint_recipe(
        &good,
        "wasm:byteswap",
        OpKind::Wasm,
        SeekClass::Affine,
        &[(0, input_id)],
        &[(0, swapped_id, swapped.len() as u64)],
    );
    w.db.set_verify_state(
        good_id,
        VerifyAdvance::Failed {
            error: "simulated host bug",
            peer: None,
        },
        0,
    )
    .expect("poison by fiat");

    // A genuinely bad claim: same op, wrong claimed output hash.
    let bogus = Blake3::compute(b"never these bytes");
    let (_, bogus_id) = w.claim_absent(b"never these bytes");
    let bad = Recipe {
        op: Op::Wasm {
            component: component_hash,
            world: WasmWorld::Transform2,
            export: "byteswap".into(),
        },
        inputs: vec![InputRef {
            hash: input_hash,
            role: None,
        }],
        outputs: vec![OutputRef {
            hash: bogus,
            size: 17,
            name: None,
        }],
        params: Vec::new(),
    };
    let bad_id = w.mint_recipe(
        &bad,
        "wasm:byteswap",
        OpKind::Wasm,
        SeekClass::Affine,
        &[(0, input_id)],
        &[(0, bogus_id, 17)],
    );
    w.db.set_verify_state(
        bad_id,
        VerifyAdvance::Failed {
            error: "real poison",
            peer: None,
        },
        0,
    )
    .expect("poison");

    let exec = Executor::new(&w.store, ExecConfig::default()).expect("executor");

    // Poisoned recipes refuse ordinary replay...
    assert!(matches!(
        exec.replay(&w.db, good_id),
        Err(ExecError::Poisoned(_))
    ));
    // ...but rehabilitation re-executes and clears the wrong poison.
    exec.rehabilitate(&w.db, good_id).expect("rehabilitated");
    assert_eq!(
        w.db.recipe_by_id(good_id).expect("row").verify,
        VerifyState::ReplayedLocal
    );
    assert!(w.store.has(StoreNs::Data, &swapped_hash));

    // The genuinely bad claim fails re-execution and stays poisoned.
    assert!(exec.rehabilitate(&w.db, bad_id).is_err());
    assert_eq!(
        w.db.recipe_by_id(bad_id).expect("row").verify,
        VerifyState::Failed
    );
}

/// A future or foreign world is REFUSABLE, never poisonable: it must not
/// prefix-match onto a frozen ABI ("@10" driven as "@1", "@2.0.0" as
/// "@2") and its refusal must not fail the claim.
#[test]
fn unknown_worlds_refuse_instead_of_dispatching_or_poisoning() {
    let mut w = world();
    let input = pattern(4096);
    let (component_hash, _) = w.put_literal(COMPONENT);
    let (input_hash, input_id) = w.put_literal(&input);
    let (output_hash, output_id) = w.claim_absent(&byteswap(&input));

    let mut minted = Vec::new();
    for world_str in ["datboi:transform@10", "datboi:transform@2.0.0"] {
        let recipe = Recipe {
            op: Op::Wasm {
                component: component_hash,
                world: WasmWorld::parse(world_str),
                export: "byteswap".into(),
            },
            inputs: vec![InputRef {
                hash: input_hash,
                role: None,
            }],
            outputs: vec![OutputRef {
                hash: output_hash,
                size: input.len() as u64,
                name: None,
            }],
            params: Vec::new(),
        };
        let recipe_id = w.mint_recipe(
            &recipe,
            "wasm:byteswap",
            OpKind::Wasm,
            SeekClass::Opaque,
            &[(0, input_id)],
            &[(0, output_id, input.len() as u64)],
        );
        minted.push((world_str, recipe_id));
    }

    let exec = Executor::new(&w.store, ExecConfig::default()).expect("executor");
    for (world_str, recipe_id) in minted {
        let err = exec
            .replay(&w.db, recipe_id)
            .expect_err("unknown world must refuse");
        assert!(
            matches!(&err, ExecError::UnsupportedOp(w) if w == world_str),
            "{world_str}: {err}"
        );
        assert!(!err.is_claim_failure(), "{world_str} stays retryable");
        assert_ne!(
            w.db.recipe_by_id(recipe_id).expect("row").verify,
            VerifyState::Failed,
            "{world_str} must not poison"
        );
    }
}

/// The extractor world sanctions exactly one export: a recipe naming any
/// other must refuse at dispatch (identity skew — it would silently run
/// `extract` under a different recipe hash), not execute and not poison.
#[test]
fn extractor_recipe_with_wrong_export_refuses() {
    let mut w = world();
    let (container_hash, container_id) = w.put_literal(&pattern(4096));
    let (component_hash, _) = w.put_literal(datboi_ingest::EX_UNRAR_WASM);
    let (member_hash, member_id) = w.claim_absent(b"member bytes");
    let recipe = Recipe {
        op: Op::Wasm {
            component: component_hash,
            world: WasmWorld::Extractor1,
            export: "garbage".into(),
        },
        inputs: vec![InputRef {
            hash: container_hash,
            role: None,
        }],
        outputs: vec![OutputRef {
            hash: member_hash,
            size: 12,
            name: None,
        }],
        params: cbor::encode(&Value::Map(vec![(1, Value::Uint(0))])).expect("params"),
    };
    let recipe_id = w.mint_recipe(
        &recipe,
        "ex-unrar/garbage",
        OpKind::Wasm,
        SeekClass::Opaque,
        &[(0, container_id)],
        &[(0, member_id, 12)],
    );

    let exec = Executor::new(&w.store, ExecConfig::default()).expect("executor");
    let err = exec
        .replay(&w.db, recipe_id)
        .expect_err("wrong export must refuse");
    assert!(
        matches!(&err, ExecError::UnsupportedOp(msg) if msg.contains("garbage")),
        "{err}"
    );
    assert!(!err.is_claim_failure(), "refusal stays retryable");
    assert_ne!(
        w.db.recipe_by_id(recipe_id).expect("row").verify,
        VerifyState::Failed,
        "wrong export must not poison"
    );
}

/// An extractor guest that refuses (or traps) during replay is a CLAIM
/// failure: the typed verdict must survive the pipe path and poison the
/// recipe, instead of dissolving into retryable consumer I/O that
/// maintenance re-runs forever.
#[test]
fn extractor_guest_failure_poisons_the_recipe() {
    let mut w = world();
    // Not a rar: ex-unrar refuses it deterministically.
    let container = pattern(4096);
    let (container_hash, container_id) = w.put_literal(&container);
    let (component_hash, _) = w.put_literal(datboi_ingest::EX_UNRAR_WASM);
    let (member_lie, member_id) = w.claim_absent(b"member that never decodes");
    let recipe = Recipe {
        op: Op::Wasm {
            component: component_hash,
            world: WasmWorld::Extractor1,
            export: "extract".into(),
        },
        inputs: vec![InputRef {
            hash: container_hash,
            role: None,
        }],
        outputs: vec![OutputRef {
            hash: member_lie,
            size: 25,
            name: Some("VERSION".into()),
        }],
        params: cbor::encode(&Value::Map(vec![(1, Value::Uint(0))])).expect("params"),
    };
    let recipe_id = w.mint_recipe(
        &recipe,
        "ex-unrar/extract",
        OpKind::Wasm,
        SeekClass::Opaque,
        &[(0, container_id)],
        &[(0, member_id, 25)],
    );

    let exec = Executor::new(&w.store, ExecConfig::default()).expect("executor");
    let err = exec
        .replay(&w.db, recipe_id)
        .expect_err("garbage container cannot extract");
    assert!(
        matches!(err, ExecError::Runtime(_)),
        "typed guest verdict surfaced: {err}"
    );
    assert!(err.is_claim_failure(), "guest refusal indicts the claim");
    assert!(
        !w.store.has(StoreNs::Data, &member_lie),
        "nothing published"
    );
    assert_eq!(
        w.db.recipe_by_id(recipe_id).expect("row").verify,
        VerifyState::Failed,
        "poisoned (D25)"
    );
}

/// A child recipe lying about its output LENGTH must not poison the
/// parent: the truncated sequential input surfaces as the typed
/// mismatch naming the input, never as the parent's hash-mismatch
/// poison.
#[test]
fn child_length_lie_does_not_poison_the_parent() {
    let mut w = world();
    let real = pattern(1000);
    let (real_hash, real_id) = w.put_literal(&real);
    let (component_hash, _) = w.put_literal(COMPONENT);

    // Child: an assemble slicing the full 1000 bytes, CLAIMING 2000.
    let fake = Blake3::compute(b"claims 2000, produces 1000");
    let fake_id =
        w.db.upsert_blob(&fake, Some(2000), IndexNs::Data, Residency::Absent)
            .expect("row");
    let child = Recipe {
        op: Op::Builtin {
            name: "assemble".into(),
            major: 1,
        },
        inputs: vec![InputRef {
            hash: real_hash,
            role: None,
        }],
        outputs: vec![OutputRef {
            hash: fake,
            size: 2000,
            name: None,
        }],
        params: AssembleParams {
            segments: vec![Segment::BlobRange {
                input_ix: 0,
                offset: 0,
                len: 1000,
            }],
        }
        .encode()
        .expect("params"),
    };
    w.mint_recipe(
        &child,
        "assemble@1",
        OpKind::Builtin,
        SeekClass::Affine,
        &[(0, real_id)],
        &[(0, fake_id, 2000)],
    );

    // Parent: byteswap over the child's claimed output.
    let out = Blake3::compute(b"parent output, never produced");
    let out_id =
        w.db.upsert_blob(&out, Some(2000), IndexNs::Data, Residency::Absent)
            .expect("row");
    let parent = Recipe {
        op: Op::Wasm {
            component: component_hash,
            world: WasmWorld::Transform2,
            export: "byteswap".into(),
        },
        inputs: vec![InputRef {
            hash: fake,
            role: None,
        }],
        outputs: vec![OutputRef {
            hash: out,
            size: 2000,
            name: None,
        }],
        params: Vec::new(),
    };
    let parent_id = w.mint_recipe(
        &parent,
        "wasm:byteswap",
        OpKind::Wasm,
        SeekClass::Affine,
        &[(0, fake_id)],
        &[(0, out_id, 2000)],
    );

    let exec = Executor::new(&w.store, ExecConfig::default()).expect("executor");
    let err = exec
        .replay(&w.db, parent_id)
        .expect_err("truncated input must surface");
    match &err {
        ExecError::Runtime(datboi_runtime::RuntimeError::InputLengthMismatch {
            input_ix,
            claimed,
            actual,
        }) => assert_eq!((*input_ix, *claimed, *actual), (0, 2000, 1000)),
        other => panic!("expected InputLengthMismatch, got {other}"),
    }
    assert!(
        !err.is_claim_failure(),
        "the parent's claim was never tested"
    );
    assert_ne!(
        w.db.recipe_by_id(parent_id).expect("row").verify,
        VerifyState::Failed,
        "the child's lie must not poison the parent"
    );
}

#[test]
fn no_route_is_a_clean_error() {
    let w = world();
    let exec = Executor::new(&w.store, ExecConfig::default()).expect("executor");
    let missing = Blake3::compute(b"nobody claims me");
    assert!(matches!(
        exec.materialize(&w.db, &missing),
        Err(ExecError::NoRoute(_))
    ));
}

/// D56 headroom guard: a claimed output too large for the store's
/// filesystem refuses cleanly BEFORE any replay writes bytes.
#[test]
fn materialize_refuses_without_disk_headroom() {
    let mut w = world();
    let exec = Executor::new(&w.store, ExecConfig::default()).expect("executor");

    // An absent blob claiming ~1 EiB, with a (row-level) covering
    // recipe so the route exists — the guard must fire first.
    let huge = Blake3::compute(b"claimed enormous output");
    let huge_id =
        w.db.upsert_blob(&huge, Some(1 << 60), IndexNs::Data, Residency::Absent)
            .expect("upsert");
    let meta = Blake3::compute(b"headroom recipe object");
    let meta_id =
        w.db.upsert_blob(&meta, Some(32), IndexNs::Meta, Residency::Resident)
            .expect("upsert meta");
    w.db.insert_recipe(&NewRecipe {
        blob_id: meta_id,
        op_kind: OpKind::Builtin,
        op_name: "assemble@1",
        seek_class: SeekClass::Affine,
        source: RecipeSource::LocalIngest,
        inputs: &[],
        outputs: &[(0, huge_id, 1 << 60, None)],
    })
    .expect("insert");

    match exec.materialize(&w.db, &huge) {
        Err(ExecError::InsufficientHeadroom { need, have, .. }) => {
            assert!(need > have, "guard arithmetic: need {need} > have {have}");
        }
        other => panic!("expected InsufficientHeadroom, got {other:?}"),
    }
}
