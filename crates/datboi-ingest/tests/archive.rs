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

    // D110: each member carries a DERIVE RECIPE (container→member
    // through the ex-7z component) — evictable + rebuildable, exactly
    // the rar shape.
    let member_hash = Blake3::compute(&rom_a);
    let row = db.blob_by_hash(&member_hash).expect("q").expect("indexed");
    let recipes = db.recipes_for_output(row.blob_id).expect("q");
    assert_eq!(
        recipes.len(),
        1,
        "one covering derive recipe (notes: {:?})",
        report.notes
    );

    let exec =
        datboi_exec::Executor::new(&store, datboi_exec::ExecConfig::default()).expect("executor");
    exec.replay(&db, recipes[0].recipe_id).expect("replay");
    let outcome = exec.evict(&db, &member_hash).expect("evict");
    assert!(
        matches!(outcome, datboi_exec::evict::EvictOutcome::Evicted { .. }),
        "member evicted: {outcome:?}"
    );
    let mut rebuilt = Vec::new();
    exec.open_stream(&db, &member_hash)
        .expect("route")
        .read_to_end(&mut rebuilt)
        .expect("read");
    assert_eq!(&rebuilt, &rom_a, "member rebuilds bit-exact through ex-7z");

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

    // The member landed resident and byte-exact (the fixture's one member
    // "VERSION" holds "unrar-0.4.0").
    let member = b"unrar-0.4.0";
    let member_hash = Blake3::compute(member);
    let mut got = Vec::new();
    store
        .get(StoreNs::Data, &member_hash)
        .expect("get")
        .expect("member resident")
        .read_to_end(&mut got)
        .expect("read");
    assert_eq!(&got, member, "member bytes bit-exact");

    // D58: the member carries a DERIVE RECIPE (container→member through the
    // ex-unrar component) and is therefore evictable + rebuildable.
    let row = db.blob_by_hash(&member_hash).expect("q").expect("indexed");
    let recipes = db.recipes_for_output(row.blob_id).expect("q");
    assert_eq!(recipes.len(), 1, "one covering derive recipe");
    let recipe_id = recipes[0].recipe_id;

    let exec =
        datboi_exec::Executor::new(&store, datboi_exec::ExecConfig::default()).expect("executor");

    // Replay licenses the recipe (D25): running the ex-unrar component
    // rebuilds the member bit-exact and verifies its hash.
    exec.replay(&db, recipe_id).expect("replay");

    // Now the literal is droppable; evict then rebuild it through the
    // component — bit-exact = the derive route is real.
    let outcome = exec.evict(&db, &member_hash).expect("evict");
    assert!(
        matches!(outcome, datboi_exec::evict::EvictOutcome::Evicted { .. }),
        "member evicted: {outcome:?}"
    );
    assert!(
        !store.has(StoreNs::Data, &member_hash),
        "member literal gone"
    );

    let mut rebuilt = Vec::new();
    exec.open_stream(&db, &member_hash)
        .expect("route")
        .read_to_end(&mut rebuilt)
        .expect("read");
    assert_eq!(
        &rebuilt, member,
        "member rebuilds bit-exact through ex-unrar"
    );
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
