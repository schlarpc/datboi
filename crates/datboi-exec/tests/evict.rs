//! Eviction safety (D21/D25/D27/D49): the byte-destroying path refuses
//! everything the rules forbid and leaves evicted content fully servable.

use std::io::Write as _;

use datboi_core::cbor::{self, Value};
use datboi_core::hash::Blake3;
use datboi_core::recipe::{InputRef, Op, OutputRef, Recipe};
use datboi_exec::evict::{Blocked, EvictOutcome};
use datboi_exec::{ExecConfig, Executor};
use datboi_index::recipes::NewRecipe;
use datboi_index::{
    Db, Namespace as IndexNs, OpKind, RecipeSource, Residency, SeekClass, VerifyState,
};
use datboi_store_fs::{Namespace as StoreNs, Store};
use flate2::Compression;
use flate2::write::DeflateEncoder;

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
    let mut state: u64 = 0x1357_9BDF_2468_ACE0;
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

    fn mint_deflate_recipe(
        &mut self,
        container: &[u8],
        member: &[u8],
        offset: u64,
        comp_len: u64,
    ) -> i64 {
        let container_hash = Blake3::compute(container);
        let container_id = self
            .db
            .get_blob_id(&container_hash)
            .expect("q")
            .expect("row");
        let member_hash = Blake3::compute(member);
        let member_id = self
            .db
            .upsert_blob(
                &member_hash,
                Some(member.len() as u64),
                IndexNs::Data,
                Residency::Absent,
            )
            .expect("row");
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
                name: None,
            }],
            params: cbor::encode(&Value::Map(vec![
                (1, Value::Uint(offset)),
                (2, Value::Uint(comp_len)),
            ]))
            .expect("params"),
        };
        let encoded = recipe.encode().expect("valid");
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
            .expect("row");
        self.db
            .insert_recipe(&NewRecipe {
                blob_id: recipe_blob_id,
                op_kind: OpKind::Builtin,
                op_name: "deflate-decompress@1",
                seek_class: SeekClass::Opaque,
                source: RecipeSource::LocalIngest,
                inputs: &[(0, container_id, None)],
                outputs: &[(0, member_id, member.len() as u64, None)],
            })
            .expect("recipe row")
    }
}

fn deflate(bytes: &[u8]) -> Vec<u8> {
    let mut enc = DeflateEncoder::new(Vec::new(), Compression::default());
    enc.write_all(bytes).expect("deflate");
    enc.finish().expect("finish")
}

#[test]
fn eviction_enforces_every_rule_then_serves_from_the_recipe() {
    let mut w = world();
    let member = pattern(200_000);
    let compressed = deflate(&member);
    let mut container = b"hdr".to_vec();
    container.extend_from_slice(&compressed);
    let (container_hash, _) = w.put_literal(&container);
    let member_hash = Blake3::compute(&member);
    let recipe_id = w.mint_deflate_recipe(&container, &member, 3, compressed.len() as u64);

    let exec = Executor::new(&w.store, ExecConfig::default()).expect("executor");

    // An uncovered literal (the container) may never be evicted.
    let out = exec.evict(&w.db, &container_hash).expect("checked");
    assert!(matches!(out, EvictOutcome::Blocked(Blocked::NotGrounded)));

    // Materialize the member (replays the recipe → ReplayedLocal).
    exec.materialize(&w.db, &member_hash).expect("materializes");
    assert!(w.store.has(StoreNs::Data, &member_hash));

    // Now the member is evictable: D25 license exists, grounding holds
    // through the retained container.
    let out = exec.evict(&w.db, &member_hash).expect("evicts");
    let EvictOutcome::Evicted { bytes_reclaimed } = out else {
        panic!("expected eviction, got {out:?}");
    };
    assert_eq!(bytes_reclaimed, member.len() as u64);
    assert!(!w.store.has(StoreNs::Data, &member_hash), "bytes gone");
    assert!(
        w.store
            .get_obao(StoreNs::Data, &member_hash)
            .expect("q")
            .is_some(),
        "outboard survives (D49 rule 1)"
    );
    assert_eq!(
        w.db.blob_by_hash(&member_hash)
            .expect("q")
            .expect("row")
            .residency,
        Residency::EvictedCovered
    );

    // Evicting again is a clean no-op.
    assert!(matches!(
        exec.evict(&w.db, &member_hash).expect("idempotent"),
        EvictOutcome::Blocked(Blocked::NotResident)
    ));

    // The container is still not evictable: dropping it would strand the
    // member (grounding is computed against the post-drop world).
    assert!(matches!(
        exec.evict(&w.db, &container_hash).expect("checked"),
        EvictOutcome::Blocked(Blocked::NotGrounded)
    ));

    // Evicted content still serves: sequential and verified ranges.
    let mut streamed = Vec::new();
    std::io::Read::read_to_end(
        &mut exec.open_stream(&w.db, &member_hash).expect("route"),
        &mut streamed,
    )
    .expect("read");
    assert_eq!(streamed, member);
    assert_eq!(
        exec.serve_range(&w.db, &member_hash, 100_001, 77)
            .expect("range"),
        &member[100_001..100_078]
    );

    // And materialize brings the literal back (rematerialization).
    exec.materialize(&w.db, &member_hash)
        .expect("rematerializes");
    assert!(w.store.has(StoreNs::Data, &member_hash));
    assert_eq!(
        w.db.blob_by_hash(&member_hash)
            .expect("q")
            .expect("row")
            .residency,
        Residency::Resident
    );
    assert_eq!(
        recipe_id,
        w.db.recipes_for_output(w.db.get_blob_id(&member_hash).expect("q").expect("row"))
            .expect("q")[0]
            .recipe_id
    );
}

/// The D21 grounding trap: headered ↔ headerless style mutually-inverse
/// recipes must never license dropping BOTH literals.
#[test]
fn mutually_inverse_pair_cannot_both_evict() {
    let mut w = world();
    let body = pattern(40_000);
    let mut headered = b"HEADER!!".to_vec();
    headered.extend_from_slice(&body);

    let (headered_hash, headered_id) = w.put_literal(&headered);
    let (header_hash, header_id) = w.put_literal(b"HEADER!!");
    let body_hash = Blake3::compute(&body);
    let body_id =
        w.db.upsert_blob(
            &body_hash,
            Some(body.len() as u64),
            IndexNs::Data,
            Residency::Absent,
        )
        .expect("row");

    let mint = |w: &mut World,
                recipe: &Recipe,
                inputs: &[(u32, i64)],
                outputs: &[(u32, i64, u64)]|
     -> i64 {
        let encoded = recipe.encode().expect("valid");
        let recipe_hash = Blake3::compute(&encoded);
        w.store
            .put(StoreNs::Meta, recipe_hash, encoded.as_slice())
            .expect("blob");
        let blob_id =
            w.db.upsert_blob(
                &recipe_hash,
                Some(encoded.len() as u64),
                IndexNs::Meta,
                Residency::Resident,
            )
            .expect("row");
        let inputs: Vec<(u32, i64, Option<&str>)> =
            inputs.iter().map(|(p, i)| (*p, *i, None)).collect();
        let outputs: Vec<(u32, i64, u64, Option<&str>)> =
            outputs.iter().map(|(o, i, s)| (*o, *i, *s, None)).collect();
        w.db.insert_recipe(&NewRecipe {
            blob_id,
            op_kind: OpKind::Builtin,
            op_name: "assemble@1",
            seek_class: SeekClass::Affine,
            source: RecipeSource::LocalIngest,
            inputs: &inputs,
            outputs: &outputs,
        })
        .expect("recipe row")
    };

    // derive: body = slice(headered)
    let derive = Recipe {
        op: Op::Builtin {
            name: "assemble".into(),
            major: 1,
        },
        inputs: vec![InputRef {
            hash: headered_hash,
            role: None,
        }],
        outputs: vec![OutputRef {
            hash: body_hash,
            size: body.len() as u64,
            name: None,
        }],
        params: datboi_core::assemble::AssembleParams {
            segments: vec![datboi_core::assemble::Segment::BlobRange {
                input_ix: 0,
                offset: 8,
                len: body.len() as u64,
            }],
        }
        .encode()
        .expect("params"),
    };
    let derive_id = mint(
        &mut w,
        &derive,
        &[(0, headered_id)],
        &[(0, body_id, body.len() as u64)],
    );

    // rebuild: headered = header + body
    let rebuild = Recipe {
        op: Op::Builtin {
            name: "assemble".into(),
            major: 1,
        },
        inputs: vec![
            InputRef {
                hash: header_hash,
                role: None,
            },
            InputRef {
                hash: body_hash,
                role: None,
            },
        ],
        outputs: vec![OutputRef {
            hash: headered_hash,
            size: headered.len() as u64,
            name: None,
        }],
        params: datboi_core::assemble::AssembleParams {
            segments: vec![
                datboi_core::assemble::Segment::BlobRange {
                    input_ix: 0,
                    offset: 0,
                    len: 8,
                },
                datboi_core::assemble::Segment::BlobRange {
                    input_ix: 1,
                    offset: 0,
                    len: body.len() as u64,
                },
            ],
        }
        .encode()
        .expect("params"),
    };
    let rebuild_id = mint(
        &mut w,
        &rebuild,
        &[(0, header_id), (1, body_id)],
        &[(0, headered_id, headered.len() as u64)],
    );

    let exec = Executor::new(&w.store, ExecConfig::default()).expect("executor");
    // License everything that can be licensed.
    exec.replay(&w.db, derive_id).expect("derive replays");
    exec.replay(&w.db, rebuild_id).expect("rebuild replays");
    assert_eq!(
        w.db.recipe_by_id(derive_id).expect("q").verify,
        VerifyState::ReplayedLocal
    );
    assert_eq!(
        w.db.recipe_by_id(rebuild_id).expect("q").verify,
        VerifyState::ReplayedLocal
    );
    // (replay stored the body literal as a side effect — both ends are
    // now resident, both covered by replayed recipes: maximum danger.)

    // Planner sweep: evict everything evictable. Exactly ONE of the
    // inverse pair may go; the fixpoint keeps the other.
    let report = exec.evict_covered(&w.db, 0, false).expect("planner");
    let headered_resident = w.store.has(StoreNs::Data, &headered_hash);
    let body_resident = w.store.has(StoreNs::Data, &body_hash);
    assert!(
        headered_resident != body_resident,
        "exactly one of the inverse pair survives (headered={headered_resident}, body={body_resident})"
    );
    assert!(report.evicted >= 1);

    // Whichever went, it still materializes back.
    let gone = if headered_resident {
        body_hash
    } else {
        headered_hash
    };
    exec.materialize(&w.db, &gone).expect("rematerializes");
    assert!(w.store.has(StoreNs::Data, &gone));
}
