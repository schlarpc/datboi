//! Refinement analyzers over real stores: provenance recorded both ways,
//! negatives never re-paid (D45/D48), discovery honest about D24.

use std::io::Write as _;

use datboi_core::hash::Blake3;
use datboi_index::{AnalysisOutcome, Db, Namespace as IndexNs, Residency};
use datboi_ingest::analyzers::DeflateTrialAnalyzer;
use datboi_ingest::refine::{Analyzer, run_sweep};
use datboi_store_fs::{Namespace as StoreNs, Store};
use flate2::Compression;
use flate2::write::DeflateEncoder;

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
fn deflate_trial_discovers_rebuildability_and_records_negatives() {
    let (_dir, store, mut db) = world();
    let payload: Vec<u8> = (0..100_000u32).map(|i| (i % 251) as u8).collect();

    // A zip whose member WAS produced by our deflate (miniz level 6):
    // trial must rediscover it.
    let ours = {
        let mut enc = DeflateEncoder::new(Vec::new(), Compression::new(6));
        enc.write_all(&payload).expect("deflate");
        enc.finish().expect("finish")
    };
    let (rebuildable_hash, rebuildable_id) = put(&store, &db, &zip_with_member(&payload, &ours));

    // A zip whose member deflate stream came from "somewhere else": a
    // valid stream miniz won't emit (stored/no-compression deflate
    // blocks). Trial must record the negative.
    let foreign = {
        let mut enc = DeflateEncoder::new(Vec::new(), Compression::none());
        enc.write_all(&payload).expect("deflate");
        enc.finish().expect("finish")
    };
    assert_ne!(foreign, ours);
    let (literal_hash, literal_id) = put(&store, &db, &zip_with_member(&payload, &foreign));

    // A non-zip blob: negative, cheap.
    let (_plain_hash, plain_id) = put(&store, &db, b"just bytes, not a container");

    let mut analyzer = DeflateTrialAnalyzer;
    let report = run_sweep(&mut db, &store, &mut analyzer, 1000).expect("sweep");
    assert_eq!(report.errors.len(), 0, "{:?}", report.errors);
    assert_eq!(report.analyzed, 3);
    assert_eq!(report.positive, 1);
    assert_eq!(report.negative, 2);

    let id = analyzer.id();
    assert_eq!(
        db.analysis_outcome(rebuildable_id, &id).expect("q"),
        Some(AnalysisOutcome::Positive),
        "{rebuildable_hash} was made by our deflate — rediscovered"
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

    // Level-none trick note: Compression::none() emits stored deflate
    // blocks — a legal stream our LEVELS search never reproduces, which
    // is exactly the "foreign compressor" shape.

    // Nothing re-pays: the next sweep is a no-op.
    let again = run_sweep(&mut db, &store, &mut analyzer, 1000).expect("sweep");
    assert_eq!(again.analyzed, 0, "fixpoint at rest");
}
