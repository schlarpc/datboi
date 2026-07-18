//! Refinement analyzers over real stores: provenance recorded both ways,
//! negatives never re-paid (D45/D48), discovery honest about D24.

use std::io::Write as _;

use datboi_core::hash::Blake3;
use datboi_index::{AnalysisOutcome, Db, Namespace as IndexNs, Residency};
use datboi_ingest::analyzers::PreflateZipAnalyzer;
use datboi_ingest::refine::{Analyzer, Logical, SweepReport, run_sweep};
use datboi_store_fs::{Namespace as StoreNs, Store};
use flate2::Compression;
use flate2::write::DeflateEncoder;

/// One sweep through the D92 logical-CAS shape (executor-backed reads).
fn sweep_all(db: &mut Db, store: &Store, analyzer: &mut dyn Analyzer, limit: usize) -> SweepReport {
    let exec =
        datboi_exec::Executor::new(store, datboi_exec::ExecConfig::default()).expect("executor");
    let bytes = Logical::new(store, &exec);
    run_sweep(db, store, &bytes, analyzer, limit).expect("sweep")
}

fn world() -> (tempfile::TempDir, Store, Db) {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Store::open(dir.path().join("store")).expect("store");
    let db = Db::open(dir.path()).expect("db");
    (dir, store, db)
}

fn put(store: &Store, db: &Db, bytes: &[u8]) -> (Blake3, i64) {
    let hash = Blake3::compute(bytes);
    store.put(StoreNs::Data, hash, bytes).expect("put");
    let id = db
        .upsert_blob(
            &hash,
            Some(bytes.len() as u64),
            IndexNs::Data,
            Residency::Resident,
        )
        .expect("row");
    (hash, id)
}

/// Minimal zip with one DEFLATE member compressed by `level`.
fn zip_with_member(payload: &[u8], compressed: &[u8]) -> Vec<u8> {
    let crc = {
        let mut h = crc32fast::Hasher::new();
        h.update(payload);
        h.finalize()
    };
    let name = b"m.bin";
    let mut out = Vec::new();
    out.extend_from_slice(b"PK\x03\x04");
    out.extend_from_slice(&20u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&8u16.to_le_bytes());
    out.extend_from_slice(&[0; 4]);
    out.extend_from_slice(&crc.to_le_bytes());
    out.extend_from_slice(
        &u32::try_from(compressed.len())
            .expect("small")
            .to_le_bytes(),
    );
    out.extend_from_slice(&u32::try_from(payload.len()).expect("small").to_le_bytes());
    out.extend_from_slice(&u16::try_from(name.len()).expect("small").to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(name);
    out.extend_from_slice(compressed);
    let cd_offset = out.len();
    out.extend_from_slice(b"PK\x01\x02");
    out.extend_from_slice(&20u16.to_le_bytes());
    out.extend_from_slice(&20u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&8u16.to_le_bytes());
    out.extend_from_slice(&[0; 4]);
    out.extend_from_slice(&crc.to_le_bytes());
    out.extend_from_slice(
        &u32::try_from(compressed.len())
            .expect("small")
            .to_le_bytes(),
    );
    out.extend_from_slice(&u32::try_from(payload.len()).expect("small").to_le_bytes());
    out.extend_from_slice(&u16::try_from(name.len()).expect("small").to_le_bytes());
    out.extend_from_slice(&[0; 12]);
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(name);
    let cd_len = out.len() - cd_offset;
    out.extend_from_slice(b"PK\x05\x06");
    out.extend_from_slice(&[0; 4]);
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&u32::try_from(cd_len).expect("small").to_le_bytes());
    out.extend_from_slice(&u32::try_from(cd_offset).expect("small").to_le_bytes());
    out.extend_from_slice(&[0; 2]);
    out
}

#[test]
fn preflate_split_mints_rebuild_recipes_and_records_negatives() {
    let (_dir, store, mut db) = world();
    let payload: Vec<u8> = (0..100_000u32)
        .map(|i| (i % 251) as u8 ^ (i / 997) as u8)
        .collect();

    // Any zlib-family deflate splits — the compressor no longer needs to
    // be ours (D53). miniz level 6 here; production streams are wild.
    let compressed = {
        let mut enc = DeflateEncoder::new(Vec::new(), Compression::new(6));
        enc.write_all(&payload).expect("deflate");
        enc.finish().expect("finish")
    };
    let (rebuildable_hash, rebuildable_id) =
        put(&store, &db, &zip_with_member(&payload, &compressed));

    // A zip with a truncated member stream: inflates up to a point, then
    // the split fails deterministically — the negative D48 records.
    let truncated = &compressed[..compressed.len() - 4];
    let (literal_hash, literal_id) = put(&store, &db, &zip_with_member(&payload, truncated));

    // A non-zip blob: negative, cheap.
    let (_plain_hash, plain_id) = put(&store, &db, b"just bytes, not a container");

    let mut analyzer = PreflateZipAnalyzer::new();
    let report = sweep_all(&mut db, &store, &mut analyzer, 1000);
    assert_eq!(report.errors.len(), 0, "{:?}", report.errors);
    assert_eq!(report.analyzed, 3);
    assert_eq!(report.positive, 1);
    assert_eq!(report.negative, 2);

    let id = analyzer.id();
    assert_eq!(
        db.analysis_outcome(rebuildable_id, &id).expect("q"),
        Some(AnalysisOutcome::Positive),
        "{rebuildable_hash} splits: recipes minted"
    );
    assert_eq!(
        db.analysis_outcome(literal_id, &id).expect("q"),
        Some(AnalysisOutcome::Negative),
        "{literal_hash} stays literal (D24), negative recorded forever (D48)"
    );
    assert_eq!(
        db.analysis_outcome(plain_id, &id).expect("q"),
        Some(AnalysisOutcome::Negative)
    );

    // The split's products are all in the store: member plaintext (alias-
    // indexed for dat audit), the corrections blob, the skeleton, and the
    // pinned component itself.
    let plaintext_hash = Blake3::compute(&payload);
    assert!(
        store.has(StoreNs::Data, &plaintext_hash),
        "plaintext stored"
    );
    assert!(
        store.has(StoreNs::Data, &PreflateZipAnalyzer::component_hash()),
        "component published as an ordinary CAS blob"
    );
    let plaintext_row = db
        .blob_by_hash(&plaintext_hash)
        .expect("q")
        .expect("plaintext indexed");
    assert_eq!(plaintext_row.residency, Residency::Resident);

    // The container's rebuild route exists: an assemble recipe claims it.
    let container_row = db
        .blob_by_hash(&rebuildable_hash)
        .expect("q")
        .expect("container row");
    let recipes = db.recipes_for_output(container_row.blob_id).expect("q");
    assert!(
        !recipes.is_empty(),
        "container is recipe-covered after the sweep"
    );

    // A second sweep sees only the new products; none are zips, so all
    // negative — and a third finds nothing (the fixpoint at rest).
    let again = sweep_all(&mut db, &store, &mut analyzer, 1000);
    assert_eq!(again.positive, 0, "no zips among split products");
    let rest = sweep_all(&mut db, &store, &mut analyzer, 1000);
    assert_eq!(rest.analyzed, 0, "fixpoint at rest");
}

/// D93: N workers over ONE leased queue — claims are atomic under
/// WAL, so concurrent drains never duplicate an analysis (leases are
/// dedup; at-least-once only re-runs after a lease EXPIRES, which
/// this test never waits for). The workers share one executor
/// (`Executor` is `Sync`) and own private `Db` connections — the
/// exact daemon drone shape.
/// D92: the prime enqueues every family, THEN runs ONE admission pass
/// (the grounding fixpoint) — not one per family. This proves the two
/// halves are independently usable and idempotent, the property that
/// dedup relies on; `refresh_queue` (run_sweep) still bundles both.
#[test]
fn split_enqueue_and_admission_are_independent_and_idempotent() {
    use datboi_ingest::refine::{Analyzer, NoopAnalyzer, enqueue_candidates, refresh_admission};

    let (_dir, store, db) = world();
    for i in 0..5 {
        put(&store, &db, format!("split blob {i}").as_bytes());
    }
    // Per-family enqueue, dat-blind over resident blobs.
    assert_eq!(enqueue_candidates(&db, &NoopAnalyzer).expect("enqueue"), 5);
    // The once-per-wake admission pass, separable from enqueue.
    refresh_admission(&db).expect("admission");
    // Re-running both is a no-op / safe: INSERT OR IGNORE means no
    // double-enqueue, and admission is a rebuild of derivable state.
    assert_eq!(
        enqueue_candidates(&db, &NoopAnalyzer).expect("re-enqueue"),
        0
    );
    refresh_admission(&db).expect("admission again");
    assert_eq!(
        db.sweep_queue_len(&NoopAnalyzer.id()).expect("len"),
        5,
        "resident items sit claimable in the queue"
    );
}

#[test]
fn concurrent_drains_share_the_queue_without_duplication() {
    use datboi_ingest::refine::{NoObserver, NoopAnalyzer, process_round, refresh_queue};

    let (dir, store, mut db) = world();
    const BLOBS: usize = 40;
    for i in 0..BLOBS {
        put(&store, &db, format!("d93 blob {i}").as_bytes());
    }
    let enqueued = refresh_queue(&mut db, &NoopAnalyzer).expect("refresh");
    assert_eq!(enqueued, BLOBS);

    let exec =
        datboi_exec::Executor::new(&store, datboi_exec::ExecConfig::default()).expect("executor");
    std::thread::scope(|s| {
        for _ in 0..2 {
            s.spawn(|| {
                let mut db = Db::open(dir.path()).expect("drone db");
                let bytes = Logical::new(&store, &exec);
                let mut analyzer = NoopAnalyzer;
                loop {
                    let report =
                        process_round(&mut db, &store, &bytes, &mut analyzer, 4, &mut NoObserver)
                            .expect("round");
                    assert!(report.errors.is_empty(), "{:?}", report.errors);
                    if report.analyzed == 0 {
                        break;
                    }
                }
            });
        }
    });

    // Exactly-once: one provenance row per blob, an empty queue.
    let rows: i64 = db
        .cache()
        .query_row("SELECT COUNT(*) FROM analysis", [], |r| r.get(0))
        .expect("q");
    assert_eq!(rows as usize, BLOBS, "no duplicates, no losses");
    assert_eq!(
        db.sweep_queue_len(&{
            use datboi_ingest::refine::Analyzer as _;
            NoopAnalyzer.id()
        })
        .expect("q"),
        0
    );
}

/// D108: the roster sorts by class (structural families strictly before
/// fallback), and `process_round` refuses to claim a blob for a
/// fallback analyzer while a structural family still has it queued —
/// the drone-fleet race that chunked a bin mid-ecm, reproduced at the
/// claim layer.
#[test]
fn class_gate_holds_fallback_until_structural_settles() {
    use datboi_ingest::analyzers::{ChunkAnalyzer, EcmAnalyzer, sweep_roster};
    use datboi_ingest::refine::{AnalyzerClass, NoObserver, process_round};

    // Roster order is the class order, chunk (fallback) at the end.
    let roster = sweep_roster();
    let classes: Vec<AnalyzerClass> = roster.iter().map(|a| a.class()).collect();
    let mut sorted = classes.clone();
    sorted.sort();
    assert_eq!(classes, sorted, "roster must be class-sorted");
    assert_eq!(
        roster.last().expect("non-empty").family(),
        "chunk",
        "the CDC fallback drains last"
    );

    let (_dir, store, mut db) = world();
    let (_hash, id) = put(&store, &db, b"gated bytes");
    let ecm_id = EcmAnalyzer::new().id();
    db.enqueue_fresh(&ecm_id, &[id], 10).expect("enqueue ecm");
    db.enqueue_fresh(&ChunkAnalyzer.id(), &[id], 10)
        .expect("enqueue chunk");

    let exec =
        datboi_exec::Executor::new(&store, datboi_exec::ExecConfig::default()).expect("executor");
    let bytes = Logical::new(&store, &exec);

    // ecm's queue row gates the chunk claim entirely.
    let gated = process_round(
        &mut db,
        &store,
        &bytes,
        &mut ChunkAnalyzer,
        5,
        &mut NoObserver,
    )
    .expect("round");
    assert_eq!(gated.analyzed, 0, "fallback must not claim past ecm");

    // ecm settling opens the gate on the next round.
    db.complete_sweep_item(id, &ecm_id, AnalysisOutcome::Negative, None, 20)
        .expect("settle");
    let opened = process_round(
        &mut db,
        &store,
        &bytes,
        &mut ChunkAnalyzer,
        5,
        &mut NoObserver,
    )
    .expect("round");
    assert_eq!(opened.analyzed, 1, "settled structural row frees the blob");
}
