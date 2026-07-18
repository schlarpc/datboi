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

/// One sweep through the D92 logical-CAS shape (executor-backed reads).
fn sweep_all(
    db: &mut datboi_index::Db,
    store: &datboi_store_fs::Store,
    analyzer: &mut dyn datboi_ingest::refine::Analyzer,
    limit: usize,
) -> datboi_ingest::refine::SweepReport {
    let exec = Executor::new(store, ExecConfig::default()).expect("executor");
    let bytes = datboi_ingest::refine::Logical::new(store, &exec);
    run_sweep(db, store, &bytes, analyzer, limit).expect("sweep")
}

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
    let sweep = sweep_all(&mut db, &store, &mut ChunkAnalyzer, 10_000);
    assert_eq!(sweep.errors.len(), 0, "{:?}", sweep.errors);
    assert_eq!(sweep.positive, 2, "both images chunked");

    // A second sweep only sees the new chunk blobs — all below the
    // threshold, all negative, nothing re-analyzed (the fixpoint).
    let sweep2 = sweep_all(&mut db, &store, &mut ChunkAnalyzer, 10_000);
    assert_eq!(sweep2.positive, 0);
    assert!(sweep2.analyzed > 0, "chunks got their negative rows");
    let sweep3 = sweep_all(&mut db, &store, &mut ChunkAnalyzer, 10_000);
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

/// D91/D59 pack-per-chunking: a chunk set's loose flood consolidates
/// into ONE sealed pack, the original evicts, and it still serves
/// byte-exact and range-verified through the packed chunks.
#[test]
fn pack_chunk_sets_consolidates_the_loose_flood() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Store::open(dir.path().join("store")).expect("store");
    let mut db = Db::open(dir.path()).expect("db");

    // One 6 MiB image → a CDC set of dozens of loose chunks.
    let size = 6 << 20;
    let blob = pattern(size, 0xFEED_FACE_1234_5678);
    let src = dir.path().join("src");
    fs::create_dir_all(&src).expect("mkdir");
    fs::write(src.join("big.iso"), &blob).expect("write");
    assert_eq!(
        Ingester::new(&store, &mut db, &[])
            .ingest(&[&src])
            .files_stored,
        1
    );
    let hash = Blake3::compute(&blob);

    let sweep = sweep_all(&mut db, &store, &mut ChunkAnalyzer, 10_000);
    assert_eq!(sweep.positive, 1, "the image chunked");

    // Capture the chunk hashes while the original is still a swap
    // candidate (resident, affine assemble route).
    let exec = Executor::new(&store, ExecConfig::default()).expect("exec");
    let cand = db
        .swap_candidates()
        .expect("candidates")
        .into_iter()
        .find(|c| c.hash == hash)
        .expect("the original's rebuild route");
    let chunks: Vec<Blake3> = db
        .rebuild_inputs(cand.recipe_id)
        .expect("inputs")
        .into_iter()
        .map(|i| i.hash)
        .collect();
    assert!(chunks.len() >= 4, "a flood worth packing");
    assert!(chunks.iter().all(|c| store.has_loose(StoreNs::Data, c)));

    // Pack the set: one pack, every chunk packed, loose copies dropped.
    let report = exec.pack_chunk_sets(&mut db).expect("pack");
    assert_eq!(report.sets_packed, 1, "skipped: {:?}", report.skipped);
    assert_eq!(report.members, chunks.len());
    assert_eq!(store.list_packs().len(), 1, "one pack for the set");
    for c in &chunks {
        assert!(store.is_packed(c), "chunk packed");
        assert!(!store.has_loose(StoreNs::Data, c), "loose .data dropped");
        // D105: the pack carries the tree, so the loose sidecar drops
        // too — the inode win covers both files.
        let hex = c.to_hex();
        let sidecar = dir
            .path()
            .join("store")
            .join("data")
            .join(&hex[0..2])
            .join(&hex[2..4])
            .join(format!("{hex}.obao4"));
        assert!(!sidecar.exists(), "loose .obao4 dropped");
        // And the outboard still resolves — out of the pack's section.
        assert!(
            store
                .get_obao(StoreNs::Data, c)
                .expect("get_obao")
                .is_some(),
            "tree serves from the pack"
        );
    }

    // Evict the original: it now routes through the PACKED chunks.
    let evicted = exec.evict_covered(&db, 0, true).expect("evict");
    assert!(evicted.evicted >= 1);
    assert!(!store.has_loose(StoreNs::Data, &hash), "original dropped");

    // Serves byte-exact and range-verified from the packed chunks.
    let mut streamed = Vec::new();
    exec.open_stream(&db, &hash)
        .expect("route")
        .read_to_end(&mut streamed)
        .expect("read");
    assert_eq!(streamed, blob, "byte-exact through packed chunks");
    let mid = exec.serve_range(&db, &hash, 1 << 20, 4096).expect("range");
    assert_eq!(mid, blob[1 << 20..(1 << 20) + 4096]);

    // Idempotent: nothing loose remains to pack or sweep.
    let again = exec.pack_chunk_sets(&mut db).expect("again");
    assert_eq!((again.sets_packed, again.swept_loose), (0, 0));

    // Footer-truth: a fresh open resolves the packed chunks and serves.
    drop(exec);
    drop(store);
    let store = Store::open(dir.path().join("store")).expect("reopen");
    let exec = Executor::new(&store, ExecConfig::default()).expect("exec");
    assert!(store.is_packed(&chunks[0]));
    let mut again = Vec::new();
    exec.open_stream(&db, &hash)
        .expect("route")
        .read_to_end(&mut again)
        .expect("read");
    assert_eq!(again, blob, "serves after reopen");
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

    // Give it a REAL covering route: big assembles from a SEPARATE
    // resident blob, so dropping big's own bytes still leaves it
    // reconstructible (is_covered_by_others → true). A self-referential
    // recipe would NOT count — that is the rank-7 grounding-leaf shape
    // the sibling test exercises. (Row-level is enough: eligibility
    // reads the OR-graph, it never executes the recipe.)
    let src = pattern(4096, 0x9999_AAAA_BBBB_CCCC);
    let src_hash = Blake3::compute(&src);
    store
        .put(StoreNs::Data, src_hash, src.as_slice())
        .expect("put src");
    let src_id = db
        .upsert_blob(
            &src_hash,
            Some(src.len() as u64),
            IndexNs::Data,
            Residency::Resident,
        )
        .expect("upsert src");
    let meta_hash = Blake3::compute(b"pretend recipe object");
    let meta_id = db
        .upsert_blob(&meta_hash, Some(32), IndexNs::Meta, Residency::Resident)
        .expect("upsert meta");
    let rid = db
        .insert_recipe(&NewRecipe {
            blob_id: meta_id,
            op_kind: OpKind::Builtin,
            op_name: "assemble@1",
            seek_class: SeekClass::Affine,
            source: RecipeSource::LocalIngest,
            inputs: &[(0, src_id, None)],
            outputs: &[(0, big_id, size as u64, None)],
        })
        .expect("insert recipe");
    // Real minted recipes are Verified (D4); grounding only trusts
    // non-pending claims.
    db.set_verify_state(rid, datboi_index::VerifyAdvance::Verified, 1)
        .expect("verify");

    let blobs_before: i64 = db
        .cache()
        .query_row("SELECT COUNT(*) FROM blob", [], |row| row.get(0))
        .expect("count");
    let sweep = sweep_all(&mut db, &store, &mut ChunkAnalyzer, 10_000);
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

/// Rank-7 D59 amendment (D91): a big GROUNDING-LEAF piece — one whose
/// only recipe derives it from an absent container that itself grounds
/// via this very piece — IS chunked, so its cross-variant near-misses
/// dedup. The old has-any-recipe gate mispredicted it as "covered".
#[test]
fn rank7_chunks_a_grounding_leaf_piece() {
    use datboi_index::recipes::NewRecipe;
    use datboi_index::{Namespace as IndexNs, OpKind, RecipeSource, Residency, SeekClass};

    let dir = tempfile::tempdir().expect("tempdir");
    let store = Store::open(dir.path().join("store")).expect("store");
    let mut db = Db::open(dir.path()).expect("db");

    // A 6 MiB resident piece.
    let size = 6 << 20;
    let piece = pattern(size, 0xABCD_1234_5678_9AB0);
    let piece_hash = Blake3::compute(&piece);
    store
        .put(StoreNs::Data, piece_hash, piece.as_slice())
        .expect("put");
    let piece_id = db
        .upsert_blob(
            &piece_hash,
            Some(size as u64),
            IndexNs::Data,
            Residency::Resident,
        )
        .expect("upsert piece");

    // Its container is ABSENT and grounds via this piece — the D91
    // mutual-inverse shape: container→piece (the decomposition slice)
    // and piece→container (the rebuild). The piece is therefore a
    // grounding leaf: route-less to the fixpoint despite two recipe rows.
    let cont_hash = Blake3::compute(b"absent container");
    let cont_id = db
        .upsert_blob(
            &cont_hash,
            Some(size as u64),
            IndexNs::Data,
            Residency::Absent,
        )
        .expect("upsert container");
    for (meta, input, out) in [
        (b"slice".as_slice(), cont_id, piece_id),
        (b"rebuild".as_slice(), piece_id, cont_id),
    ] {
        let meta_id = db
            .upsert_blob(
                &Blake3::compute(meta),
                Some(32),
                IndexNs::Meta,
                Residency::Resident,
            )
            .expect("meta");
        let rid = db
            .insert_recipe(&NewRecipe {
                blob_id: meta_id,
                op_kind: OpKind::Builtin,
                op_name: "assemble@1",
                seek_class: SeekClass::Affine,
                source: RecipeSource::LocalIngest,
                inputs: &[(0, input, None)],
                outputs: &[(0, out, size as u64, None)],
            })
            .expect("recipe");
        db.set_verify_state(rid, datboi_index::VerifyAdvance::Verified, 1)
            .expect("verify");
    }

    // The piece carries recipe rows but is NOT reconstructible from
    // other bytes — the distinction the amendment turns on.
    assert!(
        !db.recipes_for_output(piece_id).expect("recipes").is_empty(),
        "the piece has recipe rows (routed on paper)"
    );
    assert!(
        !db.is_covered_by_others(piece_id).expect("cover"),
        "grounding leaf: route-less to the fixpoint"
    );

    // So the chunker chunks it (the old gate would have skipped it).
    let sweep = sweep_all(&mut db, &store, &mut ChunkAnalyzer, 10_000);
    assert_eq!(sweep.errors.len(), 0, "{:?}", sweep.errors);
    assert_eq!(sweep.positive, 1, "the grounding-leaf piece got chunked");
    // A chunk-assemble recipe now covers the piece for real (the slice
    // recipe outputs the piece too; the rebuild outputs the container).
    assert!(
        db.recipes_for_output(piece_id).expect("recipes").len() >= 2,
        "the slice recipe plus the new chunk recipe"
    );
}
