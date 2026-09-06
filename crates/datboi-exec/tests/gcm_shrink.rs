//! The GameCube shrink (D115), end to end through the executor and the
//! D91/D112 swap: a junk-padded image is split by the sweep into file
//! pieces + a zero-input junk recipe; the swap fires on the regenerated
//! bytes, packs the pieces and NOT the junk, licenses the rebuild
//! (running the xf-gc-junk component under wasmtime), and evicts the
//! image; the image then streams back bit-exact AND serves verified
//! ranges through the assemble whose junk child is a seekable wasm node.

use std::io::Read;

use datboi_core::hash::Blake3;
use datboi_exec::{ExecConfig, Executor};
use datboi_index::{Db, Residency};
use datboi_ingest::analyzers::GcmAnalyzer;
use datboi_ingest::gcm::synth::{self, Pad};
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

/// The junk input of the rebuild: the one whose route has no inputs.
fn junk_of(db: &Db, img_hash: &Blake3) -> Option<(Blake3, u64)> {
    let img_id = db.get_blob_id(img_hash).expect("q").expect("row");
    let rebuild = db
        .recipes_for_output(img_id)
        .expect("recipes")
        .into_iter()
        .next()
        .expect("rebuild route");
    let inputs = db.rebuild_inputs(rebuild.recipe_id).expect("inputs");
    let generated: Vec<_> = inputs.iter().filter(|i| i.generated).collect();
    assert!(generated.len() <= 1, "at most one generated input");
    let f = generated.first()?;
    assert_eq!(f.residency, Residency::Absent);
    Some((f.hash, f.size.expect("size")))
}

#[test]
fn gcm_sweep_swaps_evicts_and_serves_ranges_through_the_junk() {
    let (_dir, store, mut db) = world();
    let files = vec![
        ("/opening.bnr", pattern(6496, 1)),
        ("/data/big.arc", pattern(700_000, 2)),
        ("/data/small.dat", pattern(700, 3)),
        ("/last.bin", pattern(40_001, 5)),
    ];
    // Big enough that the junk clears the 4 MiB reclaim floor: the
    // builder pads to a few MiB, so pad a large gap with a big sparse
    // file layout — simplest is a bigger image via a large file whose
    // tail is followed by junk; the builder's trailing junk after the
    // zero pad is what we count.
    let image = synth::image(*b"GALE01", 0, &files, Pad::Junk);
    let img = image.bytes;
    let img_hash = ingest(&store, &db, &img);

    let sweep = sweep_all(&mut db, &store, &mut GcmAnalyzer::new(), 1000);
    assert_eq!(sweep.errors.len(), 0, "{:?}", sweep.errors);
    assert_eq!(sweep.positive, 1, "image split");
    let (junk_hash, junk_len) = junk_of(&db, &img_hash).expect("junk claimed");
    assert_eq!(junk_len, img.len() as u64);

    // Lower the floor for the small fixture: the point here is the
    // mechanism, the economics are D112's.
    db.config_set("swap:reclaim-min-bytes", b"65536")
        .expect("policy");
    let exec = Executor::new(&store, ExecConfig::default()).expect("executor");
    let report = exec.swap_covered(&mut db).expect("swap phase");
    assert_eq!(report.swapped, 1, "{report:?}");
    assert!(!store.has(StoreNs::Data, &img_hash), "literal gone");
    assert!(!store.has(StoreNs::Data, &junk_hash), "junk never stored");
    assert!(
        report.bytes_packed < img.len() as u64 - 300_000,
        "junk was not packed: {report:?}"
    );

    let mut out = Vec::new();
    exec.open_stream(&db, &img_hash)
        .expect("route")
        .read_to_end(&mut out)
        .expect("stream");
    assert_eq!(out, img);

    let len = img.len() as u64;
    for (offset, wlen) in [
        (0u64, 0x2440u64),        // boot + bi2 (literal + piece)
        (0x2440 - 5, 100),        // into the apploader
        (len / 2, 1 << 20),       // through junk and files
        (len - 100_000, 200_000), // trailing junk, EOF clamp
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

/// Real-disc proof, opt-in: `DATBOI_GCM_IMAGE=/path/to.iso
/// DATBOI_GCM_WORKDIR=/big/disk cargo test --release -p datboi-exec
/// --test gcm_shrink real_image_swaps_and_serves -- --ignored
/// --nocapture`. Ingests the image into a fresh store, sweeps, swaps
/// (pack + license through the component + evict), then streams the
/// whole image back and checks its identity, plus a few verified
/// ranges. Prints the verdict and the swap report.
#[test]
#[ignore]
fn real_image_swaps_and_serves() {
    let Ok(path) = std::env::var("DATBOI_GCM_IMAGE") else {
        eprintln!("DATBOI_GCM_IMAGE unset");
        return;
    };
    let workdir = std::env::var("DATBOI_GCM_WORKDIR")
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
    let sweep = sweep_all(&mut db, &store, &mut GcmAnalyzer::new(), 10);
    assert_eq!(sweep.errors.len(), 0, "{:?}", sweep.errors);
    let detail: String = db
        .cache()
        .query_row("SELECT COALESCE(detail,'') FROM analysis", [], |r| r.get(0))
        .expect("detail");
    eprintln!("verdict ({:.1?}): {detail}", t1.elapsed());
    assert_eq!(sweep.positive, 1, "image split");
    let junk = junk_of(&db, &img_hash);
    match junk {
        Some((h, l)) => eprintln!("junk {h}: {l} B address space"),
        None => eprintln!("no junk matched (zero-padded or scrubbed master)"),
    }

    let t2 = std::time::Instant::now();
    let exec = Executor::new(&store, ExecConfig::default()).expect("executor");
    let report = exec.swap_covered(&mut db).expect("swap phase");
    eprintln!("swap ({:.1?}): {report:?}", t2.elapsed());
    assert_eq!(report.swapped, 1, "{report:?}");
    assert!(!store.has(StoreNs::Data, &img_hash), "literal gone");
    if let Some((h, _)) = junk {
        assert!(!store.has(StoreNs::Data, &h), "junk never stored");
    }

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
        (0u64, 0x2440u64),
        (0x2440 - 5, 100_000),
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

/// Two real discs in one store, opt-in: `DATBOI_GCM_IMAGE` and
/// `DATBOI_GCM_IMAGE_B` (plus `DATBOI_GCM_WORKDIR`). Ingests both,
/// sweeps, runs the swap phase twice (the second disc's sharing
/// evidence exists only after the first swapped), and prints resident
/// bytes against the raw pair — the measured cross-variant dedupe.
#[test]
#[ignore]
fn real_pair_dedupe() {
    let (Ok(a), Ok(b)) = (
        std::env::var("DATBOI_GCM_IMAGE"),
        std::env::var("DATBOI_GCM_IMAGE_B"),
    ) else {
        eprintln!("DATBOI_GCM_IMAGE / DATBOI_GCM_IMAGE_B unset");
        return;
    };
    let workdir = std::env::var("DATBOI_GCM_WORKDIR")
        .unwrap_or_else(|_| std::env::temp_dir().display().to_string());
    let dir = tempfile::tempdir_in(&workdir).expect("workdir");
    let store = Store::open(dir.path().join("store")).expect("store");
    let mut db = Db::open(dir.path()).expect("db");

    let resident = |db: &Db| -> u64 {
        db.cache()
            .query_row(
                "SELECT COALESCE(SUM(size),0) FROM blob WHERE namespace = 0 AND residency = 0",
                [],
                |r| r.get::<_, i64>(0),
            )
            .expect("sum") as u64
    };

    let mut raw = 0u64;
    let mut hashes = Vec::new();
    for path in [&a, &b] {
        let file = std::fs::File::open(path).expect("open image");
        let len = file.metadata().expect("meta").len();
        let (hash, _, _) = store.put_new(StoreNs::Data, file).expect("ingest");
        db.upsert_blob(
            &hash,
            Some(len),
            datboi_index::Namespace::Data,
            Residency::Resident,
        )
        .expect("row");
        raw += len;
        hashes.push(hash);
        eprintln!("ingested {path} ({len} B) as {hash}");
    }
    let sweep = sweep_all(&mut db, &store, &mut GcmAnalyzer::new(), 10);
    assert_eq!(sweep.errors.len(), 0, "{:?}", sweep.errors);
    assert_eq!(sweep.positive, 2, "both split");
    let details: Vec<String> = db
        .cache()
        .prepare("SELECT COALESCE(detail,'') FROM analysis")
        .expect("q")
        .query_map([], |r| r.get(0))
        .expect("q")
        .collect::<Result<_, _>>()
        .expect("q");
    for d in &details {
        eprintln!("verdict: {d}");
    }
    eprintln!(
        "resident after sweep: {} B (raw pair {} B)",
        resident(&db),
        raw
    );

    let exec = Executor::new(&store, ExecConfig::default()).expect("executor");
    for pass in 1..=2 {
        let report = exec.swap_covered(&mut db).expect("swap phase");
        eprintln!("swap pass {pass}: {report:?}");
        eprintln!(
            "resident after pass {pass}: {} B = {:.1}% of the raw pair",
            resident(&db),
            resident(&db) as f64 * 100.0 / raw as f64
        );
    }
    for hash in &hashes {
        let row = db.blob_by_hash(hash).expect("q").expect("row");
        let inputs = db
            .rebuild_inputs(db.recipes_for_output(row.blob_id).expect("r")[0].recipe_id)
            .expect("inputs");
        let total: u64 = inputs
            .iter()
            .filter(|i| !i.generated)
            .filter_map(|i| i.size)
            .sum();
        let shared: u64 = inputs
            .iter()
            .filter(|i| {
                !i.generated && (i.covering_claims >= 2 || i.residency == Residency::Resident)
            })
            .filter_map(|i| i.size)
            .sum();
        let generated: u64 = inputs
            .iter()
            .filter(|i| i.generated)
            .filter_map(|i| i.size)
            .sum();
        eprintln!(
            "{hash}: residency {:?}; inputs {} B packable, {} B shared/resident ({:.1}%), {} B generated",
            row.residency,
            total,
            shared,
            shared as f64 * 100.0 / total.max(1) as f64,
            generated
        );
    }
}
