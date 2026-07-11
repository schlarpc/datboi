//! The M3 headline, end to end: two similar disc-sized blobs get
//! content-defined-chunked by the refinement sweep, their chunks dedupe,
//! the originals are licensed (D25 replay) and evicted, and the NAS is
//! smaller — while both blobs still stream and serve verified ranges.

use std::fs;
use std::io::Read;

use datboi_core::hash::Blake3;
use datboi_exec::{ExecConfig, Executor};
use datboi_index::{Db, Residency};
use datboi_ingest::Ingester;
use datboi_ingest::analyzers::ChunkAnalyzer;
use datboi_ingest::refine::run_sweep;
use datboi_store_fs::{Namespace as StoreNs, Store};

fn pattern(len: usize, seed: u64) -> Vec<u8> {
    let mut state = seed;
    (0..len)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state >> 24) as u8
        })
        .collect()
}

#[test]
fn chunk_sweep_dedupes_and_eviction_shrinks_the_store() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Store::open(dir.path().join("store")).expect("store");
    let mut db = Db::open(dir.path()).expect("db");

    // Two 6 MiB images sharing everything except a 256 KiB patch in the
    // middle — the multi-region-variant shape CDC exists for.
    let size = 6 << 20;
    let alpha = pattern(size, 0xAAAA_BBBB_CCCC_DDDD);
    let mut beta = alpha.clone();
    let patch = pattern(256 * 1024, 0x1111_2222_3333_4444);
    beta[3_000_000..3_000_000 + patch.len()].copy_from_slice(&patch);

    let src = dir.path().join("src");
    fs::create_dir_all(&src).expect("mkdir");
    fs::write(src.join("alpha.iso"), &alpha).expect("write");
    fs::write(src.join("beta.iso"), &beta).expect("write");
    let report = Ingester::new(&store, &mut db, &[]).ingest(&[&src]);
    assert_eq!(report.errors.len(), 0, "{:?}", report.errors);
    assert_eq!(report.files_stored, 2);

    let alpha_hash = Blake3::compute(&alpha);
    let beta_hash = Blake3::compute(&beta);

    // Refinement sweep: both images chunked, provenance positive.
    let sweep = run_sweep(&mut db, &store, &mut ChunkAnalyzer, 10_000).expect("sweep");
    assert_eq!(sweep.errors.len(), 0, "{:?}", sweep.errors);
    assert_eq!(sweep.positive, 2, "both images chunked");

    // A second sweep only sees the new chunk blobs — all below the
    // threshold, all negative, nothing re-analyzed (the fixpoint).
    let sweep2 = run_sweep(&mut db, &store, &mut ChunkAnalyzer, 10_000).expect("sweep");
    assert_eq!(sweep2.positive, 0);
    assert!(sweep2.analyzed > 0, "chunks got their negative rows");
    let sweep3 = run_sweep(&mut db, &store, &mut ChunkAnalyzer, 10_000).expect("sweep");
    assert_eq!(sweep3.analyzed, 0, "at rest");

    let resident_bytes = |db: &Db| -> u64 {
        let n: i64 = db
            .cache()
            .query_row(
                "SELECT COALESCE(SUM(size), 0) FROM blob WHERE namespace = 0 AND residency = 0",
                [],
                |row| row.get(0),
            )
            .expect("q");
        u64::try_from(n).expect("non-negative")
    };
    let before = resident_bytes(&db);
    assert!(before as usize >= 2 * size, "originals + chunks resident");

    // Evict with licensing: recipes replay (D25), originals drop, chunks
    // stay. The shared content is now stored ONCE.
    let exec = Executor::new(&store, ExecConfig::default()).expect("executor");
    let evict_report = exec.evict_covered(&db, 0, true).expect("planner");
    assert_eq!(evict_report.evicted, 2, "both originals evicted");
    assert_eq!(evict_report.replays, 2, "licensing replays ran");
    assert!(!store.has(StoreNs::Data, &alpha_hash));
    assert!(!store.has(StoreNs::Data, &beta_hash));

    let after = resident_bytes(&db);
    // 2 × 6 MiB of originals became ~6.25 MiB + CDC slop of chunks: the
    // shared 5.75 MiB is stored once. Anything under 1.5× a single image
    // proves cross-image dedup happened.
    assert!(
        after < (size as u64 * 3) / 2,
        "dedup shrank the store: {before} -> {after} bytes resident"
    );

    // Both images still serve, fully and by verified range.
    for (hash, expected) in [(alpha_hash, &alpha), (beta_hash, &beta)] {
        let mut streamed = Vec::new();
        exec.open_stream(&db, &hash)
            .expect("route")
            .read_to_end(&mut streamed)
            .expect("read");
        assert_eq!(&streamed, expected, "full stream after eviction");
        let got = exec
            .serve_range(&db, &hash, 2_999_900, 300)
            .expect("verified range straddling the patch boundary");
        assert_eq!(got, &expected[2_999_900..3_000_200]);
        assert_eq!(
            db.blob_by_hash(&hash).expect("q").expect("row").residency,
            Residency::EvictedCovered
        );
    }
}

/// D59: a big literal that already has a covering route is NOT chunked —
/// it's already evictable through that route; chunking it would add
/// recipe metadata for no marginal dedup.
#[test]
fn chunking_skips_already_routed_blobs() {
    use datboi_index::recipes::NewRecipe;
    use datboi_index::{Namespace as IndexNs, OpKind, RecipeSource, Residency, SeekClass};

    let dir = tempfile::tempdir().expect("tempdir");
    let store = Store::open(dir.path().join("store")).expect("store");
    let mut db = Db::open(dir.path()).expect("db");

    let size = 6 << 20;
    let big = pattern(size, 0x5555_6666_7777_8888);
    let big_hash = Blake3::compute(&big);
    store
        .put(StoreNs::Data, big_hash, big.as_slice())
        .expect("put");
    let big_id = db
        .upsert_blob(
            &big_hash,
            Some(size as u64),
            IndexNs::Data,
            Residency::Resident,
        )
        .expect("upsert");

    // Give it a covering route (row-level is enough: eligibility reads
    // the OR-graph, it never executes the recipe).
    let meta_hash = Blake3::compute(b"pretend recipe object");
    let meta_id = db
        .upsert_blob(&meta_hash, Some(32), IndexNs::Meta, Residency::Resident)
        .expect("upsert meta");
    db.insert_recipe(&NewRecipe {
        blob_id: meta_id,
        op_kind: OpKind::Builtin,
        op_name: "assemble@1",
        seek_class: SeekClass::Affine,
        source: RecipeSource::LocalIngest,
        inputs: &[(0, big_id, None)],
        outputs: &[(0, big_id, size as u64, None)],
    })
    .expect("insert recipe");

    let blobs_before: i64 = db
        .cache()
        .query_row("SELECT COUNT(*) FROM blob", [], |row| row.get(0))
        .expect("count");
    let sweep = run_sweep(&mut db, &store, &mut ChunkAnalyzer, 10_000).expect("sweep");
    assert_eq!(sweep.errors.len(), 0, "{:?}", sweep.errors);
    assert_eq!(sweep.positive, 0, "routed blob is skipped (D59)");
    assert!(sweep.analyzed >= 1, "it still got its negative row");
    let blobs_after: i64 = db
        .cache()
        .query_row("SELECT COUNT(*) FROM blob", [], |row| row.get(0))
        .expect("count");
    assert_eq!(blobs_before, blobs_after, "no chunk blobs were minted");
    assert_eq!(
        db.recipes_for_output(big_id).expect("recipes").len(),
        1,
        "no chunk recipe was added"
    );
}
