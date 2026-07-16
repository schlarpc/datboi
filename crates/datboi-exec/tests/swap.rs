//! The D91 arc, end to end: two NDS variants sharing most NitroFS
//! pieces get decomposed (nds-split claims), the swap predicate trips,
//! their absent pieces materialize into sealed packs, the rebuild
//! routes license, the container literals evict — and both ROMs still
//! serve byte-exact through assemble-over-packed-pieces. A lone
//! variant with nothing shared never trips the predicate (never
//! eager), and packed pieces refuse eviction (grounding leaves).

use std::io::Read as _;

use datboi_core::hash::Blake3;
use datboi_exec::evict::{Blocked, EvictOutcome};
use datboi_exec::{ExecConfig, Executor};
use datboi_index::{Db, Namespace as IndexNs, Residency};
use datboi_ingest::analyzers::NdsAnalyzer;
use datboi_ingest::nds::crc16;
use datboi_ingest::refine::{Logical, run_sweep};
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
    let id = db
        .upsert_blob(
            &hash,
            Some(bytes.len() as u64),
            IndexNs::Data,
            Residency::Resident,
        )
        .expect("row");
    db.set_verified(id, 1).expect("verified");
    hash
}

fn pattern(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
        .collect()
}

/// A NitroFS ROM with four root files; `title` differentiates the
/// header (a literal segment in the rebuild — never a piece), `f4`
/// carries the variant-unique content, and `family` seeds everything
/// else — two ROMs share pieces exactly when they share a family (the
/// MKDS shape in miniature).
fn variant_nds(title: &[u8], family: u8, f4: &[u8]) -> Vec<u8> {
    let files: Vec<Vec<u8>> = vec![
        pattern(300, family.wrapping_add(1)),
        pattern(300, family.wrapping_add(2)),
        pattern(300, family.wrapping_add(3)),
        f4.to_vec(),
    ];
    let mut b = vec![0u8; 0x200];
    b[..title.len().min(12)].copy_from_slice(&title[..title.len().min(12)]);
    for (i, x) in b[0xC0..0x15C].iter_mut().enumerate() {
        *x = (i as u8).wrapping_mul(7).wrapping_add(3);
    }
    b[0x12] = 0x00;

    let put_section = |b: &mut Vec<u8>, data: &[u8]| -> (u32, u32) {
        let start = u32::try_from(b.len()).expect("small rom");
        b.extend_from_slice(data);
        (start, u32::try_from(data.len()).expect("small rom"))
    };
    let arm9 = put_section(&mut b, &pattern(0x300, family.wrapping_add(9)));
    let mut fnt = Vec::new();
    fnt.extend_from_slice(&8u32.to_le_bytes());
    fnt.extend_from_slice(&0u16.to_le_bytes());
    fnt.extend_from_slice(&1u16.to_le_bytes());
    for stem in [1u8, 2, 3, 4] {
        let name = format!("f{}_{:02}.bin", stem, family);
        fnt.push(u8::try_from(name.len()).expect("short name"));
        fnt.extend_from_slice(name.as_bytes());
    }
    fnt.push(0);
    let fnt_pos = put_section(&mut b, &fnt);
    let mut fat = Vec::new();
    for file in &files {
        let pos = put_section(&mut b, file);
        fat.extend_from_slice(&pos.0.to_le_bytes());
        fat.extend_from_slice(&(pos.0 + pos.1).to_le_bytes());
    }
    let fat_pos = put_section(&mut b, &fat);

    let set = |b: &mut Vec<u8>, at: usize, v: u32| b[at..at + 4].copy_from_slice(&v.to_le_bytes());
    set(&mut b, 0x20, arm9.0);
    set(&mut b, 0x2C, arm9.1);
    set(&mut b, 0x40, fnt_pos.0);
    set(&mut b, 0x44, fnt_pos.1);
    set(&mut b, 0x48, fat_pos.0);
    set(&mut b, 0x4C, fat_pos.1);
    let total = u32::try_from(b.len()).expect("small rom");
    set(&mut b, 0x80, total);

    let logo = crc16(&b[0xC0..0x15C]);
    b[0x15C..0x15E].copy_from_slice(&logo.to_le_bytes());
    let header = crc16(&b[..0x15E]);
    b[0x15E..0x160].copy_from_slice(&header.to_le_bytes());
    b
}

fn read_back(exec: &Executor, db: &Db, hash: &Blake3) -> Vec<u8> {
    let mut out = Vec::new();
    exec.open_stream(db, hash)
        .expect("route")
        .read_to_end(&mut out)
        .expect("read");
    out
}

#[test]
fn variants_swap_into_packs_and_serve_byte_exact() {
    let (dir, store, mut db) = world();

    // Variants USA/EUR share arm9 + fnt + fat + f1..f3 (~79% of piece
    // bytes); f4 differs. The loner shares nothing with anyone.
    let usa = variant_nds(b"MKDS USA", 0, &pattern(600, 100));
    let eur = variant_nds(b"MKDS EUR", 0, &pattern(600, 101));
    let lone = variant_nds(b"LONER", 50, &pattern(600, 102));
    let usa_hash = put(&store, &db, &usa);
    let eur_hash = put(&store, &db, &eur);
    let lone_hash = put(&store, &db, &lone);

    // Decompose all three (claims only — pieces stay absent).
    let exec = Executor::new(&store, ExecConfig::default()).expect("executor");
    let bytes = Logical::new(&store, &exec);
    let sweep = run_sweep(&mut db, &store, &bytes, &mut NdsAnalyzer, 100).expect("sweep");
    assert_eq!(sweep.positive, 3, "{:?}", sweep.errors);
    let shared_piece = Blake3::compute(&pattern(300, 1)); // family 0, f1
    assert_eq!(
        db.blob_by_hash(&shared_piece)
            .expect("q")
            .expect("claimed")
            .residency,
        Residency::Absent,
        "decomposition mints claims, not bytes"
    );

    // The swap: USA and EUR trip the predicate, the loner never does.
    let report = exec.swap_covered(&mut db).expect("swap");
    assert_eq!(
        (report.swapped, report.packs),
        (2, 2),
        "skipped: {:?}",
        report.skipped
    );
    assert!(report.below_threshold >= 1, "the loner stays whole");
    assert!(report.bytes_reclaimed > 0);

    // Containers: variants evicted (covered), loner untouched.
    for (hash, want) in [
        (usa_hash, Residency::EvictedCovered),
        (eur_hash, Residency::EvictedCovered),
        (lone_hash, Residency::Resident),
    ] {
        let row = db.blob_by_hash(&hash).expect("q").expect("row");
        assert_eq!(row.residency, want, "{hash}");
    }
    assert!(!store.has_loose(StoreNs::Data, &usa_hash));
    assert!(store.has_loose(StoreNs::Data, &lone_hash));
    assert_eq!(store.list_packs().len(), 2);

    // Shared pieces live once, in USA's pack (first swap wins); EUR's
    // pack holds only its unique piece. Both resolve transparently.
    let piece_row = db.blob_by_hash(&shared_piece).expect("q").expect("row");
    assert_eq!(piece_row.residency, Residency::Resident);
    assert!(store.is_packed(&shared_piece));

    // The whole point: both ROMs serve byte-exact from packed pieces.
    assert_eq!(read_back(&exec, &db, &usa_hash), usa);
    assert_eq!(read_back(&exec, &db, &eur_hash), eur);

    // Range serving works too (D49: the container's outboard was
    // built before its bytes dropped).
    let range = exec.serve_range(&db, &usa_hash, 0x200, 64).expect("range");
    assert_eq!(range, usa[0x200..0x200 + 64]);

    // Packed pieces are grounding leaves: eviction refuses them
    // explicitly (never a half-evict that strands index truth).
    match exec.evict(&db, &shared_piece).expect("evict call") {
        EvictOutcome::Blocked(Blocked::Packed) => {}
        other => panic!("packed piece must refuse eviction, got {other:?}"),
    }

    // Idempotence: a second phase finds nothing to do.
    let again = exec.swap_covered(&mut db).expect("swap again");
    assert_eq!(
        (again.swapped, again.packs),
        (0, 0),
        "skipped: {:?}",
        again.skipped
    );

    // The recovery half that lives in the store: a FRESH open rescans
    // pack footers (no database, no memory of this process) and both
    // resolution and serving come back — footers are the truth (D15).
    drop(exec);
    drop(store);
    let store = Store::open(dir.path().join("store")).expect("reopen");
    let exec = Executor::new(&store, ExecConfig::default()).expect("executor");
    assert!(store.is_packed(&shared_piece), "footer rescan resolves");
    assert!(
        store
            .list_packed()
            .iter()
            .any(|(hash, _)| *hash == shared_piece),
        "recovery scan surfaces packed members"
    );
    assert_eq!(read_back(&exec, &db, &usa_hash), usa, "serves after reopen");
}
