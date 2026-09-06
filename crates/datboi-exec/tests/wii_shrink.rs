//! The Wii shrink (D116), end to end through the executor and the
//! D91/D112/D116 swap: a synthetic disc is split by the sweep into disc
//! pieces + a verified partition whose plaintext is split into file
//! pieces with the junk regenerated; the swap walks the route graph
//! down to its GROUNDING LEAVES — never the encrypted body, never the
//! plaintext — packs those, licenses the plaintext's assemble and the
//! body's `encrypt` without materializing either, licenses the disc's
//! rebuild, and evicts the disc; the disc then streams back bit-exact
//! through the component AND serves verified ranges through the
//! assemble whose body child is a seekable wasm node encrypting one
//! hash group per window.

use std::io::Read;

use datboi_core::hash::Blake3;
use datboi_exec::{ExecConfig, Executor};
use datboi_index::{Db, Residency};
use datboi_ingest::analyzers::WiiAnalyzer;
use datboi_ingest::gcm::synth::Pad;
use datboi_ingest::refine::run_sweep;
use datboi_ingest::wii::synth::{self, test_common_key};
use datboi_ingest::wii::{ISSUER_RETAIL, KnownKey};
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

fn test_analyzer() -> WiiAnalyzer {
    WiiAnalyzer::with_keys(vec![KnownKey {
        issuer: ISSUER_RETAIL,
        index: 0,
        hash: Blake3::compute(&test_common_key()),
    }])
}

fn residency(db: &Db, hash: &Blake3) -> Residency {
    db.blob_by_hash(hash).expect("q").expect("row").residency
}

#[test]
fn wii_sweep_swaps_leaves_only_and_serves_ranges_through_encrypt() {
    let (_dir, store, mut db) = world();
    let files = vec![
        ("/opening.bnr", pattern(6496, 1)),
        ("/data/big.arc", pattern(2_400_000, 2)),
        ("/data/small.dat", pattern(700, 3)),
        ("/last.bin", pattern(40_001, 5)),
    ];
    let disc = synth::disc(*b"RTSTD1", 0, &files, Pad::Junk);
    let img = disc.bytes.clone();
    let key_hash = ingest(&store, &db, &disc.common_key);
    let img_hash = ingest(&store, &db, &img);
    let body_hash = Blake3::compute(&disc.body);
    let plain_hash = Blake3::compute(&disc.plain);

    let sweep = sweep_all(&mut db, &store, &mut test_analyzer(), 1000);
    assert_eq!(sweep.errors.len(), 0, "{:?}", sweep.errors);
    assert_eq!(sweep.positive, 1, "disc split");
    assert_eq!(residency(&db, &body_hash), Residency::Absent);
    assert_eq!(residency(&db, &plain_hash), Residency::Absent);

    // Lower the floor for the small fixture: the point here is the
    // mechanism, the economics are D112's.
    db.config_set("swap:reclaim-min-bytes", b"65536")
        .expect("policy");
    let exec = Executor::new(&store, ExecConfig::default()).expect("executor");
    let report = exec.swap_covered(&mut db).expect("swap phase");
    assert_eq!(report.swapped, 1, "{report:?}");
    assert!(!store.has(StoreNs::Data, &img_hash), "disc literal gone");
    // The intermediates were proven, never materialized (D116).
    assert!(!store.has(StoreNs::Data, &body_hash), "body never stored");
    assert!(
        !store.has(StoreNs::Data, &plain_hash),
        "plaintext never stored"
    );
    assert_eq!(residency(&db, &body_hash), Residency::Absent);
    assert_eq!(residency(&db, &plain_hash), Residency::Absent);
    // What was packed is the leaves: the files and the disc's small
    // structures — less than the body alone would have cost (the
    // fixture's files fill its partition; a real disc's junk and hash
    // blocks make the gap disc-sized).
    let file_bytes: u64 = files.iter().map(|(_, b)| b.len() as u64).sum();
    assert!(
        report.bytes_packed < disc.body.len() as u64,
        "only leaves packed: {report:?}"
    );
    assert!(
        report.bytes_packed >= file_bytes,
        "the files are leaves: {report:?}"
    );
    for (_, bytes) in &files {
        let h = Blake3::compute(bytes);
        assert_eq!(residency(&db, &h), Residency::Resident, "file packed");
        assert!(store.is_packed(&h));
    }
    // The key stays where it was: a resident literal the recipes name.
    assert!(store.has_loose(StoreNs::Data, &key_hash));
    // Every route on the path is licensed (ReplayedLocal), so the
    // eviction's grounding held without the disc.
    for hash in [&plain_hash, &body_hash, &img_hash] {
        let id = db.get_blob_id(hash).unwrap().unwrap();
        let states: Vec<_> = db
            .recipes_for_output(id)
            .unwrap()
            .iter()
            .map(|r| (r.op_name.clone(), r.verify))
            .collect();
        assert!(
            states
                .iter()
                .any(|(_, v)| *v == datboi_index::VerifyState::ReplayedLocal),
            "{hash}: {states:?}"
        );
    }

    // The disc streams back bit-exact — through the plaintext assemble,
    // the encrypt component, and the disc assemble.
    let mut out = Vec::new();
    exec.open_stream(&db, &img_hash)
        .expect("route")
        .read_to_end(&mut out)
        .expect("stream");
    assert_eq!(out, img);
    assert!(
        !store.has(StoreNs::Data, &body_hash),
        "a stream spills nothing durable"
    );

    // Verified ranges: the header, across the ticket, inside the body
    // (one hash group re-encrypted in place), across the body's end
    // into the trailing junk, and the EOF clamp.
    let len = img.len() as u64;
    let body_off = disc.body_offset;
    for (offset, wlen) in [
        (0u64, 0x500u64),
        (disc.partition_offset - 16, 0x300),
        (body_off + 0x8000 * 3 + 0x3D0, 0x9000),
        (body_off + disc.body.len() as u64 - 4096, 3 * 8192),
        (len - 100_000, 200_000),
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

    // Idempotence: a second phase finds nothing to do.
    let again = exec.swap_covered(&mut db).expect("swap again");
    assert_eq!((again.swapped, again.packs), (0, 0), "{again:?}");
}

/// Real-disc proof, opt-in: `DATBOI_WII_IMAGE=/path/to.iso
/// DATBOI_WII_KEY=/path/to/common-key.bin DATBOI_WII_WORKDIR=/big/disk
/// cargo test --release -p datboi-exec --test wii_shrink
/// real_image_swaps_and_serves -- --ignored --nocapture`. Ingests the
/// image and the key into a fresh store, sweeps, swaps (pack the
/// leaves + license through the components + evict), then streams the
/// whole image back and checks its identity, plus a few verified
/// ranges. Prints the verdict and the swap report.
#[test]
#[ignore]
fn real_image_swaps_and_serves() {
    let (Ok(path), Ok(key_path)) = (
        std::env::var("DATBOI_WII_IMAGE"),
        std::env::var("DATBOI_WII_KEY"),
    ) else {
        eprintln!("DATBOI_WII_IMAGE / DATBOI_WII_KEY unset");
        return;
    };
    let workdir = std::env::var("DATBOI_WII_WORKDIR")
        .unwrap_or_else(|_| std::env::temp_dir().display().to_string());
    let dir = tempfile::tempdir_in(&workdir).expect("workdir");
    let store = Store::open(dir.path().join("store")).expect("store");
    let mut db = Db::open(dir.path()).expect("db");
    if let Ok(cap) = std::env::var("DATBOI_WII_MAX_PIECES") {
        db.config_set("wii:max-pieces", cap.as_bytes())
            .expect("policy");
    }

    let key = std::fs::read(&key_path).expect("key file");
    ingest(&store, &db, &key);
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
    let sweep = sweep_all(&mut db, &store, &mut WiiAnalyzer::new(), 10);
    assert_eq!(sweep.errors.len(), 0, "{:?}", sweep.errors);
    let details: Vec<String> = db
        .cache()
        .prepare("SELECT COALESCE(detail,'') FROM analysis")
        .expect("q")
        .query_map([], |r| r.get(0))
        .expect("q")
        .collect::<Result<_, _>>()
        .expect("q");
    for d in &details {
        eprintln!("verdict ({:.1?}): {d}", t1.elapsed());
    }
    assert_eq!(sweep.positive, 1, "image split");

    let t2 = std::time::Instant::now();
    let exec = Executor::new(&store, ExecConfig::default()).expect("executor");
    let report = exec.swap_covered(&mut db).expect("swap phase");
    eprintln!("swap ({:.1?}): {report:?}", t2.elapsed());
    assert_eq!(report.swapped, 1, "{report:?}");
    assert!(!store.has(StoreNs::Data, &img_hash), "literal gone");
    let resident: i64 = db
        .cache()
        .query_row(
            "SELECT COALESCE(SUM(size),0) FROM blob WHERE namespace = 0 AND residency = 0",
            [],
            |r| r.get(0),
        )
        .expect("sum");
    eprintln!(
        "resident after swap: {resident} B = {:.1}% of the {len} B image",
        resident as f64 * 100.0 / len as f64
    );

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
        (0u64, 0x50000u64),
        (0xF800000, 100_000),
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
