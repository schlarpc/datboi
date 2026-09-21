//! `chd-verify` over a real store (D44 amendment): the decompressing
//! check that moves a CHD's disk claims from `probable` to
//! have-verified, and every way it declines to.
//!
//! The CHDs here are synthesised by `datboi-formats` — no disc data
//! lands in the tree — but they are complete, valid files: real maps,
//! real metadata chains, real codec streams.

use datboi_core::hash::Blake3;
use datboi_formats::chd::{self, CODEC_AVHUFF, CODEC_ZLIB};
use datboi_index::{AliasAlgo, BASIS_DECLARED, BASIS_SHA1, Db, Namespace as IndexNs, Residency};
use datboi_ingest::analyzers::ChdVerifyAnalyzer;
use datboi_ingest::refine::{SweepReport, run_sweep};
use datboi_store_fs::{Namespace as StoreNs, Store};

fn world() -> (tempfile::TempDir, Store, Db) {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Store::open(dir.path().join("store")).expect("store");
    let db = Db::open(dir.path()).expect("db");
    (dir, store, db)
}

fn put(store: &Store, db: &Db, bytes: &[u8]) -> Blake3 {
    let hash = Blake3::compute(bytes);
    store.put(StoreNs::Data, hash, bytes).expect("put");
    db.upsert_blob(
        &hash,
        Some(bytes.len() as u64),
        IndexNs::Data,
        Residency::Resident,
    )
    .expect("row");
    hash
}

fn sweep(db: &mut Db, store: &Store) -> SweepReport {
    let exec =
        datboi_exec::Executor::new(store, datboi_exec::ExecConfig::default()).expect("executor");
    let bytes = datboi_ingest::refine::Logical::new(store, &exec);
    let mut analyzer = ChdVerifyAnalyzer;
    run_sweep(db, store, &bytes, &mut analyzer, 100).expect("sweep")
}

fn details(db: &Db) -> Vec<String> {
    db.cache()
        .prepare("SELECT COALESCE(detail,'') FROM analysis")
        .expect("q")
        .query_map([], |r| r.get(0))
        .expect("q")
        .collect::<Result<_, _>>()
        .expect("q")
}

/// A disk claim's shape in the catalog: a sha1 and no size.
fn disk_claim(db: &Db, sha1: &[u8; 20]) -> i64 {
    db.cache()
        .execute(
            "INSERT INTO content_identity (size, crc32, md5, sha1, sha256, strength)
             VALUES (NULL, NULL, NULL, ?1, NULL, 2)",
            (sha1.as_slice(),),
        )
        .expect("identity");
    db.cache().last_insert_rowid()
}

fn basis(db: &Db, identity_id: i64) -> Option<i64> {
    db.cache()
        .query_row(
            "SELECT basis FROM identity_blob WHERE identity_id = ?1",
            (identity_id,),
            |r| r.get(0),
        )
        .ok()
}

fn wave(len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(len);
    let mut phase = 0i32;
    while out.len() < len {
        phase = (phase + 37) % 512;
        out.extend_from_slice(&i16::try_from(phase - 256).expect("small").to_be_bytes());
    }
    out.truncate(len);
    out
}

/// The headline: a stored CHD, a disk claim naming its internal sha1
/// linked at declared (probable) grade the way ingest leaves it, and a
/// sweep that upgrades the link in place.
#[test]
fn verifying_a_chd_upgrades_its_disk_claim_from_probable() {
    let (_dir, store, mut db) = world();
    let spec = chd::synth::SynthSpec::new(5, CODEC_ZLIB);
    let bytes = chd::synth::build(&spec, &wave(4096 * 5));
    let hash = put(&store, &db, &bytes);
    let blob_id = db.get_blob_id(&hash).expect("q").expect("row");

    let sha1 = chd::parse_header(&bytes)
        .expect("parses")
        .declared_disk_sha1()
        .expect("v5 declares one");
    db.insert_declared_chd_sha1(blob_id, &sha1).expect("alias");
    let identity = disk_claim(&db, &sha1);
    db.cache()
        .execute(
            "INSERT INTO identity_blob (identity_id, blob_id, basis) VALUES (?1, ?2, ?3)",
            (identity, blob_id, BASIS_DECLARED),
        )
        .expect("declared link");
    assert_eq!(basis(&db, identity), Some(BASIS_DECLARED));

    let report = sweep(&mut db, &store);
    assert_eq!((report.analyzed, report.positive), (1, 1));
    assert_eq!(
        basis(&db, identity),
        Some(BASIS_SHA1),
        "the link must rise to sha1 strength, not stay at the declared grade"
    );
    // The computed digest lands in its own namespace, never in the real
    // sha1 one — it describes decompressed content, not these bytes.
    assert_eq!(
        db.alias_lookup(AliasAlgo::ChdSha1Verified, &sha1)
            .expect("q"),
        vec![blob_id]
    );
    assert!(
        db.alias_lookup(AliasAlgo::Sha1, &sha1)
            .expect("q")
            .is_empty(),
        "a CHD's internal sha1 must never answer a real sha1 lookup (D44)"
    );
    assert!(details(&db)[0].contains("disk claim(s) upgraded from probable"));
}

/// D44 ruled strictness precisely here: the header still declares the
/// disk, and the hunks are not there. The claim keeps its probable
/// grade and the verdict says why.
#[test]
fn a_truncated_chd_never_reaches_verified() {
    let (_dir, store, mut db) = world();
    let spec = chd::synth::SynthSpec::new(5, CODEC_ZLIB);
    let full = chd::synth::build(&spec, &wave(4096 * 6));
    let bytes = full[..full.len() * 2 / 3].to_vec();
    let hash = put(&store, &db, &bytes);
    let blob_id = db.get_blob_id(&hash).expect("q").expect("row");

    let sha1 = chd::parse_header(&bytes)
        .expect("the header survives truncation")
        .declared_disk_sha1()
        .expect("still declared");
    let identity = disk_claim(&db, &sha1);
    db.cache()
        .execute(
            "INSERT INTO identity_blob (identity_id, blob_id, basis) VALUES (?1, ?2, ?3)",
            (identity, blob_id, BASIS_DECLARED),
        )
        .expect("declared link");

    let report = sweep(&mut db, &store);
    assert_eq!((report.analyzed, report.negative), (1, 1));
    assert!(report.errors.is_empty(), "a short file is a conclusion");
    assert_eq!(basis(&db, identity), Some(BASIS_DECLARED));
    assert!(
        db.alias_lookup(AliasAlgo::ChdSha1Verified, &sha1)
            .expect("q")
            .is_empty()
    );
}

/// A codec this build cannot decode is named, and nothing is claimed on
/// the strength of the hunks that would have decoded.
#[test]
fn an_unsupported_codec_is_a_negative_with_the_codec_named() {
    let (_dir, store, mut db) = world();
    let mut bytes = chd::synth::build(&chd::synth::SynthSpec::new(5, CODEC_ZLIB), &wave(4096));
    bytes[16..20].copy_from_slice(&CODEC_AVHUFF.to_be_bytes());
    put(&store, &db, &bytes);

    let report = sweep(&mut db, &store);
    assert_eq!((report.analyzed, report.negative), (1, 1));
    assert!(report.errors.is_empty());
    let detail = &details(&db)[0];
    assert!(detail.contains("avhu"), "{detail}");
}

/// A header that declares a digest its own data does not produce is
/// damage. It is reported, and it stays `probable` — our digest does
/// not get to stand in for the dumper's.
#[test]
fn a_header_that_lies_about_its_data_stays_probable() {
    let (_dir, store, mut db) = world();
    let mut bytes = chd::synth::build(&chd::synth::SynthSpec::new(5, CODEC_ZLIB), &wave(4096 * 2));
    bytes[84] ^= 0xff;
    put(&store, &db, &bytes);

    let report = sweep(&mut db, &store);
    assert_eq!((report.analyzed, report.negative), (1, 1));
    let detail = &details(&db)[0];
    assert!(detail.contains("contradicts itself"), "{detail}");
}

/// v1 and v2 decompress and check out against their md5 — and still
/// earn no disk claim, because the format has no sha1 for one to name.
#[test]
fn a_v2_chd_verifies_but_answers_no_disk_claim() {
    let (_dir, store, mut db) = world();
    let bytes = chd::synth::build(&chd::synth::SynthSpec::new(2, CODEC_ZLIB), &wave(4096 * 3));
    put(&store, &db, &bytes);

    let report = sweep(&mut db, &store);
    assert_eq!((report.analyzed, report.negative), (1, 1));
    let detail = &details(&db)[0];
    assert!(detail.contains("declares no sha1"), "{detail}");
}

/// The family is offered every blob in the corpus. Rejecting a
/// non-CHD must be a settled conclusion, and must cost one read.
#[test]
fn ordinary_blobs_conclude_not_a_chd() {
    let (_dir, store, mut db) = world();
    put(&store, &db, b"PK\x03\x04 an ordinary zip, not a disc image");

    let report = sweep(&mut db, &store);
    assert_eq!((report.analyzed, report.negative), (1, 1));
    assert_eq!(details(&db), vec!["not a CHD".to_string()]);
}

/// Every version and every codec this build supports reaches verified
/// through the real sweep, not just through the library's own tests.
#[test]
fn every_supported_shape_verifies_through_the_sweep() {
    let (_dir, store, mut db) = world();
    let mut expected = 0;
    for version in 1..=5u32 {
        let bytes = chd::synth::build(
            &chd::synth::SynthSpec::new(version, CODEC_ZLIB),
            &wave(4096 * 2 + version as usize * 512),
        );
        put(&store, &db, &bytes);
        // v1/v2 have no sha1 to claim with, so they conclude negative.
        if version >= 3 {
            expected += 1;
        }
    }
    for &codec in &[
        chd::CODEC_LZMA,
        chd::CODEC_HUFFMAN,
        chd::CODEC_FLAC,
        chd::CODEC_NONE,
    ] {
        let bytes = chd::synth::build(&chd::synth::SynthSpec::new(5, codec), &wave(4096 * 3));
        put(&store, &db, &bytes);
        expected += 1;
    }
    let report = sweep(&mut db, &store);
    assert_eq!(report.analyzed, 9);
    assert_eq!(report.positive, expected, "{:?}", details(&db));
}
