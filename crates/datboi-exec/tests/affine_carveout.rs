//! D63 gate: the affine carve-out's seek-equivalence property over a
//! synthesized FAT32 image recipe (random + boundary-straddling ranges
//! ≡ slices of the full sequential materialization), the predicate's
//! refusal matrix, and blessing promotion to full D49.

use std::io::Read as _;

use datboi_catalog::{ImageParams, ImageReport, mint_image};
use datboi_core::hash::Blake3;
use datboi_core::viewsnap::{ViewRow, ViewSnapshot};
use datboi_exec::{ExecConfig, ExecError, Executor};
use datboi_index::{Db, Namespace as IndexNs, Residency};
use datboi_store_fs::{Namespace as StoreNs, Store};

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

fn content(seed: u8, len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| (usize::from(seed) * 131 + i * 11) as u8)
        .collect()
}

fn put_content(w: &mut World, bytes: &[u8]) -> Blake3 {
    let hash = Blake3::compute(bytes);
    w.store.put(StoreNs::Data, hash, bytes).expect("put");
    let id =
        w.db.upsert_blob(
            &hash,
            Some(bytes.len() as u64),
            IndexNs::Data,
            Residency::Resident,
        )
        .expect("upsert");
    w.db.set_verified(id, 1).expect("verified");
    hash
}

/// Mint a no-obao image over a mixed snapshot: multi-cluster files, a
/// zero-size file, nesting, and one blob big enough (>16 KiB) that the
/// carve-out's leaf reader needs a real sidecar built on demand.
fn minted(w: &mut World) -> (ImageReport, Vec<(String, u64)>) {
    let files = vec![
        ("Alpha (USA).gba".to_owned(), 700u64),
        ("big blob straddles groups.bin".to_owned(), 100_000u64),
        ("empty.sav".to_owned(), 0u64),
        ("sub/Beta (Europe).gba".to_owned(), 1500u64),
        ("sub/deep/Gamma.gba".to_owned(), 512u64),
    ];
    let rows = files
        .iter()
        .enumerate()
        .map(|(i, (path, size))| ViewRow {
            path: path.clone(),
            hash: put_content(
                w,
                &content(
                    u8::try_from(i + 1).expect("small"),
                    usize::try_from(*size).expect("small"),
                ),
            ),
            size: *size,
            seek: 0,
        })
        .collect();
    let snap = ViewSnapshot {
        created_at: 1_780_000_000,
        view_name: "carveout".into(),
        sources: vec![],
        rows,
    };
    let snap_hash = Blake3::compute(b"carveout gate snapshot");
    let report = mint_image(
        &mut w.db,
        &w.store,
        "carveout",
        &snap_hash,
        &snap,
        &ImageParams {
            cluster_size: 512,
            partition: true,
            label: None,
        },
        false, // no output obao: serving must take the D63 path
        7,
    )
    .expect("mint");
    assert!(
        w.store
            .get_obao(StoreNs::Data, &report.image)
            .expect("get_obao")
            .is_none(),
        "gate precondition: no output sidecar"
    );
    (report, files)
}

fn full_materialization(exec: &Executor, db: &Db, report: &ImageReport) -> Vec<u8> {
    let mut full = Vec::with_capacity(usize::try_from(report.size).expect("fits"));
    exec.open_stream(db, &report.image)
        .expect("open_stream")
        .read_to_end(&mut full)
        .expect("read full");
    assert_eq!(full.len() as u64, report.size);
    // Independent of the serving path: the sequential materialization
    // hashes to the claimed output.
    assert_eq!(Blake3::compute(&full), report.image, "claimed output hash");
    full
}

/// The D63 seek-equivalence property: carve-out range reads must
/// byte-match slices of the full materialization, with ranges at ±1 of
/// every interesting boundary plus deterministic pseudo-random ones.
#[test]
fn carveout_seek_equivalence() {
    let mut w = world();
    let (report, files) = minted(&mut w);
    let exec = Executor::new(&w.store, ExecConfig::default()).expect("executor");
    let full = full_materialization(&exec, &w.db, &report);
    let total = report.size;

    // Boundary offsets, recomputed from the same pure layout math the
    // mint used (deterministic): partition start, FAT copies, data
    // region, every file segment start, EOF.
    let layout = datboi_catalog::fat32::layout(
        &files
            .iter()
            .map(|(path, size)| datboi_catalog::fat32::FileEntry {
                path: path.clone(),
                size: *size,
            })
            .collect::<Vec<_>>(),
        &datboi_catalog::fat32::Fat32Params {
            volume_label: datboi_catalog::fat32::label_for("carveout"),
            serial: u32::from_le_bytes(
                Blake3::compute(b"carveout gate snapshot").0[0..4]
                    .try_into()
                    .expect("4"),
            ),
            disk_signature: u32::from_le_bytes(
                Blake3::compute(b"carveout gate snapshot").0[4..8]
                    .try_into()
                    .expect("4"),
            ),
            cluster_size: 512,
            partition: true,
        },
    )
    .expect("layout");
    assert_eq!(layout.geometry.total_size, total, "same layout as mint");
    let mut boundaries = vec![
        0,
        layout.geometry.partition_offset,
        layout.geometry.fat_offset,
        layout.geometry.fat_offset + layout.geometry.fat_bytes,
        layout.geometry.data_offset,
        total - 1,
        total,
    ];
    let mut off = 0u64;
    for s in &layout.segments {
        boundaries.push(off);
        off += datboi_catalog::fat32::segment_len(s);
    }

    let mut checked = 0usize;
    let mut check = |start: u64, len: u64| {
        let got = exec
            .serve_range(&w.db, &report.image, start, len)
            .expect("carve-out serve");
        let s = usize::try_from(start.min(total)).expect("fits");
        let e = usize::try_from(start.saturating_add(len).min(total)).expect("fits");
        assert_eq!(got, &full[s..e], "range ({start}, {len})");
        checked += 1;
    };

    for &b in &boundaries {
        check(b.saturating_sub(1), 3);
        check(b, 1);
        check(b.saturating_sub(4096), 8192);
    }
    // Whole image in one request (len clamps at EOF).
    check(0, u64::MAX);
    // Past EOF: empty, not an error.
    check(total + 10, 5);
    // Deterministic pseudo-random ranges (the streaming.rs pattern).
    let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
    for _ in 0..64 {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        let start = state % total;
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        let len = 1 + state % 8192;
        check(start, len);
    }
    assert!(checked > 80, "the gate actually ran ({checked} ranges)");
}

/// A computed node in the route (deflate) disqualifies: no sidecar ⇒
/// MissingOutboard, exactly the pre-D63 floor.
#[test]
fn computed_route_still_requires_outboard() {
    use datboi_core::cbor::{self, Value};
    use datboi_core::recipe::{InputRef, Op, OutputRef, Recipe};
    use datboi_index::recipes::NewRecipe;
    use datboi_index::{OpKind, RecipeSource, SeekClass, VerifyState};
    use flate2::Compression;
    use flate2::write::DeflateEncoder;
    use std::io::Write as _;

    let mut w = world();
    let plain = content(9, 60_000);
    let plain_hash = Blake3::compute(&plain);
    let mut enc = DeflateEncoder::new(Vec::new(), Compression::default());
    enc.write_all(&plain).expect("compress");
    let deflated = enc.finish().expect("compress");
    let container_hash = put_content(&mut w, &deflated);

    // deflate-decompress@1 claiming the plaintext, minted exactly as
    // ingest would (LocalIngest + Verified) — the predicate must still
    // refuse it: deflate is computed, not affine arithmetic.
    let params = cbor::encode(&Value::Map(vec![
        (1, Value::Uint(0)),
        (2, Value::Uint(deflated.len() as u64)),
    ]))
    .expect("params");
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
            hash: plain_hash,
            size: plain.len() as u64,
            name: None,
        }],
        params,
    };
    let encoded = recipe.encode().expect("encode");
    let recipe_hash = Blake3::compute(&encoded);
    w.store
        .put(StoreNs::Meta, recipe_hash, encoded.as_slice())
        .expect("put recipe");
    let recipe_blob =
        w.db.upsert_blob(
            &recipe_hash,
            Some(encoded.len() as u64),
            IndexNs::Meta,
            Residency::Resident,
        )
        .expect("upsert");
    let container_blob =
        w.db.blob_by_hash(&container_hash)
            .expect("row")
            .expect("some")
            .blob_id;
    let out_blob =
        w.db.upsert_blob(
            &plain_hash,
            Some(plain.len() as u64),
            IndexNs::Data,
            Residency::Absent,
        )
        .expect("upsert");
    let recipe_id =
        w.db.insert_recipe(&NewRecipe {
            blob_id: recipe_blob,
            op_kind: OpKind::Builtin,
            op_name: "deflate-decompress@1",
            seek_class: SeekClass::ManifestSeekable,
            source: RecipeSource::LocalIngest,
            inputs: &[(0, container_blob, None)],
            outputs: &[(0, out_blob, plain.len() as u64, None)],
        })
        .expect("insert");
    w.db.set_verify_state(recipe_id, VerifyState::Verified, 1, None)
        .expect("verify");

    let exec = Executor::new(&w.store, ExecConfig::default()).expect("executor");
    let err = exec
        .serve_range(&w.db, &plain_hash, 100, 64)
        .expect_err("must refuse");
    assert!(matches!(err, ExecError::MissingOutboard(_)), "got {err}");
}

#[test]
fn peer_sourced_recipe_declined() {
    let mut w = world();
    let (report, _) = minted(&mut w);
    let image_blob =
        w.db.blob_by_hash(&report.image)
            .expect("row")
            .expect("some")
            .blob_id;
    let recipe_id = w.db.recipes_for_output(image_blob).expect("recipes")[0].recipe_id;
    // Rewrite provenance to Peer (code 1): locally-minted no more.
    w.db.cache()
        .execute(
            "UPDATE recipe SET source = 1 WHERE recipe_id = ?1",
            [recipe_id],
        )
        .expect("update");
    let exec = Executor::new(&w.store, ExecConfig::default()).expect("executor");
    let err = exec
        .serve_range(&w.db, &report.image, 0, 64)
        .expect_err("must refuse");
    assert!(matches!(err, ExecError::MissingOutboard(_)), "got {err}");
}

#[test]
fn unverified_leaf_declined() {
    let mut w = world();
    let (report, files) = minted(&mut w);
    // NULL one content leaf's verified_at: "over verified inputs" fails.
    let big = content(2, usize::try_from(files[1].1).expect("small"));
    let leaf =
        w.db.blob_by_hash(&Blake3::compute(&big))
            .expect("row")
            .expect("some")
            .blob_id;
    w.db.cache()
        .execute(
            "UPDATE blob SET verified_at = NULL WHERE blob_id = ?1",
            [leaf],
        )
        .expect("update");
    let exec = Executor::new(&w.store, ExecConfig::default()).expect("executor");
    let err = exec
        .serve_range(&w.db, &report.image, 0, 64)
        .expect_err("must refuse");
    assert!(matches!(err, ExecError::MissingOutboard(_)), "got {err}");
}

#[test]
fn non_resident_leaf_declined() {
    let mut w = world();
    let (report, files) = minted(&mut w);
    let big = content(2, usize::try_from(files[1].1).expect("small"));
    let hash = Blake3::compute(&big);
    w.db.upsert_blob(&hash, Some(files[1].1), IndexNs::Data, Residency::Absent)
        .expect("upsert");
    let exec = Executor::new(&w.store, ExecConfig::default()).expect("executor");
    let err = exec
        .serve_range(&w.db, &report.image, 0, 64)
        .expect_err("must refuse");
    assert!(matches!(err, ExecError::MissingOutboard(_)), "got {err}");
}

/// Blessing promotion: bless_output computes and caches the output
/// obao; subsequent serves take the full-D49 verify path and still
/// byte-match.
#[test]
fn blessing_promotes_to_full_d49() {
    let mut w = world();
    let (report, _) = minted(&mut w);
    let exec = Executor::new(&w.store, ExecConfig::default()).expect("executor");
    let full = full_materialization(&exec, &w.db, &report);

    let before = exec
        .serve_range(&w.db, &report.image, 1_000_000, 4096)
        .expect("carve-out serve");
    assert!(exec.bless_output(&w.db, &report.image).expect("bless"));
    assert!(
        w.store
            .get_obao(StoreNs::Data, &report.image)
            .expect("get_obao")
            .is_some(),
        "sidecar cached"
    );
    // Second blessing is a no-op.
    assert!(!exec.bless_output(&w.db, &report.image).expect("re-bless"));

    let after = exec
        .serve_range(&w.db, &report.image, 1_000_000, 4096)
        .expect("verified serve");
    assert_eq!(before, after);
    assert_eq!(after, &full[1_000_000..1_004_096]);
}

/// The D27 pin guard: while `image/<name>` stands, the image recipe's
/// inputs (content AND skeleton) refuse eviction as PinnedByView; an
/// unpinned blob still gets the ordinary answer.
#[test]
fn pinned_image_inputs_refuse_eviction() {
    use datboi_exec::evict::{Blocked, EvictOutcome};

    let mut w = world();
    let (report, files) = minted(&mut w);
    let loose = put_content(&mut w, &content(200, 100));
    let exec = Executor::new(&w.store, ExecConfig::default()).expect("executor");

    // A content input of the pinned image.
    let leaf = Blake3::compute(&content(2, usize::try_from(files[1].1).expect("small")));
    match exec.evict(&w.db, &leaf).expect("evict") {
        EvictOutcome::Blocked(Blocked::PinnedByView) => {}
        other => panic!("expected PinnedByView, got {other:?}"),
    }
    // An unpinned resident blob: the guard doesn't overreach — the
    // ordinary no-covering-recipe answer applies.
    match exec.evict(&w.db, &loose).expect("evict") {
        EvictOutcome::Blocked(Blocked::NotGrounded) => {}
        other => panic!("expected NotGrounded, got {other:?}"),
    }
    // Drop the pin: the leaf goes back to the ordinary answer too.
    w.db.delete_tag(&format!("image/{}", "carveout"))
        .expect("untag");
    match exec.evict(&w.db, &leaf).expect("evict") {
        EvictOutcome::Blocked(Blocked::NotGrounded) => {}
        other => panic!("expected NotGrounded after untag, got {other:?}"),
    }
    let _ = report;
}

/// The view/* half of the guard: a pinned snapshot's opaque-classed
/// rows are protected; affine rows are not.
#[test]
fn pinned_view_opaque_rows_refuse_eviction() {
    use datboi_exec::evict::{Blocked, EvictOutcome};

    let mut w = world();
    let opaque = put_content(&mut w, &content(31, 5000));
    let affine = put_content(&mut w, &content(32, 5000));
    let snap = ViewSnapshot {
        created_at: 1,
        view_name: "pins".into(),
        sources: vec![],
        rows: vec![
            ViewRow {
                path: "affine.bin".into(),
                hash: affine,
                size: 5000,
                seek: 0,
            },
            ViewRow {
                path: "opaque.bin".into(),
                hash: opaque,
                size: 5000,
                seek: 2,
            },
        ],
    };
    let bytes = snap.encode().expect("encode");
    let snap_hash = Blake3::compute(&bytes);
    w.store
        .put(StoreNs::Meta, snap_hash, bytes.as_slice())
        .expect("put snap");
    w.db.set_tag("view/pins", &snap_hash, 1).expect("tag");

    let exec = Executor::new(&w.store, ExecConfig::default()).expect("executor");
    match exec.evict(&w.db, &opaque).expect("evict") {
        EvictOutcome::Blocked(Blocked::PinnedByView) => {}
        other => panic!("expected PinnedByView, got {other:?}"),
    }
    match exec.evict(&w.db, &affine).expect("evict") {
        EvictOutcome::Blocked(Blocked::NotGrounded) => {}
        other => panic!("expected NotGrounded, got {other:?}"),
    }
}
