//! The ISO9660 decomposition (D114), end to end through the executor
//! and the D91/D112 swap: a cooked image is split by the sweep into
//! file pieces, a uniform pad file and the slack become fills, the swap
//! fires on the reclaimed fill bytes alone (a lone disc, nothing
//! shared), packs the pieces, licenses the rebuild and evicts the
//! image; the image then streams back bit-exact and serves verified
//! ranges through the assemble.

use std::io::Read;

use datboi_core::hash::Blake3;
use datboi_exec::{ExecConfig, Executor};
use datboi_index::{Db, Residency};
use datboi_ingest::analyzers::Iso9660Analyzer;
use datboi_ingest::iso9660::synth::{self, Spec};
use datboi_ingest::refine::run_sweep;
use datboi_store_fs::{Namespace as StoreNs, Store};

fn sweep_all(
    db: &mut Db,
    store: &Store,
    analyzer: &mut dyn datboi_ingest::refine::Analyzer,
    limit: usize,
) -> datboi_ingest::refine::SweepReport {
    let exec = Executor::new(store, ExecConfig::default()).expect("executor");
    let bytes = datboi_ingest::refine::Logical::new(store, &exec);
    run_sweep(db, store, &bytes, analyzer, limit).expect("sweep")
}

fn world() -> (tempfile::TempDir, Store, Db) {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Store::open(dir.path().join("store")).expect("store");
    let db = Db::open(dir.path()).expect("db");
    (dir, store, db)
}

fn ingest(store: &Store, db: &Db, bytes: &[u8]) -> Blake3 {
    let hash = Blake3::compute(bytes);
    store.put(StoreNs::Data, hash, bytes).expect("put");
    db.upsert_blob(
        &hash,
        Some(bytes.len() as u64),
        datboi_index::Namespace::Data,
        Residency::Resident,
    )
    .expect("row");
    hash
}

fn pattern(len: usize, salt: u32) -> Vec<u8> {
    (0..len)
        .map(|i| ((i as u32).wrapping_mul(2_654_435_761) ^ salt).to_le_bytes()[i % 4])
        .collect()
}

#[test]
fn iso9660_sweep_swaps_evicts_and_serves_ranges() {
    let (_dir, store, mut db) = world();
    let elf = pattern(600_000, 11);
    let data = pattern(1_500_000, 12);
    let dummy = vec![0u8; 6 << 20];
    let files: Vec<(&str, &[u8])> = vec![
        ("/SLUS_000.00", &elf),
        ("/DATA/PACK.BIN", &data),
        ("/DUMMY.DAT", &dummy),
    ];
    let img = synth::tree(
        &files,
        &Spec {
            tail_sectors: 64,
            ..Spec::default()
        },
    );
    let img_hash = ingest(&store, &db, &img);

    let sweep = sweep_all(&mut db, &store, &mut Iso9660Analyzer, 1000);
    assert_eq!(sweep.errors.len(), 0, "{:?}", sweep.errors);
    assert_eq!(sweep.positive, 1, "image split");

    // The swap: the pad file and the tail are reclaim above the floor,
    // the two data files pack, the image evicts.
    let exec = Executor::new(&store, ExecConfig::default()).expect("executor");
    let report = exec.swap_covered(&mut db).expect("swap phase");
    assert_eq!(report.swapped, 1, "{report:?}");
    assert!(report.bytes_packed < (3 << 20), "{report:?}");
    assert!(!store.has(StoreNs::Data, &img_hash), "literal gone");
    assert_eq!(
        db.blob_by_hash(&img_hash)
            .expect("q")
            .expect("row")
            .residency,
        Residency::EvictedCovered
    );
    for bytes in [&elf, &data] {
        let hash = Blake3::compute(bytes);
        assert!(store.is_packed(&hash), "piece packed");
    }
    assert!(
        db.blob_by_hash(&Blake3::compute(&dummy))
            .expect("q")
            .is_none(),
        "the pad file was never a claim"
    );

    // Streams back bit-exact through the route.
    let mut out = Vec::new();
    exec.open_stream(&db, &img_hash)
        .expect("route")
        .read_to_end(&mut out)
        .expect("stream");
    assert_eq!(out, img);

    // Verified ranges across every region kind: system area (fill),
    // descriptors (residue), a file, the pad file, the tail.
    let len = img.len() as u64;
    for (offset, wlen) in [
        (0u64, 40_000u64),
        (16 * 2048 - 10, 5_000),
        (len / 3, 1 << 20),
        (len - 200_000, 300_000),
    ] {
        let got = exec
            .serve_range(&db, &img_hash, offset, wlen)
            .expect("range");
        let end = offset.saturating_add(wlen).min(len);
        assert_eq!(
            got,
            &img[usize::try_from(offset).unwrap()..usize::try_from(end).unwrap()],
            "window {offset}+{wlen}"
        );
    }
}

/// Real-image proof, opt-in: `DATBOI_ISO9660_IMAGE=/path/to.iso
/// DATBOI_ISO9660_WORKDIR=/big/disk cargo test --release -p datboi-exec
/// --test iso9660_shrink real_image_swaps_and_serves -- --ignored
/// --nocapture`. Ingests the image into a fresh store, sweeps, runs the
/// swap (which fires only when the image's fills clear the D112 floor —
/// a lone disc with no pad file stays literal, and the rebuild is then
/// licensed by replay instead), streams the image back through its
/// route and checks its identity plus a few verified ranges. Prints the
/// verdict and the swap report.
#[test]
#[ignore]
fn real_image_swaps_and_serves() {
    let Ok(path) = std::env::var("DATBOI_ISO9660_IMAGE") else {
        eprintln!("DATBOI_ISO9660_IMAGE unset");
        return;
    };
    let workdir = std::env::var("DATBOI_ISO9660_WORKDIR")
        .unwrap_or_else(|_| std::env::temp_dir().display().to_string());
    let dir = tempfile::tempdir_in(&workdir).expect("workdir");
    let store = Store::open(dir.path().join("store")).expect("store");
    let mut db = Db::open(dir.path()).expect("db");

    let t0 = std::time::Instant::now();
    let file = std::fs::File::open(&path).expect("open image");
    let len = file.metadata().expect("meta").len();
    let (img_hash, _, _) = store.put_new(StoreNs::Data, file).expect("ingest");
    db.upsert_blob(
        &img_hash,
        Some(len),
        datboi_index::Namespace::Data,
        Residency::Resident,
    )
    .expect("row");
    eprintln!("ingested {len} B as {img_hash} in {:.1?}", t0.elapsed());

    let t1 = std::time::Instant::now();
    let sweep = sweep_all(&mut db, &store, &mut Iso9660Analyzer, 10);
    assert_eq!(sweep.errors.len(), 0, "{:?}", sweep.errors);
    let detail: String = db
        .cache()
        .query_row("SELECT COALESCE(detail,'') FROM analysis", [], |r| r.get(0))
        .expect("detail");
    eprintln!("verdict ({:.1?}): {detail}", t1.elapsed());
    assert_eq!(sweep.positive, 1, "image split");

    let exec = Executor::new(&store, ExecConfig::default()).expect("executor");
    let t2 = std::time::Instant::now();
    let report = exec.swap_covered(&mut db).expect("swap phase");
    eprintln!("swap ({:.1?}): {report:?}", t2.elapsed());
    if report.swapped == 0 {
        // Below the floor: license the rebuild by replay — the same
        // bit-exact proof the swap would have run.
        let img_id = db.get_blob_id(&img_hash).expect("q").expect("row");
        let rebuild = db
            .recipes_for_output(img_id)
            .expect("recipes")
            .into_iter()
            .next()
            .expect("rebuild route");
        let t = std::time::Instant::now();
        let replay = exec.replay(&db, rebuild.recipe_id).expect("replay");
        eprintln!("replay ({:.1?}): {replay:?}", t.elapsed());
        return;
    }
    assert!(!store.has(StoreNs::Data, &img_hash), "literal gone");

    let t3 = std::time::Instant::now();
    let mut hasher = blake3::Hasher::new();
    std::io::copy(
        &mut exec.open_stream(&db, &img_hash).expect("route"),
        &mut hasher,
    )
    .expect("stream");
    assert_eq!(
        Blake3(*hasher.finalize().as_bytes()),
        img_hash,
        "image rebuilds bit-exact"
    );
    eprintln!("full rebuild streamed + verified in {:.1?}", t3.elapsed());

    let mut original = std::fs::File::open(&path).expect("open image");
    use std::io::{Seek as _, SeekFrom};
    let t4 = std::time::Instant::now();
    for (offset, wlen) in [
        (0u64, 40_000u64),
        (16 * 2048 - 100, 10_000),
        (len / 2, 1 << 20),
        (len - 100_000, 200_000),
    ] {
        let got = exec
            .serve_range(&db, &img_hash, offset, wlen)
            .expect("range");
        let end = offset.saturating_add(wlen).min(len);
        let mut want = vec![0u8; usize::try_from(end - offset).expect("small")];
        original.seek(SeekFrom::Start(offset)).expect("seek");
        original.read_exact(&mut want).expect("read");
        assert_eq!(got, want, "window {offset}+{wlen}");
    }
    eprintln!("4 verified ranges in {:.1?}", t4.elapsed());
}
