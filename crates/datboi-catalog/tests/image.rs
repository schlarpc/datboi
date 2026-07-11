//! Image mint tests (D62): determinism across fresh stores, golden
//! recipe hash, residency refusal, layout refusals surfacing, tag flip,
//! idempotent re-mint, ViewDef image-param round-trip (CBOR keys 8–11).

use datboi_catalog::{
    CatalogError, ImageParams, ViewDef, define_view, get_view, mint_image, missing_inputs,
};
use datboi_core::hash::Blake3;
use datboi_core::viewsnap::{ViewRow, ViewSnapshot};
use datboi_index::{Db, Namespace as IndexNs, Residency};
use datboi_store_fs::{Namespace as StoreNs, Store};
use tempfile::TempDir;

struct Fixture {
    _dir: TempDir,
    store: Store,
    db: Db,
}

fn fixture() -> Fixture {
    let dir = TempDir::new().expect("tempdir");
    let store = Store::open(dir.path().join("store")).expect("store");
    std::fs::create_dir_all(dir.path().join("db")).expect("db dir");
    let db = Db::open(&dir.path().join("db")).expect("db");
    Fixture {
        _dir: dir,
        store,
        db,
    }
}

/// Deterministic pseudo-content: same seed, same bytes, distinct blobs.
fn content(seed: u8, len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| (usize::from(seed) * 31 + i * 7) as u8)
        .collect()
}

/// Ingest one resident, verified content blob.
fn put_content(fx: &mut Fixture, bytes: &[u8]) -> Blake3 {
    let hash = Blake3::compute(bytes);
    fx.store.put(StoreNs::Data, hash, bytes).expect("put");
    let id = fx
        .db
        .upsert_blob(
            &hash,
            Some(bytes.len() as u64),
            IndexNs::Data,
            Residency::Resident,
        )
        .expect("upsert");
    fx.db.set_verified(id, 1).expect("verified");
    hash
}

/// A small fixed snapshot: nested dirs, zero-size file, shared blob at
/// two paths (one input, two windows).
fn sample_snapshot(fx: &mut Fixture) -> (Blake3, ViewSnapshot) {
    let a = put_content(fx, &content(1, 700));
    let b = put_content(fx, &content(2, 1500));
    let zero = put_content(fx, b"");
    let rows = vec![
        ViewRow {
            path: "Alpha (USA).gba".into(),
            hash: a,
            size: 700,
            seek: 0,
        },
        ViewRow {
            path: "empty.sav".into(),
            hash: zero,
            size: 0,
            seek: 0,
        },
        ViewRow {
            path: "sub/Beta (Europe).gba".into(),
            hash: b,
            size: 1500,
            seek: 0,
        },
        ViewRow {
            path: "sub/copy of alpha.gba".into(),
            hash: a,
            size: 700,
            seek: 0,
        },
    ];
    let snap = ViewSnapshot {
        created_at: 1_780_000_000,
        view_name: "gba-test".into(),
        sources: vec![],
        rows,
    };
    // A fixed stand-in for the snapshot object hash (the mint only
    // reads its bytes for serial/signature derivation).
    (Blake3::compute(b"pinned snapshot identity"), snap)
}

fn params_512() -> ImageParams {
    ImageParams {
        cluster_size: 512,
        partition: true,
        label: None,
    }
}

#[test]
fn mint_is_deterministic_across_fresh_stores() {
    let mut reports = vec![];
    for _ in 0..2 {
        let mut fx = fixture();
        let (snap_hash, snap) = sample_snapshot(&mut fx);
        let r = mint_image(
            &mut fx.db,
            &fx.store,
            "gba-test",
            &snap_hash,
            &snap,
            &params_512(),
            true,
            42,
        )
        .expect("mint");
        reports.push(r);
    }
    assert_eq!(reports[0].image, reports[1].image);
    assert_eq!(reports[0].recipe, reports[1].recipe);
    assert_eq!(reports[0].size, reports[1].size);
}

/// Format commitment: recipe + image hashes for the pinned snapshot.
/// A change here means minted identity changed — a format event.
#[test]
fn golden_mint() {
    let mut fx = fixture();
    let (snap_hash, snap) = sample_snapshot(&mut fx);
    let r = mint_image(
        &mut fx.db,
        &fx.store,
        "gba-test",
        &snap_hash,
        &snap,
        &params_512(),
        true,
        42,
    )
    .expect("mint");
    assert_eq!(r.size, 35_138_048);
    assert_eq!(r.rows, 4);
    assert_eq!(
        r.image.to_hex(),
        "7d0b40f4f058a6c22bbc939538b294ca9354e430cefea17bcdec41df8c08a12e"
    );
    assert_eq!(
        r.recipe.to_hex(),
        "5ae445ee915286e628c98604a19f1c6bf92ab42dfe386730e3e1453880ee765c"
    );
    // Blessed at mint: the output sidecar exists (full D49 from birth).
    assert!(r.obao_stored);
    assert!(
        fx.store
            .get_obao(StoreNs::Data, &r.image)
            .expect("get_obao")
            .is_some()
    );
    // The tag flip happened.
    assert_eq!(fx.db.get_tag("image/gba-test").expect("tag"), Some(r.image));
}

#[test]
fn remint_is_idempotent_and_no_obao_skips_sidecar() {
    let mut fx = fixture();
    let (snap_hash, snap) = sample_snapshot(&mut fx);
    let first = mint_image(
        &mut fx.db,
        &fx.store,
        "gba-test",
        &snap_hash,
        &snap,
        &params_512(),
        false,
        42,
    )
    .expect("mint");
    assert!(!first.obao_stored);
    assert!(
        fx.store
            .get_obao(StoreNs::Data, &first.image)
            .expect("get_obao")
            .is_none()
    );
    let second = mint_image(
        &mut fx.db,
        &fx.store,
        "gba-test",
        &snap_hash,
        &snap,
        &params_512(),
        false,
        43,
    )
    .expect("re-mint");
    assert_eq!(first.image, second.image);
    assert_eq!(first.recipe, second.recipe);
}

#[test]
fn missing_residency_is_listed_and_refused() {
    let mut fx = fixture();
    let (snap_hash, mut snap) = sample_snapshot(&mut fx);
    // A claimed-but-absent blob: indexed, no literal.
    let ghost = Blake3::compute(b"never stored");
    fx.db
        .upsert_blob(&ghost, Some(64), IndexNs::Data, Residency::Absent)
        .expect("upsert");
    snap.rows.push(ViewRow {
        path: "zz-ghost.gba".into(),
        hash: ghost,
        size: 64,
        seek: 0,
    });
    let missing = missing_inputs(&fx.db, &snap).expect("query");
    assert_eq!(missing, vec![ghost]);
    let err = mint_image(
        &mut fx.db,
        &fx.store,
        "gba-test",
        &snap_hash,
        &snap,
        &params_512(),
        true,
        42,
    )
    .expect_err("must refuse");
    assert!(matches!(err, CatalogError::Image(_)), "got {err}");
}

#[test]
fn oversize_file_refused_by_layout() {
    let mut fx = fixture();
    let (snap_hash, mut snap) = sample_snapshot(&mut fx);
    snap.rows.push(ViewRow {
        path: "zz-huge.iso".into(),
        hash: Blake3::compute(b"huge"),
        size: 4 << 30,
        seek: 0,
    });
    let err = mint_image(
        &mut fx.db,
        &fx.store,
        "gba-test",
        &snap_hash,
        &snap,
        &params_512(),
        true,
        42,
    )
    .expect_err("must refuse");
    assert!(matches!(err, CatalogError::Fat32(_)), "got {err}");
}

/// ViewDef image params round-trip through CBOR keys 8–11; definitions
/// without them (the pre-image v2 shape) decode as image = None.
#[test]
fn view_def_image_params_round_trip() {
    let fx = fixture();
    let base = ViewDef {
        name: "plain".into(),
        provider: "no-intro".into(),
        system: "gba".into(),
        template: "{name}".into(),
        selection: None,
        profile: None,
        image: None,
        mame: None,
    };
    define_view(&fx.db, &base).expect("define");
    assert_eq!(
        get_view(&fx.db, "plain").expect("get").expect("defined"),
        base
    );

    let imaged = ViewDef {
        name: "carded".into(),
        image: Some(ImageParams {
            cluster_size: 512,
            partition: false,
            label: Some("MYCARD".into()),
        }),
        ..base.clone()
    };
    define_view(&fx.db, &imaged).expect("define");
    let got = get_view(&fx.db, "carded").expect("get").expect("defined");
    assert_eq!(got.image, imaged.image);

    // Defaults (no explicit label) survive too.
    let defaulted = ViewDef {
        name: "defaults".into(),
        image: Some(ImageParams::default()),
        ..base
    };
    define_view(&fx.db, &defaulted).expect("define");
    let got = get_view(&fx.db, "defaults").expect("get").expect("defined");
    assert_eq!(got.image, Some(ImageParams::default()));
}
