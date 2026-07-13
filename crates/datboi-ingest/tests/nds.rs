//! nds-split (D83) over real stores: synthetic ROMs exercising the
//! layouts that break naive tools — files stored out of FAT-ID order,
//! 0xFF cart padding, ARM9 post-data in a gap, the trailing Download
//! Play RSA block, DSi's [210h] trim word, fake size headers, and data
//! appended past the declared size. The critical assertion is the round
//! trip: the minted rebuild recipe, executed through the real assemble
//! reader over piece bytes derived by the minted slice recipes, must
//! reproduce the ROM bit-for-bit.

use std::io::Read as _;

use datboi_core::assemble::{self, AssembleParams, Segment};
use datboi_core::hash::Blake3;
use datboi_core::recipe::Recipe;
use datboi_index::{AliasAlgo, Db, Namespace as IndexNs, Residency};
use datboi_ingest::analyzers::NdsAnalyzer;
use datboi_ingest::nds::crc16;
use datboi_ingest::refine::run_sweep;
use datboi_store_fs::{Namespace as StoreNs, Store};

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

fn sweep(db: &mut Db, store: &Store) -> datboi_ingest::refine::SweepReport {
    let mut analyzer = NdsAnalyzer;
    run_sweep(db, store, &mut analyzer, 100).expect("sweep")
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

fn blob_hash_of(db: &Db, blob_id: i64) -> Blake3 {
    let bytes: Vec<u8> = db
        .cache()
        .query_row(
            "SELECT hash FROM blob WHERE blob_id = ?1",
            (blob_id,),
            |row| row.get(0),
        )
        .expect("blob row");
    Blake3(bytes.try_into().expect("32 bytes"))
}

fn recipes_for(store: &Store, db: &Db, output: &Blake3) -> Vec<Recipe> {
    let blob_id = db
        .get_blob_id(output)
        .expect("query")
        .expect("output blob row");
    db.recipes_for_output(blob_id)
        .expect("recipes")
        .iter()
        .map(|row| {
            let recipe_hash = blob_hash_of(db, row.blob_id);
            let mut bytes = Vec::new();
            store
                .get(StoreNs::Meta, &recipe_hash)
                .expect("get")
                .expect("recipe blob resident")
                .read_to_end(&mut bytes)
                .expect("read");
            Recipe::decode(&bytes).expect("valid recipe object")
        })
        .collect()
}

fn materialize(params: &AssembleParams, sources: &[&[u8]]) -> Vec<u8> {
    let mut out = Vec::new();
    assemble::reader(params, sources)
        .expect("valid")
        .read_to_end(&mut out)
        .expect("materialize");
    out
}

/// Derive one claimed piece's bytes by executing its ROM→piece slice
/// recipe against the original bytes (the grounding loop, for real).
fn derive_piece(store: &Store, db: &Db, rom: &[u8], rom_hash: &Blake3, piece: &Blake3) -> Vec<u8> {
    let recipe = recipes_for(store, db, piece)
        .into_iter()
        .find(|r| r.inputs.len() == 1 && r.inputs[0].hash == *rom_hash)
        .expect("piece has a ROM-derive recipe");
    let params = AssembleParams::decode(&recipe.params).expect("slice params");
    materialize(&params, &[rom])
}

// ---- synthetic ROM builder -------------------------------------------

struct RomBuilder {
    bytes: Vec<u8>,
    pad: u8,
}

impl RomBuilder {
    fn new(pad: u8) -> Self {
        let mut bytes = vec![0u8; 0x200];
        bytes[..4].copy_from_slice(b"TEST");
        // Any logo works: detection compares the STORED CRCs against the
        // computed ones, not against Nintendo's bitmap.
        for (i, b) in bytes[0xC0..0x15C].iter_mut().enumerate() {
            *b = (i as u8).wrapping_mul(7).wrapping_add(3);
        }
        Self { bytes, pad }
    }

    fn len(&self) -> u64 {
        self.bytes.len() as u64
    }

    fn pad_to(&mut self, len: usize) {
        assert!(len >= self.bytes.len());
        self.bytes.resize(len, self.pad);
    }

    fn align(&mut self, to: usize) {
        while !self.bytes.len().is_multiple_of(to) {
            self.bytes.push(self.pad);
        }
    }

    fn put(&mut self, data: &[u8]) -> (u64, u64) {
        let start = self.len();
        self.bytes.extend_from_slice(data);
        (start, data.len() as u64)
    }

    fn set_u32(&mut self, at: usize, v: u32) {
        self.bytes[at..at + 4].copy_from_slice(&v.to_le_bytes());
    }

    /// FNT/FAT/overlay-table shape: offset at `field`, size at +4.
    fn set_section(&mut self, field: usize, (start, len): (u64, u64)) {
        self.set_u32(field, u32::try_from(start).expect("small rom"));
        self.set_u32(field + 4, u32::try_from(len).expect("small rom"));
    }

    /// ARM9/ARM7(/ARM9i) shape: offset at `field`, size at +0Ch (the
    /// two words between are entry/load addresses).
    fn set_arm(&mut self, field: usize, (start, len): (u64, u64)) {
        self.set_u32(field, u32::try_from(start).expect("small rom"));
        self.set_u32(field + 0xC, u32::try_from(len).expect("small rom"));
    }

    /// Stamp both CRCs (logo, then header over [0, 15Eh)) — LAST.
    fn finish(mut self) -> Vec<u8> {
        let logo = crc16(&self.bytes[0xC0..0x15C]);
        self.bytes[0x15C..0x15E].copy_from_slice(&logo.to_le_bytes());
        let header = crc16(&self.bytes[..0x15E]);
        self.bytes[0x15E..0x160].copy_from_slice(&header.to_le_bytes());
        self.bytes
    }
}

/// FNT naming three files: root holds "a.bin" (id 0) and "e.bin" (id 1)
/// plus directory "sub" (F001h) holding "b.bin" (id 2).
fn fnt_three_files() -> Vec<u8> {
    let mut fnt = Vec::new();
    fnt.extend_from_slice(&16u32.to_le_bytes()); // root sub-table offset
    fnt.extend_from_slice(&0u16.to_le_bytes()); // root first file id
    fnt.extend_from_slice(&2u16.to_le_bytes()); // total directories
    fnt.extend_from_slice(&35u32.to_le_bytes()); // F001 sub-table offset
    fnt.extend_from_slice(&2u16.to_le_bytes()); // F001 first file id
    fnt.extend_from_slice(&0xF000u16.to_le_bytes()); // parent
    fnt.extend_from_slice(&[0x05]);
    fnt.extend_from_slice(b"a.bin");
    fnt.extend_from_slice(&[0x05]);
    fnt.extend_from_slice(b"e.bin");
    fnt.extend_from_slice(&[0x83]);
    fnt.extend_from_slice(b"sub");
    fnt.extend_from_slice(&0xF001u16.to_le_bytes());
    fnt.push(0);
    assert_eq!(fnt.len(), 35);
    fnt.extend_from_slice(&[0x05]);
    fnt.extend_from_slice(b"b.bin");
    fnt.push(0);
    fnt
}

fn fat(entries: &[(u64, u64)]) -> Vec<u8> {
    let mut out = Vec::new();
    for (start, end) in entries {
        out.extend_from_slice(&u32::try_from(*start).expect("small").to_le_bytes());
        out.extend_from_slice(&u32::try_from(*end).expect("small").to_le_bytes());
    }
    out
}

fn pattern(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
        .collect()
}

/// The full retail-shaped ROM: 0xFF cart pad, files stored OUT of
/// FAT-ID order, ARM9 post-data in a gap, an empty FAT entry, the
/// trailing "ac" RSA block, power-of-two tail pad. Returns
/// (rom, ntr_size, a_bytes, b_bytes).
fn retail_rom() -> (Vec<u8>, u64, Vec<u8>, Vec<u8>) {
    let arm9_data = pattern(0x300, 9);
    let arm7_data = pattern(0x180, 7);
    let a_data = pattern(300, 41);
    let b_data = pattern(512, 42);

    let mut rb = RomBuilder::new(0xFF);
    rb.bytes[0x12] = 0x00; // unitcode: plain NTR
    rb.pad_to(0x4000);
    let arm9 = rb.put(&arm9_data);
    // ARM9 post-data: real ROMs park tagged records in this gap — the
    // thing ndstool round-trips lose.
    rb.put(&[0x21, 0x06, 0xC0, 0xDE, 1, 2, 3, 4, 5, 6, 7, 8]);
    rb.align(0x200);
    let arm7 = rb.put(&arm7_data);
    rb.align(0x200);
    let fnt = rb.put(&fnt_three_files());
    rb.align(0x200);
    let b_pos = rb.put(&b_data); // id 2 stored FIRST (out of id order)
    rb.align(0x200);
    let a_pos = rb.put(&a_data); // id 0 stored last
    rb.align(0x200);
    // id 1 is a zero-length file (start == end, nonzero).
    let fat_pos = rb.put(&fat(&[
        (a_pos.0, a_pos.0 + a_pos.1),
        (a_pos.0, a_pos.0),
        (b_pos.0, b_pos.0 + b_pos.1),
    ]));
    let ntr_size = rb.len();
    let mut sig = vec![0x61, 0x63]; // "ac"
    sig.extend(pattern(0x86, 77));
    rb.put(&sig);
    rb.pad_to(0x20000);

    rb.set_arm(0x20, arm9);
    rb.set_arm(0x30, arm7);
    rb.set_section(0x40, fnt);
    rb.set_section(0x48, fat_pos);
    rb.set_u32(0x80, u32::try_from(ntr_size).expect("small"));
    rb.set_u32(0x84, 0x4000);
    (rb.finish(), ntr_size, a_data, b_data)
}

#[test]
fn round_trip_is_bit_faithful_and_members_are_claimed() {
    let (rom, ntr_size, a_data, b_data) = retail_rom();
    let (_dir, store, mut db) = world();
    let (rom_hash, _) = put(&store, &db, &rom);

    let report = sweep(&mut db, &store);
    assert_eq!(report.errors, vec![]);
    assert_eq!(
        (report.positive, report.negative),
        (1, 0),
        "analysis details: {:?}",
        analysis_details(&db)
    );

    // THE round trip: rebuild recipe × derived piece bytes == the ROM.
    let rebuilds = recipes_for(&store, &db, &rom_hash);
    assert_eq!(rebuilds.len(), 1, "one rebuild claim for the ROM");
    let rebuild = &rebuilds[0];
    let params = AssembleParams::decode(&rebuild.params).expect("rebuild params");
    let piece_bytes: Vec<Vec<u8>> = rebuild
        .inputs
        .iter()
        .map(|input| derive_piece(&store, &db, &rom, &rom_hash, &input.hash))
        .collect();
    let sources: Vec<&[u8]> = piece_bytes.iter().map(Vec::as_slice).collect();
    assert_eq!(materialize(&params, &sources), rom, "bit-faithful rebuild");

    // Physical storage order is the recipe's order, not FAT-ID order:
    // b.bin (id 2) must appear as a source BEFORE a.bin (id 0).
    let b_ix = piece_bytes.iter().position(|p| *p == b_data).expect("b");
    let a_ix = piece_bytes.iter().position(|p| *p == a_data).expect("a");
    assert!(b_ix < a_ix, "storage order preserved");

    // Members carry their FNT paths; the RSA block is its own piece.
    let b_recipe = recipes_for(&store, &db, &Blake3::compute(&b_data));
    assert_eq!(b_recipe[0].outputs[0].name.as_deref(), Some("sub/b.bin"));
    let sig_hash = Blake3::compute(&rom[ntr_size as usize..(ntr_size + 0x88) as usize]);
    let sig_recipe = recipes_for(&store, &db, &sig_hash);
    assert_eq!(
        sig_recipe[0].outputs[0].name.as_deref(),
        Some("rsa_sig.bin")
    );

    // Pieces are CLAIMS: nothing but the ROM literal is stored.
    assert!(!store.has(StoreNs::Data, &Blake3::compute(&b_data)));
    // The empty FAT entry grounded the empty literal (the zip rule).
    assert!(store.has(StoreNs::Data, &Blake3::compute(b"")));

    // Trim keeps the Download Play RSA block: [80h] + 88h, dat-aliased.
    let trim_len = (ntr_size + 0x88) as usize;
    let trimmed = &rom[..trim_len];
    let trim_recipes = recipes_for(&store, &db, &Blake3::compute(trimmed));
    let trim = trim_recipes
        .iter()
        .find(|r| r.inputs[0].role.as_deref() == Some("nds:trim"))
        .expect("trim claim minted");
    let trim_params = AssembleParams::decode(&trim.params).expect("params");
    assert_eq!(
        trim_params.segments,
        vec![Segment::BlobRange {
            input_ix: 0,
            offset: 0,
            len: trim_len as u64,
        }]
    );
    let crc = {
        let mut h = crc32fast::Hasher::new();
        h.update(trimmed);
        h.finalize().to_be_bytes()
    };
    let hits = db.alias_lookup(AliasAlgo::Crc32, &crc).expect("lookup");
    let hit_hashes: Vec<Blake3> = hits.iter().map(|id| blob_hash_of(&db, *id)).collect();
    assert!(
        hit_hashes.contains(&Blake3::compute(trimmed)),
        "a trimmed dump in the wild dat-matches the claimed identity"
    );

    // Provenance settled: a second sweep never re-analyzes the ROM
    // (blobs the analyzer itself grounded, like the empty literal, do
    // get their own first look — and conclude negative).
    let again = sweep(&mut db, &store);
    assert_eq!(again.positive, 0, "the ROM's conclusion is settled");
}

#[test]
fn dsi_trims_at_the_twl_size_word() {
    let arm9_data = pattern(0x300, 9);
    let arm9i_data = pattern(0x240, 19);
    let mut rb = RomBuilder::new(0xFF);
    rb.bytes[0x12] = 0x02; // unitcode: NDS+DSi
    rb.pad_to(0x4000);
    let arm9 = rb.put(&arm9_data);
    let ntr_size = rb.len();
    rb.align(0x400);
    let arm9i = rb.put(&arm9i_data);
    let twl_size = rb.len();
    rb.pad_to(0x10000);
    rb.set_arm(0x20, arm9);
    rb.set_u32(0x80, u32::try_from(ntr_size).expect("small"));
    rb.set_arm(0x1C0, arm9i);
    let mut rom = rb.finish();
    // [210h] (NTR+TWL total) lives outside both CRCs.
    rom[0x210..0x214].copy_from_slice(&u32::try_from(twl_size).expect("small").to_le_bytes());

    let (_dir, store, mut db) = world();
    let (rom_hash, _) = put(&store, &db, &rom);
    let report = sweep(&mut db, &store);
    assert_eq!(report.errors, vec![]);
    assert_eq!(report.positive, 1, "details: {:?}", analysis_details(&db));

    // Trimming at [80h] would cut arm9i and hang the game — the claim
    // must cover the TWL region.
    let trimmed = Blake3::compute(&rom[..usize::try_from(twl_size).expect("small")]);
    let trim = recipes_for(&store, &db, &trimmed)
        .into_iter()
        .find(|r| r.inputs[0].role.as_deref() == Some("nds:trim"))
        .expect("DSi trim claim");
    assert_eq!(trim.inputs[0].hash, rom_hash);
    assert_eq!(trim.outputs[0].size, twl_size);
}

#[test]
fn fake_size_header_never_offers_a_trim() {
    // The "Egg Monster Hero test": a size word smaller than the real
    // content. Decomposition proceeds; no trim claim exists.
    let (mut rom_bytes, _, _, _) = retail_rom();
    {
        // Rewrite [80h] to a lie and restamp the header CRC.
        rom_bytes[0x80..0x84].copy_from_slice(&0x123u32.to_le_bytes());
        let crc = crc16(&rom_bytes[..0x15E]);
        rom_bytes[0x15E..0x160].copy_from_slice(&crc.to_le_bytes());
    }
    let (_dir, store, mut db) = world();
    put(&store, &db, &rom_bytes);
    let report = sweep(&mut db, &store);
    assert_eq!(report.errors, vec![]);
    assert_eq!(
        report.positive,
        1,
        "decomposition is not trim's hostage — details: {:?}",
        analysis_details(&db)
    );
    let trims: i64 = db
        .cache()
        .query_row(
            "SELECT COUNT(*) FROM recipe_input WHERE role = 'nds:trim'",
            [],
            |row| row.get(0),
        )
        .unwrap_or(0);
    assert_eq!(trims, 0, "no trim claim over a fake size word");
}

#[test]
fn data_appended_past_the_size_word_never_offers_a_trim() {
    // The translation-patch shape: real bytes after [80h]'s size, no
    // "ac" magic. The tail is not pure pad, so trimming would destroy
    // content — refuse the trim, keep the (exact) decomposition.
    let arm9_data = pattern(0x300, 9);
    let mut rb = RomBuilder::new(0xFF);
    rb.pad_to(0x4000);
    let arm9 = rb.put(&arm9_data);
    let ntr_size = rb.len();
    rb.put(&pattern(600, 123)); // appended, undeclared, non-pad
    rb.pad_to(0x10000);
    rb.set_arm(0x20, arm9);
    rb.set_u32(0x80, u32::try_from(ntr_size).expect("small"));
    let rom = rb.finish();
    assert_ne!(&rom[ntr_size as usize..ntr_size as usize + 2], b"ac");

    let (_dir, store, mut db) = world();
    put(&store, &db, &rom);
    let report = sweep(&mut db, &store);
    assert_eq!(report.errors, vec![]);
    assert_eq!(report.positive, 1, "details: {:?}", analysis_details(&db));
    let trims: i64 = db
        .cache()
        .query_row(
            "SELECT COUNT(*) FROM recipe_input WHERE role = 'nds:trim'",
            [],
            |row| row.get(0),
        )
        .unwrap_or(0);
    assert_eq!(trims, 0, "appended data means the tail is not pad");
}

#[test]
fn overlapping_fat_entries_refuse_the_whole_rom() {
    let arm9_data = pattern(0x300, 9);
    let file_data = pattern(0x400, 55);
    let mut rb = RomBuilder::new(0xFF);
    rb.pad_to(0x4000);
    let arm9 = rb.put(&arm9_data);
    rb.align(0x200);
    let f = rb.put(&file_data);
    rb.align(0x200);
    // Two FAT entries share bytes — the shape D83 refuses outright.
    let fat_pos = rb.put(&fat(&[(f.0, f.0 + f.1), (f.0 + 4, f.0 + f.1)]));
    let ntr = rb.len();
    rb.pad_to(0x10000);
    rb.set_arm(0x20, arm9);
    rb.set_section(0x48, fat_pos);
    rb.set_u32(0x80, u32::try_from(ntr).expect("small"));
    let rom = rb.finish();

    let (_dir, store, mut db) = world();
    let (rom_hash, rom_id) = put(&store, &db, &rom);
    let report = sweep(&mut db, &store);
    assert_eq!(report.errors, vec![]);
    assert_eq!((report.positive, report.negative), (0, 1));
    assert!(
        db.recipes_for_output(rom_id).expect("query").is_empty(),
        "refusal mints nothing"
    );
    // The literal stays custodied, untouched.
    assert!(store.has(StoreNs::Data, &rom_hash));
}

#[test]
fn non_nds_blobs_conclude_negative() {
    let (_dir, store, mut db) = world();
    put(
        &store,
        &db,
        b"just some loose rom bytes, nothing nds about them",
    );
    let report = sweep(&mut db, &store);
    assert_eq!(report.errors, vec![]);
    assert_eq!((report.positive, report.negative), (0, 1));
}

/// Real-dump harness: point `DATBOI_NDS_ROMS` at colon-separated .nds
/// paths (full cart dumps) and run with `--ignored --nocapture`. All
/// ROMs share one store, so the printed stats include the cross-ROM
/// piece dedupe the whole lane exists for. Asserts the same invariant
/// as the synthetic tests: every rebuild is bit-faithful.
#[test]
#[ignore = "needs real ROMs via DATBOI_NDS_ROMS"]
fn real_dumps_round_trip() {
    let Ok(paths) = std::env::var("DATBOI_NDS_ROMS") else {
        panic!("set DATBOI_NDS_ROMS=<path>[:<path>…]");
    };
    let (_dir, store, mut db) = world();
    let roms: Vec<(String, Vec<u8>, Blake3)> = paths
        .split(':')
        .map(|p| {
            let bytes = std::fs::read(p).expect("readable rom");
            let (hash, _) = put(&store, &db, &bytes);
            (p.to_owned(), bytes, hash)
        })
        .collect();

    let report = sweep(&mut db, &store);
    assert_eq!(report.errors, vec![]);
    assert_eq!(
        report.positive,
        roms.len(),
        "details: {:?}",
        analysis_details(&db)
    );
    eprintln!("analysis details: {:#?}", analysis_details(&db));

    let mut piece_sets: Vec<std::collections::HashMap<Blake3, u64>> = Vec::new();
    for (path, bytes, hash) in &roms {
        let rebuilds = recipes_for(&store, &db, hash);
        let rebuild = rebuilds
            .iter()
            .find(|r| !r.inputs.is_empty() && r.outputs[0].hash == *hash)
            .expect("rebuild recipe");
        let params = AssembleParams::decode(&rebuild.params).expect("rebuild params");
        let piece_bytes: Vec<Vec<u8>> = rebuild
            .inputs
            .iter()
            .map(|input| derive_piece(&store, &db, bytes, hash, &input.hash))
            .collect();
        let sources: Vec<&[u8]> = piece_bytes.iter().map(Vec::as_slice).collect();
        assert_eq!(
            materialize(&params, &sources),
            *bytes,
            "{path}: rebuild not bit-faithful"
        );
        eprintln!(
            "{path}: {} unique piece(s), {} segment(s), bit-faithful ✓",
            rebuild.inputs.len(),
            params.segments.len(),
        );
        piece_sets.push(
            rebuild
                .inputs
                .iter()
                .zip(&piece_bytes)
                .map(|(i, b)| (i.hash, b.len() as u64))
                .collect(),
        );
    }

    if let [a, b] = piece_sets.as_slice() {
        let shared: u64 = a
            .iter()
            .filter(|(h, _)| b.contains_key(*h))
            .map(|(_, len)| len)
            .sum();
        let a_total: u64 = a.values().sum();
        eprintln!(
            "cross-rom dedupe: {} of {} piece(s) shared, {shared} of {a_total} bytes ({:.1}%)",
            a.keys().filter(|h| b.contains_key(*h)).count(),
            a.len(),
            100.0 * shared as f64 / a_total as f64,
        );
    }
}
