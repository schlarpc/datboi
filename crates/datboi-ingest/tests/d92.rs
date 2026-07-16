//! D92 end-to-end: the refinement fixpoint advances over
//! claimed-but-absent identities — the exact stalls that motivated the
//! ruling. An .nds STORED inside a zip is claimed at ingest and never
//! materialized by anything; before D92 it was permanently
//! un-analyzed (preflate only materializes DEFLATE plaintext, and a
//! stored member lives in the skeleton). Now nds-split reads it
//! through the executor and the NitroFS claims appear — with the
//! member's residency untouched (a spill is not a residency flip).
//! The DEFLATE twin covers the preflate-refused-container stall: the
//! member claim's decompress route serves analysis with no preflate
//! pass anywhere in sight.

use std::io::Write as _;

use datboi_core::hash::Blake3;
use datboi_index::{AbsentMode, AnalysisOutcome, Db, Residency};
use datboi_ingest::Ingester;
use datboi_ingest::analyzers::NdsAnalyzer;
use datboi_ingest::nds::crc16;
use datboi_ingest::refine::{Analyzer as _, Logical, run_sweep};
use datboi_store_fs::Store;
use flate2::Compression;
use flate2::write::DeflateEncoder;

fn world() -> (tempfile::TempDir, Store, Db) {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Store::open(dir.path().join("store")).expect("store");
    let db = Db::open(dir.path()).expect("db");
    (dir, store, db)
}

fn pattern(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
        .collect()
}

/// Minimal NitroFS-shaped ROM: header, ARM9, FNT naming one file, FAT,
/// one file body — dense (zero residue), already-trimmed shape. `seed`
/// varies content so two ROMs never dedupe to one blob.
fn tiny_nds(seed: u8) -> Vec<u8> {
    let mut b = vec![0u8; 0x200];
    b[..4].copy_from_slice(b"D92!");
    for (i, x) in b[0xC0..0x15C].iter_mut().enumerate() {
        *x = (i as u8).wrapping_mul(7).wrapping_add(seed);
    }
    b[0x12] = 0x00; // unitcode: plain NTR

    let put = |b: &mut Vec<u8>, data: &[u8]| -> (u32, u32) {
        let start = u32::try_from(b.len()).expect("small rom");
        b.extend_from_slice(data);
        (start, u32::try_from(data.len()).expect("small rom"))
    };
    let arm9 = put(&mut b, &pattern(0x300, seed.wrapping_add(9)));
    // FNT: one directory, one file "f.bin" with id 0.
    let mut fnt = Vec::new();
    fnt.extend_from_slice(&8u32.to_le_bytes()); // root sub-table offset
    fnt.extend_from_slice(&0u16.to_le_bytes()); // first file id
    fnt.extend_from_slice(&1u16.to_le_bytes()); // total directories
    fnt.extend_from_slice(&[0x05]);
    fnt.extend_from_slice(b"f.bin");
    fnt.push(0);
    let fnt_pos = put(&mut b, &fnt);
    let file_pos = put(&mut b, &pattern(300, seed.wrapping_add(41)));
    let mut fat = Vec::new();
    fat.extend_from_slice(&file_pos.0.to_le_bytes());
    fat.extend_from_slice(&(file_pos.0 + file_pos.1).to_le_bytes());
    let fat_pos = put(&mut b, &fat);

    let set = |b: &mut Vec<u8>, at: usize, v: u32| b[at..at + 4].copy_from_slice(&v.to_le_bytes());
    set(&mut b, 0x20, arm9.0);
    set(&mut b, 0x2C, arm9.1);
    set(&mut b, 0x40, fnt_pos.0);
    set(&mut b, 0x44, fnt_pos.1);
    set(&mut b, 0x48, fat_pos.0);
    set(&mut b, 0x4C, fat_pos.1);
    let total = u32::try_from(b.len()).expect("small rom");
    set(&mut b, 0x80, total); // declared NTR size = whole rom (no trim)

    let logo = crc16(&b[0xC0..0x15C]);
    b[0x15C..0x15E].copy_from_slice(&logo.to_le_bytes());
    let header = crc16(&b[..0x15E]);
    b[0x15E..0x160].copy_from_slice(&header.to_le_bytes());
    b
}

/// One-member zip; `deflate` picks the compression method.
fn zip_one_member(payload: &[u8], deflate: bool) -> Vec<u8> {
    let crc = {
        let mut h = crc32fast::Hasher::new();
        h.update(payload);
        h.finalize()
    };
    let data = if deflate {
        let mut enc = DeflateEncoder::new(Vec::new(), Compression::new(6));
        enc.write_all(payload).expect("deflate");
        enc.finish().expect("finish")
    } else {
        payload.to_vec()
    };
    let method: u16 = if deflate { 8 } else { 0 };
    let name = b"game.nds";
    let sizes = |out: &mut Vec<u8>| {
        out.extend_from_slice(&crc.to_le_bytes());
        out.extend_from_slice(&u32::try_from(data.len()).expect("small").to_le_bytes());
        out.extend_from_slice(&u32::try_from(payload.len()).expect("small").to_le_bytes());
        out.extend_from_slice(&u16::try_from(name.len()).expect("small").to_le_bytes());
    };
    let mut out = Vec::new();
    out.extend_from_slice(b"PK\x03\x04");
    out.extend_from_slice(&20u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&method.to_le_bytes());
    out.extend_from_slice(&[0; 4]);
    sizes(&mut out);
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(name);
    out.extend_from_slice(&data);
    let cd_offset = out.len();
    out.extend_from_slice(b"PK\x01\x02");
    out.extend_from_slice(&20u16.to_le_bytes());
    out.extend_from_slice(&20u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&method.to_le_bytes());
    out.extend_from_slice(&[0; 4]);
    sizes(&mut out);
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
fn absent_zip_members_get_nds_split_through_the_executor() {
    let (dir, store, mut db) = world();

    // Two zips: the same stall in both method shapes. STORED is the
    // member preflate can never materialize; DEFLATE is the member of
    // a container whose preflate pass never ran (or refused).
    let stored_nds = tiny_nds(3);
    let deflated_nds = tiny_nds(200);
    for (name, bytes) in [
        ("stored.zip", zip_one_member(&stored_nds, false)),
        ("deflated.zip", zip_one_member(&deflated_nds, true)),
    ] {
        let path = dir.path().join(name);
        std::fs::write(&path, &bytes).expect("write zip");
        let report = Ingester::new(&store, &mut db, &[]).ingest(&[&path]);
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert_eq!(report.members_claimed, 1, "{name}: member claimed");
    }
    for hash in [Blake3::compute(&stored_nds), Blake3::compute(&deflated_nds)] {
        let row = db.blob_by_hash(&hash).expect("q").expect("member row");
        assert_eq!(row.residency, Residency::Absent, "claimed, not stored");
    }

    // No dats in this world: use the molten `all` mode (the dat-named
    // default's gating is pinned by the index tests).
    db.set_absent_mode(Some(AbsentMode::All)).expect("config");

    let exec =
        datboi_exec::Executor::new(&store, datboi_exec::ExecConfig::default()).expect("executor");
    let bytes = Logical::new(&store, &exec);
    let report = run_sweep(&mut db, &store, &bytes, &mut NdsAnalyzer, 100).expect("sweep");
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    assert_eq!(
        report.positive, 2,
        "both absent members split — the D92 point"
    );

    for (rom, seed) in [(&stored_nds, 3u8), (&deflated_nds, 200u8)] {
        let hash = Blake3::compute(rom);
        let row = db.blob_by_hash(&hash).expect("q").expect("member row");
        assert_eq!(
            db.analysis_outcome(row.blob_id, &NdsAnalyzer.id())
                .expect("q"),
            Some(AnalysisOutcome::Positive),
            "provenance for the absent member"
        );
        assert_eq!(
            row.residency,
            Residency::Absent,
            "analysis spilled, never materialized — a residency flip is the planner's call"
        );
        // The member now has TWO routes: the ingest-time zip claim and
        // the freshly minted NitroFS rebuild.
        assert_eq!(
            db.recipes_for_output(row.blob_id).expect("q").len(),
            2,
            "zip derive + nds rebuild"
        );
        // And its NitroFS file is a claimed identity with a slice route.
        let piece = Blake3::compute(&pattern(300, seed.wrapping_add(41)));
        let piece_row = db.blob_by_hash(&piece).expect("q").expect("piece claimed");
        assert!(
            !db.recipes_for_output(piece_row.blob_id)
                .expect("q")
                .is_empty(),
            "nitrofs piece has its ROM-derive route"
        );
    }
}
