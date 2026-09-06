//! The D116 leaf walk one level down on DS (D94): two regional ROM
//! variants whose only difference is ONE member inside a NARC. Before
//! D116 the swap packed the NARC whole (a direct input of the ROM's
//! rebuild) — two NARCs, the shared members twice. Now the NARC is an
//! intermediate (its own assemble over its members is a downward
//! route): the swap packs the MEMBERS, licenses the NARC's rebuild
//! verify-only, and the second variant's pack holds only the member
//! that differs. Both ROMs rebuild and serve ranges byte-exact through
//! the two-level assemble.

use std::io::Read as _;

use datboi_core::hash::Blake3;
use datboi_exec::{ExecConfig, Executor};
use datboi_index::{AbsentMode, Db, Namespace as IndexNs, Residency};
use datboi_ingest::analyzers::{NarcAnalyzer, NdsAnalyzer};
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
    db.upsert_blob(
        &hash,
        Some(bytes.len() as u64),
        IndexNs::Data,
        Residency::Resident,
    )
    .expect("row");
    hash
}

fn pattern(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
        .collect()
}

/// The narc test's synthetic archive: header + BTAF + minimal BTNF +
/// GMIF, members 0xFF-padded to four bytes.
fn build_narc(members: &[&[u8]]) -> Vec<u8> {
    let mut fat: Vec<(u32, u32)> = Vec::new();
    let mut data: Vec<u8> = Vec::new();
    for m in members {
        let start = data.len() as u32;
        data.extend_from_slice(m);
        fat.push((start, data.len() as u32));
        while !data.len().is_multiple_of(4) {
            data.push(0xFF);
        }
    }
    let mut btaf = Vec::new();
    btaf.extend_from_slice(b"BTAF");
    btaf.extend_from_slice(&((12 + fat.len() * 8) as u32).to_le_bytes());
    btaf.extend_from_slice(&(members.len() as u16).to_le_bytes());
    btaf.extend_from_slice(&0u16.to_le_bytes());
    for (s, e) in &fat {
        btaf.extend_from_slice(&s.to_le_bytes());
        btaf.extend_from_slice(&e.to_le_bytes());
    }
    let mut btnf = Vec::new();
    btnf.extend_from_slice(b"BTNF");
    btnf.extend_from_slice(&16u32.to_le_bytes());
    btnf.extend_from_slice(&4u32.to_le_bytes());
    btnf.extend_from_slice(&0u16.to_le_bytes());
    btnf.extend_from_slice(&1u16.to_le_bytes());
    let mut gmif = Vec::new();
    gmif.extend_from_slice(b"GMIF");
    gmif.extend_from_slice(&((8 + data.len()) as u32).to_le_bytes());
    gmif.extend_from_slice(&data);
    let total = (0x10 + btaf.len() + btnf.len() + gmif.len()) as u32;
    let mut narc = Vec::new();
    narc.extend_from_slice(b"NARC");
    narc.extend_from_slice(&0xFFFEu16.to_le_bytes());
    narc.extend_from_slice(&0x0100u16.to_le_bytes());
    narc.extend_from_slice(&total.to_le_bytes());
    narc.extend_from_slice(&0x10u16.to_le_bytes());
    narc.extend_from_slice(&3u16.to_le_bytes());
    narc.extend_from_slice(&btaf);
    narc.extend_from_slice(&btnf);
    narc.extend_from_slice(&gmif);
    narc
}

/// The swap test's NitroFS ROM: three family-shared files and `f4`.
fn variant_nds(title: &[u8], family: u8, f4: &[u8], shared_len: usize) -> Vec<u8> {
    let files: Vec<Vec<u8>> = vec![
        pattern(shared_len, family.wrapping_add(1)),
        pattern(shared_len, family.wrapping_add(2)),
        pattern(shared_len, family.wrapping_add(3)),
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

fn residency(db: &Db, hash: &Blake3) -> Residency {
    db.blob_by_hash(hash).expect("q").expect("row").residency
}

#[test]
fn narc_members_are_the_leaves_and_variants_share_them() {
    let (_dir, store, mut db) = world();
    db.config_set("swap:reclaim-min-bytes", b"1024")
        .expect("policy");
    // No dats here: the molten `all` mode lets the NARC analyzer reach
    // the absent NARC pieces (D92).
    db.set_absent_mode(Some(AbsentMode::All)).expect("config");

    // Two variants: identical outside the NARC, and inside it two
    // shared members plus one localized member.
    let m1 = pattern(5000, 11);
    let m2 = pattern(7000, 12);
    let usa_text = pattern(3000, 13);
    let eur_text = pattern(3000, 14);
    let narc_usa = build_narc(&[&m1, &m2, &usa_text]);
    let narc_eur = build_narc(&[&m1, &m2, &eur_text]);
    let usa = variant_nds(b"GAME USA", 0, &narc_usa, 4000);
    let eur = variant_nds(b"GAME EUR", 0, &narc_eur, 4000);
    let usa_hash = put(&store, &db, &usa);
    let eur_hash = put(&store, &db, &eur);
    let narc_usa_hash = Blake3::compute(&narc_usa);
    let narc_eur_hash = Blake3::compute(&narc_eur);

    let exec = Executor::new(&store, ExecConfig::default()).expect("executor");
    let bytes = Logical::new(&store, &exec);
    let nds = run_sweep(&mut db, &store, &bytes, &mut NdsAnalyzer, 100).expect("nds sweep");
    assert_eq!(nds.positive, 2, "{:?}", nds.errors);
    let narc = run_sweep(&mut db, &store, &bytes, &mut NarcAnalyzer, 100).expect("narc sweep");
    assert_eq!(narc.positive, 2, "both NARCs split: {:?}", narc.errors);
    assert_eq!(residency(&db, &narc_usa_hash), Residency::Absent);

    // The swap: both ROMs qualify; the NARCs are intermediates.
    let report = exec.swap_covered(&mut db).expect("swap");
    assert_eq!((report.swapped, report.packs), (2, 2), "{report:?}");
    for narc_hash in [&narc_usa_hash, &narc_eur_hash] {
        assert_eq!(
            residency(&db, narc_hash),
            Residency::Absent,
            "NARC never packed"
        );
        assert!(!store.has(StoreNs::Data, narc_hash));
        // Its rebuild from members was licensed verify-only.
        let id = db.get_blob_id(narc_hash).unwrap().unwrap();
        assert!(
            db.recipes_for_output(id)
                .unwrap()
                .iter()
                .any(|r| r.verify == datboi_index::VerifyState::ReplayedLocal),
            "NARC rebuild licensed"
        );
    }
    for member in [&m1, &m2, &usa_text, &eur_text] {
        let h = Blake3::compute(member);
        assert_eq!(residency(&db, &h), Residency::Resident, "member packed");
        assert!(store.is_packed(&h));
    }
    // What the pair costs: the ROM's other pieces once, every member
    // once, and not even one NARC on top — the old behaviour packed
    // both NARCs whole, m1 + m2 twice over.
    let members_once: u64 = [&m1, &m2, &usa_text, &eur_text]
        .iter()
        .map(|m| m.len() as u64)
        .sum();
    assert!(
        report.bytes_packed < members_once + narc_usa.len() as u64,
        "packed {} — a NARC went into a pack: {report:?}",
        report.bytes_packed
    );
    assert!(report.bytes_packed >= members_once, "{report:?}");

    // Both ROMs rebuild byte-exact through the two-level assemble, and
    // a range inside the NARC serves through it.
    for (hash, rom) in [(&usa_hash, &usa), (&eur_hash, &eur)] {
        assert!(!store.has_loose(StoreNs::Data, hash), "ROM evicted");
        let mut out = Vec::new();
        exec.open_stream(&db, hash)
            .expect("route")
            .read_to_end(&mut out)
            .expect("stream");
        assert_eq!(&out, rom);
        let at = rom.len() as u64 - narc_usa.len() as u64 / 2;
        let got = exec.serve_range(&db, hash, at, 1000).expect("range");
        assert_eq!(got, rom[usize::try_from(at).unwrap()..][..1000]);
    }
    let again = exec.swap_covered(&mut db).expect("swap again");
    assert_eq!((again.swapped, again.packs), (0, 0), "{again:?}");
}
