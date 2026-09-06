//! wii-split (D116) over real stores: the synthetic Wii fixture with
//! real hash trees and junk, the deferred-key flow, and the refusals.
//! The critical assertions are the round trips — the disc through its
//! minted rebuild, and a FILE inside the partition through its derive
//! chain (slice of the plaintext ← `decrypt` of the body ← slice of the
//! disc), which runs the xf-wii-crypt component under wasmtime with
//! the key found by its hash.

use std::io::Read as _;

use datboi_core::hash::Blake3;
use datboi_exec::{ExecConfig, Executor};
use datboi_index::{Db, Namespace as IndexNs, Residency};
use datboi_ingest::analyzers::WiiAnalyzer;
use datboi_ingest::gcm::synth::Pad;
use datboi_ingest::refine::{Analyzer as _, run_sweep};
use datboi_ingest::wii::synth::{self, test_common_key};
use datboi_ingest::wii::{self, ISSUER_RETAIL, KnownKey};
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

/// The analyzer under test knows the fixture's key by hash — exactly
/// how it knows a console's.
fn analyzer() -> WiiAnalyzer {
    WiiAnalyzer::with_keys(vec![KnownKey {
        issuer: ISSUER_RETAIL,
        index: 0,
        hash: Blake3::compute(&test_common_key()),
    }])
}

fn sweep(
    db: &mut Db,
    store: &Store,
    analyzer: &mut WiiAnalyzer,
) -> datboi_ingest::refine::SweepReport {
    let exec = Executor::new(store, ExecConfig::default()).expect("executor");
    let bytes = datboi_ingest::refine::Logical::new(store, &exec);
    run_sweep(db, store, &bytes, analyzer, 100).expect("sweep")
}

fn analysis_details(db: &Db) -> Vec<String> {
    db.cache()
        .prepare("SELECT COALESCE(detail,'') FROM analysis")
        .expect("q")
        .query_map([], |r| r.get(0))
        .expect("q")
        .collect::<Result<_, _>>()
        .expect("q")
}

fn pattern(len: usize, salt: u32) -> Vec<u8> {
    (0..len)
        .map(|i| ((i as u32).wrapping_mul(2_654_435_761) ^ salt).to_le_bytes()[i % 4])
        .collect()
}

fn files() -> Vec<(&'static str, Vec<u8>)> {
    vec![
        ("/opening.bnr", pattern(6496, 1)),
        ("/data/big.arc", pattern(2_400_000, 2)), // spans a hash group boundary
        ("/data/small.dat", pattern(700, 3)),
        ("/empty.bin", Vec::new()),
        ("/last.bin", pattern(40_001, 5)),
    ]
}

fn stream(exec: &Executor, db: &Db, hash: &Blake3) -> Vec<u8> {
    let mut out = Vec::new();
    exec.open_stream(db, hash)
        .expect("route")
        .read_to_end(&mut out)
        .expect("stream");
    out
}

fn routes(db: &Db, hash: &Blake3) -> Vec<String> {
    let id = db.get_blob_id(hash).expect("q").expect("row");
    db.recipes_for_output(id)
        .expect("recipes")
        .iter()
        .map(|r| r.op_name.clone())
        .collect()
}

#[test]
fn disc_splits_verifies_the_partition_and_round_trips() {
    let disc = synth::disc(*b"RTSTD1", 0, &files(), Pad::Junk);
    let (_dir, store, mut db) = world();
    let key_hash = put(&store, &db, &disc.common_key);
    let img_hash = put(&store, &db, &disc.bytes);

    let report = sweep(&mut db, &store, &mut analyzer());
    assert_eq!(report.errors.len(), 0, "{:?}", report.errors);
    let details = analysis_details(&db);
    assert_eq!((report.positive, report.deferred), (1, 0), "{details:?}");
    // (The 16-byte key was swept too and is, correctly, not a disc.)
    let detail = details
        .iter()
        .find(|d| d.contains("split into"))
        .expect("verdict");
    assert!(
        detail.contains("1 partition(s), 1 walked (4 file(s))"),
        "{detail}"
    );
    assert!(detail.contains("regenerated"), "{detail}");
    assert!(!detail.contains("opaque"), "{detail}");

    // The body has two routes — the slice of the disc first, then the
    // encrypt — and the plaintext has decrypt then assemble.
    let body_hash = Blake3::compute(&disc.body);
    let plain_hash = Blake3::compute(&disc.plain);
    let crypt = WiiAnalyzer::component_hash().to_hex();
    assert_eq!(
        routes(&db, &body_hash),
        vec!["assemble@1".to_owned(), format!("{crypt}#encrypt")]
    );
    assert_eq!(
        routes(&db, &plain_hash),
        vec![format!("{crypt}#decrypt"), "assemble@1".to_owned()]
    );
    // The key is a plain resident blob the recipes name by hash.
    let plain_id = db.get_blob_id(&plain_hash).unwrap().unwrap();
    let decrypt = db.recipes_for_output(plain_id).unwrap()[0].recipe_id;
    let inputs = db.rebuild_inputs(decrypt).unwrap();
    assert_eq!(inputs.len(), 2);
    assert_eq!(inputs[1].hash, key_hash);

    let exec = Executor::new(&store, ExecConfig::default()).expect("executor");
    // The disc through its rebuild: every piece a slice, junk native.
    assert_eq!(stream(&exec, &db, &img_hash), disc.bytes);
    // The plaintext through its FIRST route — decrypt of the resident
    // body, under wasmtime, with the key found by hash.
    assert_eq!(stream(&exec, &db, &plain_hash), disc.plain);
    // A file inside the partition, through the whole derive chain.
    let big = disc
        .files
        .iter()
        .find(|(p, _)| p == "/data/big.arc")
        .unwrap();
    let big_hash = Blake3::compute(&big.1);
    assert_eq!(stream(&exec, &db, &big_hash), big.1);
    // The junk stream is one blob over the disc's address space, never
    // stored; the rebuilds of the disc AND the plaintext reference it.
    let disc_id = db.get_blob_id(&img_hash).unwrap().unwrap();
    let rebuild = db.recipes_for_output(disc_id).unwrap()[0].recipe_id;
    let junk: Vec<_> = db
        .rebuild_inputs(rebuild)
        .unwrap()
        .into_iter()
        .filter(|i| i.generated)
        .collect();
    assert_eq!(junk.len(), 1);
    assert_eq!(junk[0].size, Some(disc.bytes.len() as u64));
    assert!(!store.has(StoreNs::Data, &junk[0].hash));
    let plain_rebuild = db.recipes_for_output(plain_id).unwrap()[1].recipe_id;
    assert!(
        db.rebuild_inputs(plain_rebuild)
            .unwrap()
            .iter()
            .any(|i| i.generated && i.hash == junk[0].hash),
        "the plaintext's junk is a range of the same stream"
    );
}

#[test]
fn a_missing_key_defers_and_the_keys_arrival_resumes() {
    let disc = synth::disc(*b"RTSTD1", 0, &files(), Pad::Junk);
    let (_dir, store, mut db) = world();
    let img_hash = put(&store, &db, &disc.bytes);
    let mut analyzer = analyzer();

    let report = sweep(&mut db, &store, &mut analyzer);
    assert_eq!(report.errors.len(), 0, "{:?}", report.errors);
    assert_eq!((report.analyzed, report.deferred), (0, 1));
    assert!(analysis_details(&db).is_empty(), "no conclusion recorded");
    assert_eq!(
        db.sweep_queue_len(&analyzer.id()).unwrap(),
        0,
        "left the queue"
    );
    let waiting = db.deferred_sweep_items(&analyzer.id()).unwrap();
    assert_eq!(waiting.len(), 1);
    assert_eq!(waiting[0].1, Blake3::compute(&test_common_key()));
    // Another sweep changes nothing: the item is neither queued nor
    // retried while the key is absent.
    let report = sweep(&mut db, &store, &mut analyzer);
    assert_eq!(
        (report.enqueued, report.analyzed, report.deferred),
        (0, 0, 0)
    );

    // The key arrives (an ordinary ingest of 16 bytes) and the next
    // sweep picks the disc back up.
    put(&store, &db, &disc.common_key);
    let report = sweep(&mut db, &store, &mut analyzer);
    assert_eq!(
        (report.positive, report.deferred),
        (1, 0),
        "{:?}",
        report.errors
    );
    assert!(db.deferred_sweep_items(&analyzer.id()).unwrap().is_empty());
    let exec = Executor::new(&store, ExecConfig::default()).expect("executor");
    assert_eq!(stream(&exec, &db, &img_hash), disc.bytes);
}

#[test]
fn a_tampered_body_stays_an_opaque_piece() {
    let mut disc = synth::disc(*b"RTSTD1", 0, &files(), Pad::Junk);
    // Flip one byte of one sector's hash block: the plaintext decrypts
    // but the block no longer equals what the data yields.
    let at = usize::try_from(disc.body_offset).unwrap() + 0x8000 * 3 + 0x100;
    disc.bytes[at] ^= 0x55;
    let (_dir, store, mut db) = world();
    put(&store, &db, &disc.common_key);
    let img_hash = put(&store, &db, &disc.bytes);

    let report = sweep(&mut db, &store, &mut analyzer());
    assert_eq!(report.positive, 1, "{:?}", analysis_details(&db));
    let details = analysis_details(&db);
    let detail = details
        .iter()
        .find(|d| d.contains("split into"))
        .expect("verdict");
    assert!(detail.contains("0 walked"), "{detail}");
    assert!(
        detail.contains("opaque: part0-data: hash block of sector 3"),
        "{detail}"
    );
    // The disc still decomposes around the body, which is a piece with
    // its slice route only.
    let body_hash = Blake3::compute(
        &disc.bytes[usize::try_from(disc.body_offset).unwrap()..][..disc.body.len()],
    );
    assert_eq!(routes(&db, &body_hash), vec!["assemble@1".to_owned()]);
    let exec = Executor::new(&store, ExecConfig::default()).expect("executor");
    assert_eq!(stream(&exec, &db, &img_hash), disc.bytes);
}

#[test]
fn non_wii_bytes_and_unknown_keys_are_negative() {
    let (_dir, store, mut db) = world();
    put(&store, &db, &pattern(400_000, 99));
    let gc = datboi_ingest::gcm::synth::image(*b"GALE01", 0, &files(), Pad::Zero);
    put(&store, &db, &gc.bytes);
    let mut unknown = synth::disc(*b"RTSTD1", 0, &files(), Pad::Zero);
    let p = usize::try_from(unknown.partition_offset).unwrap();
    unknown.bytes[p + 0x1F1] = 7; // common key index nobody has
    put(&store, &db, &unknown.bytes);
    let mut scrubbed = synth::disc(*b"RTSTD1", 0, &files(), Pad::Zero);
    scrubbed.bytes[0x60] = 1;
    put(&store, &db, &scrubbed.bytes);

    let report = sweep(&mut db, &store, &mut analyzer());
    assert_eq!(report.errors.len(), 0, "{:?}", report.errors);
    assert_eq!((report.negative, report.deferred), (4, 0));
    let details = analysis_details(&db);
    for needle in [
        "not a wii disc",
        "a gamecube disc",
        "not a key this build knows",
        "declares no partition hashes",
    ] {
        assert!(
            details.iter().any(|d| d.contains(needle)),
            "{needle}: {details:?}"
        );
    }
}

/// Real-disc walk, opt-in: `DATBOI_WII_IMAGE=/path/to.iso
/// DATBOI_WII_KEY=/path/to/common-key.bin cargo test --release -p
/// datboi-ingest --test wii real_image_from_env -- --ignored
/// --nocapture`. Prints the verdict and per-partition facts.
#[test]
#[ignore]
fn real_image_from_env() {
    let (Ok(path), Ok(key_path)) = (
        std::env::var("DATBOI_WII_IMAGE"),
        std::env::var("DATBOI_WII_KEY"),
    ) else {
        eprintln!("DATBOI_WII_IMAGE / DATBOI_WII_KEY unset");
        return;
    };
    let t0 = std::time::Instant::now();
    let mut file = std::fs::File::open(&path).expect("open image");
    let header = wii::read_disc_header(&mut file).expect("wii disc");
    let parts = wii::read_partitions(&mut file).expect("partitions");
    let key: [u8; 16] = std::fs::read(&key_path)
        .expect("key file")
        .as_slice()
        .try_into()
        .expect("16-byte key");
    let known = wii::known_keys();
    let keys: Vec<[u8; 16]> = parts
        .iter()
        .map(|p| {
            let k = wii::key_for(&known, &p.ticket).expect("known issuer/index");
            assert_eq!(
                k.hash,
                Blake3::compute(&key),
                "the supplied key is the one partition {} names",
                p.index
            );
            key
        })
        .collect();
    for p in &parts {
        eprintln!(
            "partition {} kind {} at 0x{:x}: body 0x{:x}+{} ({} sectors), tmd {} B, certs {} B, key {}/{}",
            p.index,
            p.kind,
            p.offset,
            p.data_offset,
            p.data_len,
            p.sectors(),
            p.tmd_len,
            p.cert_len,
            p.ticket.issuer,
            p.ticket.common_key_index
        );
    }
    let layout = wii::parse_layout(&mut file, 4096, header, &parts, &keys).expect("layout");
    eprintln!(
        "walked in {:.1?}: {} disc pieces; disc-level junk {} B, fill {} B, residue {} B; junk stream {}",
        t0.elapsed(),
        layout.pieces.len(),
        layout.junk_bytes,
        layout.fill_bytes,
        layout.residual_bytes,
        layout
            .junk
            .map_or("none".to_owned(), |j| j.hash.to_string())
    );
    for p in &layout.partitions {
        match &p.plain {
            Some(l) => eprintln!(
                "  {}: verified; {} pieces ({} files, {} dirs), junk {} B ({:.1}% of {} B plaintext), fill {} B, residue {} B{}",
                p.header.prefix(),
                l.pieces.len(),
                l.file_count,
                l.dir_count,
                l.junk_bytes,
                l.junk_bytes as f64 * 100.0 / l.total_len.max(1) as f64,
                l.total_len,
                l.fill_bytes,
                l.residual_bytes,
                if l.coalesced { " (coalesced)" } else { "" }
            ),
            None => eprintln!(
                "  {}: OPAQUE — {}",
                p.header.prefix(),
                p.opaque_reason.as_deref().unwrap_or("?")
            ),
        }
    }
    for p in &layout.partitions {
        if let Some(j) = &p.junk {
            eprintln!(
                "  {}: own junk stream {} ({:?} disc {}, {} B)",
                p.header.prefix(),
                j.hash,
                String::from_utf8_lossy(&j.id),
                j.disc,
                j.len
            );
        }
    }
    let total = layout.total_junk_bytes();
    eprintln!(
        "junk total {} B = {:.1}% of the {} B image",
        total,
        total as f64 * 100.0 / layout.total_len as f64,
        layout.total_len
    );
}
