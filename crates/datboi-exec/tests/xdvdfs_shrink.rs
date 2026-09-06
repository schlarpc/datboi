//! The XDVDFS shrink (D111), end to end through the executor and the
//! D91 swap: a seed-era XISO is split by the sweep into file pieces + a
//! zero-input filler recipe; the swap phase fires on the regeneration
//! trigger alone (a lone disc, nothing shared), packs the pieces and
//! NOT the filler, licenses the rebuild (running the xf-xgd1-prng
//! component under wasmtime), and evicts the image; the image then
//! streams back bit-exact AND serves verified ranges through the
//! assemble whose filler child is a seekable wasm node — the in-place
//! `serve-range` path `open_random` grew for exactly this.

use std::io::Read;

use datboi_core::hash::Blake3;
use datboi_exec::{ExecConfig, Executor};
use datboi_index::{Db, Residency};
use datboi_ingest::analyzers::XdvdfsAnalyzer;
use datboi_ingest::refine::run_sweep;
use datboi_ingest::xdvdfs::synth::{self, Filler};
use datboi_store_fs::{Namespace as StoreNs, Store};

const SEED: u32 = 0x4E99_8EB0;
const SECTOR: u64 = 2048;

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

/// The rebuild's filler input: the one whose route has no inputs.
fn filler_of(db: &Db, img_hash: &Blake3) -> (Blake3, u64) {
    let img_id = db.get_blob_id(img_hash).expect("q").expect("row");
    let rebuild = db
        .recipes_for_output(img_id)
        .expect("recipes")
        .into_iter()
        .next()
        .expect("rebuild route");
    let inputs = db.rebuild_inputs(rebuild.recipe_id).expect("inputs");
    let generated: Vec<_> = inputs.iter().filter(|i| i.generated).collect();
    assert_eq!(generated.len(), 1, "exactly one generated input");
    let f = generated[0];
    assert_eq!(f.residency, Residency::Absent);
    (f.hash, f.size.expect("size"))
}

#[test]
fn xdvdfs_sweep_swaps_evicts_and_serves_ranges_through_the_filler() {
    let (_dir, store, mut db) = world();
    let image = synth::image(Filler::Seed(SEED), false);
    let img = image.bytes;
    let img_hash = ingest(&store, &db, &img);

    let sweep = sweep_all(&mut db, &store, &mut XdvdfsAnalyzer::new(), 1000);
    assert_eq!(sweep.errors.len(), 0, "{:?}", sweep.errors);
    assert_eq!(sweep.positive, 1, "image split");
    let (filler_hash, filler_len) = filler_of(&db, &img_hash);
    assert_eq!(filler_len, image.stream_sectors * SECTOR);

    // The D91 swap, production path: a LONE disc shares nothing, so the
    // sharing predicate says no — the D111 regeneration trigger says yes
    // (this fixture regenerates ~97% of its bytes; a real seed-era disc
    // ~40%, above the 25% default). Pieces pack, the filler does not,
    // the rebuild licenses (running the component), the image evicts.
    let exec = Executor::new(&store, ExecConfig::default()).expect("executor");
    let report = exec.swap_covered(&mut db).expect("swap phase");
    assert_eq!(report.swapped, 1, "{report:?}");
    assert_eq!(report.packs, 1, "{report:?}");
    let piece_bytes: u64 = image.files.iter().map(|(_, b)| b.len() as u64).sum::<u64>()
        + 2 * SECTOR // two directory tables
        + 3 * SECTOR; // the residue tail
    assert_eq!(
        report.bytes_packed, piece_bytes,
        "the filler is never packed"
    );
    assert!(!store.has(StoreNs::Data, &img_hash), "literal gone");
    assert_eq!(
        db.blob_by_hash(&img_hash)
            .expect("q")
            .expect("row")
            .residency,
        Residency::EvictedCovered
    );
    assert_eq!(
        db.blob_by_hash(&filler_hash)
            .expect("q")
            .expect("row")
            .residency,
        Residency::Absent,
        "the filler stream is never stored"
    );
    assert!(!store.has(StoreNs::Data, &filler_hash));

    // Bit-exact full stream through the recipe route.
    let mut streamed = Vec::new();
    exec.open_stream(&db, &img_hash)
        .expect("route")
        .read_to_end(&mut streamed)
        .expect("read");
    assert_eq!(streamed, img, "image rebuilds bit-exact");

    // Verified ranges: windows inside stream sectors, across a
    // file/filler boundary, inside the security-range fill, across the
    // security range into regenerated filler (the stream jump), inside
    // the residue tail, and the EOF clamp. Each filler window is served
    // by the wasm node in place — a spill would regenerate the whole
    // stream per read.
    let total = img.len() as u64;
    for (offset, len) in [
        (0u64, 16u64),
        (31 * SECTOR + 2000, 200),   // stream -> volume descriptor
        (35 * SECTOR + 100, 3000),   // filler -> a.bin
        (37 * SECTOR + 1000, 3000),  // a.bin slack -> filler
        (60 * SECTOR, 64),           // inside the security range
        (4144 * SECTOR + 2040, 100), // security range -> regenerated
        (4152 * SECTOR + 10, 100),   // residue tail
        (total - 64, 200),           // EOF clamp
    ] {
        let got = exec
            .serve_range(&db, &img_hash, offset, len)
            .expect("range");
        let start = usize::try_from(offset.min(total)).expect("small");
        let end = usize::try_from(offset.saturating_add(len).min(total)).expect("small");
        assert_eq!(got, &img[start..end], "window {offset}+{len}");
    }
    assert!(
        !db.is_seek_quarantined(&XdvdfsAnalyzer::component_hash())
            .expect("q"),
        "honest component stays trusted"
    );

    // The filler blob itself streams through its zero-input route. A
    // direct RANGE of it is refused (D49: recipe-served ranges verify
    // against the output's outboard, and the filler was never
    // materialized to earn one) — the disc's ranges above are the real
    // consumer, verified against the disc's own tree.
    assert!(
        matches!(
            exec.serve_range(&db, &filler_hash, 4135 * SECTOR - 8, 16),
            Err(datboi_exec::ExecError::MissingOutboard(_))
        ),
        "an unmaterialized generated blob has no outboard to verify ranges against"
    );
    let mut all = Vec::new();
    exec.open_stream(&db, &filler_hash)
        .expect("route")
        .read_to_end(&mut all)
        .expect("read");
    assert_eq!(all.len() as u64, filler_len);
    assert_eq!(Blake3::compute(&all), filler_hash);

    // A second pass finds nothing to do.
    let again = exec.swap_covered(&mut db).expect("swap phase");
    assert_eq!(again.swapped, 0, "{again:?}");
}

/// The regeneration trigger is a threshold, not a switch: an rc4-era
/// disc (nothing generated, nothing shared) stays below both predicates
/// and keeps its literal.
#[test]
fn rc4_era_lone_disc_does_not_swap() {
    let (_dir, store, mut db) = world();
    let image = synth::image(Filler::Random, false);
    let img_hash = ingest(&store, &db, &image.bytes);
    let sweep = sweep_all(&mut db, &store, &mut XdvdfsAnalyzer::new(), 1000);
    assert_eq!(sweep.positive, 1);
    let exec = Executor::new(&store, ExecConfig::default()).expect("executor");
    let report = exec.swap_covered(&mut db).expect("swap phase");
    assert_eq!(report.swapped, 0, "{report:?}");
    assert_eq!(report.below_threshold, 1, "{report:?}");
    assert!(store.has(StoreNs::Data, &img_hash));
}

/// The knob is molten: raising `swap:generated-min-pct` above the
/// fixture's regeneration fraction holds the swap back.
#[test]
fn generated_threshold_is_policy() {
    let (_dir, store, mut db) = world();
    let image = synth::image(Filler::Seed(SEED), false);
    let img_hash = ingest(&store, &db, &image.bytes);
    let sweep = sweep_all(&mut db, &store, &mut XdvdfsAnalyzer::new(), 1000);
    assert_eq!(sweep.positive, 1);
    db.config_set("swap:generated-min-pct", b"100")
        .expect("policy");
    let exec = Executor::new(&store, ExecConfig::default()).expect("executor");
    let report = exec.swap_covered(&mut db).expect("swap phase");
    assert_eq!(report.swapped, 0, "{report:?}");
    assert_eq!(report.below_threshold, 1, "{report:?}");
    assert!(store.has(StoreNs::Data, &img_hash));
}
