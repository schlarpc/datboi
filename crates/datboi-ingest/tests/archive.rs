//! 7z / rar ingest: members extracted into the CAS as first-class
//! resident, alias-indexed blobs; the container stays a literal (no
//! LZMA-class rebuild transform exists yet — see archive module docs).

use std::io::Read;

use datboi_core::hash::Blake3;
use datboi_index::Db;
use datboi_ingest::Ingester;
use datboi_store_fs::{Namespace as StoreNs, Store};

fn world() -> (tempfile::TempDir, Store, Db) {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Store::open(dir.path().join("store")).expect("store");
    let db = Db::open(dir.path()).expect("db");
    (dir, store, db)
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
fn seven_zip_members_become_resident_alias_indexed_blobs() {
    let (dir, store, mut db) = world();

    let rom_a = pattern(300_000, 0xAAAA_BBBB_CCCC_DDDD);
    let rom_b = b"tiny member".to_vec();
    let archive_path = dir.path().join("set.7z");
    let mut writer = sevenz_rust2::ArchiveWriter::create(&archive_path).expect("writer");
    writer
        .push_archive_entry(
            sevenz_rust2::ArchiveEntry::new_file("a.rom"),
            Some(rom_a.as_slice()),
        )
        .expect("entry a");
    writer
        .push_archive_entry(
            sevenz_rust2::ArchiveEntry::new_file("b.rom"),
            Some(rom_b.as_slice()),
        )
        .expect("entry b");
    writer.finish().expect("finish");

    let report = Ingester::new(&store, &mut db, &[]).ingest(&[&archive_path]);
    assert_eq!(report.errors.len(), 0, "{:?}", report.errors);
    assert_eq!(report.files_stored, 1, "container stored");
    assert_eq!(report.members_extracted, 2, "both members extracted");

    for rom in [&rom_a, &rom_b] {
        let hash = Blake3::compute(rom);
        let mut streamed = Vec::new();
        store
            .get(StoreNs::Data, &hash)
            .expect("get")
            .expect("member resident")
            .read_to_end(&mut streamed)
            .expect("read");
        assert_eq!(&streamed, rom, "member bytes bit-exact");
        let row = db.blob_by_hash(&hash).expect("q").expect("indexed");
        assert_eq!(row.residency, datboi_index::Residency::Resident);
    }

    // Re-ingest is a rescan-cache hit: nothing re-extracted.
    let again = Ingester::new(&store, &mut db, &[]).ingest(&[&archive_path]);
    assert_eq!(again.files_unchanged, 1);
    assert_eq!(again.members_extracted, 0);
}

#[test]
fn rar_members_become_resident_blobs() {
    let (dir, store, mut db) = world();
    // Committed fixture from the unrar crate's test data: one member,
    // "VERSION". rar cannot be created programmatically (extraction-only
    // by license), hence the committed artifact.
    let rar = include_bytes!("fixtures/version.rar");
    let path = dir.path().join("version.rar");
    std::fs::write(&path, rar).expect("write fixture");

    let report = Ingester::new(&store, &mut db, &[]).ingest(&[&path]);
    assert_eq!(report.errors.len(), 0, "{:?}", report.errors);
    assert_eq!(report.files_stored, 1);
    assert_eq!(report.members_extracted, 1, "VERSION extracted");

    // The container itself is stored and literal.
    let container_hash = Blake3::compute(rar);
    assert!(store.has(StoreNs::Data, &container_hash));
}

#[test]
fn truncated_7z_is_an_error_not_a_panic() {
    let (dir, store, mut db) = world();
    let mut bytes = vec![0x37, 0x7A, 0xBC, 0xAF, 0x27, 0x1C];
    bytes.extend_from_slice(&[0u8; 40]);
    let path = dir.path().join("broken.7z");
    std::fs::write(&path, &bytes).expect("write");

    let report = Ingester::new(&store, &mut db, &[]).ingest(&[&path]);
    assert_eq!(report.files_stored, 1, "container still stored");
    assert_eq!(report.members_extracted, 0);
    assert_eq!(report.errors.len(), 1, "extraction failure reported");
}
