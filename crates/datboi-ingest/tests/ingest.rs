//! End-to-end ingest: synthetic tree → store + index, then the critical
//! round-trip — recipe blobs from meta/ must reproduce member bytes
//! through the real assemble executor / deflate window.

use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use datboi_core::assemble::{self, AssembleParams};
use datboi_core::cbor::{self, Value};
use datboi_core::hash::Blake3;
use datboi_core::recipe::{Op, Recipe};
use datboi_formats::skipper::Detector;
use datboi_index::{AliasAlgo, Db, VerifyState};
use datboi_ingest::{IngestReport, Ingester};
use datboi_store_fs::{Namespace, Store};
use flate2::Compression;
use flate2::write::DeflateEncoder;

const INES_DETECTOR: &str = r#"<?xml version="1.0"?>
<detector>
  <name>test iNES</name>
  <rule start_offset="10">
    <data offset="0" value="4E45531A" result="true"/>
  </rule>
</detector>"#;

/// Hand-written zip builder: deterministic, and it exercises OUR parser
/// against bytes whose layout we control exactly.
struct ZipBuilder {
    data: Vec<u8>,
    central: Vec<u8>,
    entries: u16,
}

impl ZipBuilder {
    fn new() -> Self {
        Self {
            data: Vec::new(),
            central: Vec::new(),
            entries: 0,
        }
    }

    fn add(&mut self, name: &str, contents: &[u8], deflate: bool, flags: u16) -> &mut Self {
        let (method, payload) = if deflate {
            let mut enc = DeflateEncoder::new(Vec::new(), Compression::default());
            enc.write_all(contents).expect("deflate");
            (8u16, enc.finish().expect("deflate finish"))
        } else {
            (0u16, contents.to_vec())
        };
        let crc = {
            let mut h = crc32fast::Hasher::new();
            h.update(contents);
            h.finalize()
        };
        let local_offset = self.data.len() as u32;

        // Local file header.
        self.data.extend_from_slice(b"PK\x03\x04");
        self.data.extend_from_slice(&20u16.to_le_bytes()); // version needed
        self.data.extend_from_slice(&flags.to_le_bytes());
        self.data.extend_from_slice(&method.to_le_bytes());
        self.data.extend_from_slice(&[0; 4]); // dos time+date
        self.data.extend_from_slice(&crc.to_le_bytes());
        self.data
            .extend_from_slice(&(payload.len() as u32).to_le_bytes());
        self.data
            .extend_from_slice(&(contents.len() as u32).to_le_bytes());
        self.data
            .extend_from_slice(&(name.len() as u16).to_le_bytes());
        self.data.extend_from_slice(&0u16.to_le_bytes()); // extra len
        self.data.extend_from_slice(name.as_bytes());
        self.data.extend_from_slice(&payload);

        // Central directory entry.
        self.central.extend_from_slice(b"PK\x01\x02");
        self.central.extend_from_slice(&20u16.to_le_bytes()); // made by
        self.central.extend_from_slice(&20u16.to_le_bytes()); // needed
        self.central.extend_from_slice(&flags.to_le_bytes());
        self.central.extend_from_slice(&method.to_le_bytes());
        self.central.extend_from_slice(&[0; 4]); // time+date
        self.central.extend_from_slice(&crc.to_le_bytes());
        self.central
            .extend_from_slice(&(payload.len() as u32).to_le_bytes());
        self.central
            .extend_from_slice(&(contents.len() as u32).to_le_bytes());
        self.central
            .extend_from_slice(&(name.len() as u16).to_le_bytes());
        self.central.extend_from_slice(&[0; 2]); // extra len
        self.central.extend_from_slice(&[0; 2]); // comment len
        self.central.extend_from_slice(&[0; 2]); // disk start
        self.central.extend_from_slice(&[0; 2]); // internal attrs
        self.central.extend_from_slice(&[0; 4]); // external attrs
        self.central.extend_from_slice(&local_offset.to_le_bytes());
        self.central.extend_from_slice(name.as_bytes());

        self.entries += 1;
        self
    }

    fn finish(&self) -> Vec<u8> {
        let mut out = self.data.clone();
        let cd_offset = out.len() as u32;
        out.extend_from_slice(&self.central);
        out.extend_from_slice(b"PK\x05\x06");
        out.extend_from_slice(&[0; 4]); // disk numbers
        out.extend_from_slice(&self.entries.to_le_bytes());
        out.extend_from_slice(&self.entries.to_le_bytes());
        out.extend_from_slice(&(self.central.len() as u32).to_le_bytes());
        out.extend_from_slice(&cd_offset.to_le_bytes());
        out.extend_from_slice(&[0; 2]); // comment len
        out
    }
}

struct World {
    _dir: tempfile::TempDir,
    store: Store,
    db: Db,
    src: std::path::PathBuf,
    detectors: Vec<Detector>,
}

const A_ROM: &[u8] = b"stored member payload: 0123456789";
const B_ROM: &[u8] = &[0x42; 9000]; // compresses well, multi-read inflate
const PLAIN: &[u8] = b"just a loose rom";
const INES_BODY: &[u8] = &[0x77; 64];

fn ines_file() -> Vec<u8> {
    let mut f = b"NES\x1a".to_vec();
    f.extend_from_slice(&[0x01, 0x01, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]); // rest of 16-byte header
    f.extend_from_slice(INES_BODY);
    f
}

fn setup() -> World {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Store::open(dir.path().join("store")).expect("store");
    let db_dir = dir.path().join("db");
    fs::create_dir_all(&db_dir).expect("db dir");
    let db = Db::open(&db_dir).expect("db");
    let src = dir.path().join("src");
    fs::create_dir_all(src.join("nested")).expect("src tree");

    fs::write(src.join("plain.bin"), PLAIN).expect("plain");
    let mut zb = ZipBuilder::new();
    zb.add("a.rom", A_ROM, false, 0);
    zb.add("dir/b.rom", B_ROM, true, 0);
    fs::write(src.join("nested/game.zip"), zb.finish()).expect("zip");
    fs::write(src.join("game.nes"), ines_file()).expect("nes");

    let detectors = vec![Detector::parse(INES_DETECTOR.as_bytes()).expect("detector")];
    World {
        _dir: dir,
        store,
        db,
        src,
        detectors,
    }
}

fn run(world: &mut World) -> IngestReport {
    let src = world.src.clone();
    let mut ingester = Ingester::new(&world.store, &mut world.db, &world.detectors);
    ingester.ingest(&[src])
}

fn read_recipe(store: &Store, hash: &Blake3) -> Recipe {
    let mut bytes = Vec::new();
    store
        .get(Namespace::Meta, hash)
        .expect("get")
        .expect("recipe blob resident")
        .read_to_end(&mut bytes)
        .expect("read");
    Recipe::decode(&bytes).expect("valid recipe object")
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

/// The one recipe claiming `output_hash`, decoded from the meta namespace.
fn recipe_for(world: &World, output_hash: &Blake3) -> (Recipe, VerifyState) {
    let blob_id = world
        .db
        .get_blob_id(output_hash)
        .expect("query")
        .expect("output blob row");
    let rows = world.db.recipes_for_output(blob_id).expect("recipes");
    assert_eq!(rows.len(), 1, "exactly one claim for {output_hash}");
    let recipe_hash = blob_hash_of(&world.db, rows[0].blob_id);
    (read_recipe(&world.store, &recipe_hash), rows[0].verify)
}

#[test]
fn end_to_end_ingest_and_recipe_round_trip() {
    let mut world = setup();
    let report = run(&mut world);

    assert_eq!(report.errors, vec![], "no errors: {:?}", report.errors);
    assert_eq!(report.files_scanned, 3);
    assert_eq!(report.files_stored, 3);
    assert_eq!(report.members_claimed, 2);
    assert_eq!(report.detector_hits, 1);
    assert!(report.member_skips.is_empty());

    // Containers stay literal; members are claims with no stored bytes.
    let zip_bytes = fs::read(world.src.join("nested/game.zip")).expect("zip");
    let zip_hash = Blake3::compute(&zip_bytes);
    let a_hash = Blake3::compute(A_ROM);
    let b_hash = Blake3::compute(B_ROM);
    assert!(world.store.has(Namespace::Data, &zip_hash));
    assert!(!world.store.has(Namespace::Data, &a_hash));
    assert!(!world.store.has(Namespace::Data, &b_hash));

    // Members are findable by dat-style alias (crc32 of b.rom).
    let crc = {
        let mut h = crc32fast::Hasher::new();
        h.update(B_ROM);
        h.finalize().to_be_bytes()
    };
    let hits = world
        .db
        .alias_lookup(AliasAlgo::Crc32, &crc)
        .expect("lookup");
    let hit_hashes: Vec<Blake3> = hits.iter().map(|id| blob_hash_of(&world.db, *id)).collect();
    assert!(hit_hashes.contains(&b_hash), "crc alias resolves to member");

    // THE round trip: stored member reproduces through the real executor.
    let (a_recipe, a_verify) = recipe_for(&world, &a_hash);
    assert_eq!(a_verify, VerifyState::Verified);
    assert_eq!(a_recipe.inputs[0].hash, zip_hash);
    assert!(matches!(&a_recipe.op, Op::Builtin { name, major: 1 } if name == "assemble"));
    assert_eq!(a_recipe.outputs[0].name.as_deref(), Some("a.rom"));
    let params = AssembleParams::decode(&a_recipe.params).expect("assemble params");
    let sources = [zip_bytes.as_slice()];
    let mut out = Vec::new();
    assemble::reader(&params, &sources)
        .expect("valid")
        .read_to_end(&mut out)
        .expect("materialize");
    assert_eq!(out, A_ROM);

    // Deflate member: window params slice the raw stream, which inflates
    // back to the member bytes.
    let (b_recipe, b_verify) = recipe_for(&world, &b_hash);
    assert_eq!(b_verify, VerifyState::Verified);
    assert!(matches!(&b_recipe.op, Op::Builtin { name, major: 1 } if name == "deflate-decompress"));
    let Value::Map(entries) = cbor::decode(&b_recipe.params).expect("window params") else {
        panic!("window params must be a map");
    };
    let get = |key: u64| -> u64 {
        match entries.iter().find(|(k, _)| *k == key) {
            Some((_, Value::Uint(n))) => *n,
            other => panic!("bad window param {key}: {other:?}"),
        }
    };
    let (off, len) = (get(1) as usize, get(2) as usize);
    let mut inflated = Vec::new();
    flate2::read::DeflateDecoder::new(&zip_bytes[off..off + len])
        .read_to_end(&mut inflated)
        .expect("inflate window");
    assert_eq!(inflated, B_ROM);
    assert_eq!(b_recipe.outputs[0].size, B_ROM.len() as u64);

    // Skipper: headerless variant claimed both directions.
    let nes_bytes = ines_file();
    let nes_hash = Blake3::compute(&nes_bytes);
    let body_hash = Blake3::compute(INES_BODY);
    let (derive, _) = recipe_for(&world, &body_hash);
    assert_eq!(derive.inputs[0].hash, nes_hash);
    assert_eq!(derive.inputs[0].role.as_deref(), Some("skipper:test iNES"));
    let params = AssembleParams::decode(&derive.params).expect("slice params");
    let sources = [nes_bytes.as_slice()];
    let mut body = Vec::new();
    assemble::reader(&params, &sources)
        .expect("valid")
        .read_to_end(&mut body)
        .expect("materialize");
    assert_eq!(body, INES_BODY);

    // Rebuild direction: file = header blob + variant; header is stored.
    let header_hash = Blake3::compute(&nes_bytes[..16]);
    assert!(world.store.has(Namespace::Data, &header_hash));
    let (rebuild, _) = recipe_for(&world, &nes_hash);
    assert_eq!(rebuild.inputs.len(), 2);
    assert_eq!(rebuild.inputs[0].hash, header_hash);
    assert_eq!(rebuild.inputs[1].hash, body_hash);
    let params = AssembleParams::decode(&rebuild.params).expect("rebuild params");
    let header = &nes_bytes[..16];
    let sources = [header, INES_BODY];
    let mut rebuilt = Vec::new();
    assemble::reader(&params, &sources)
        .expect("valid")
        .read_to_end(&mut rebuilt)
        .expect("materialize");
    assert_eq!(rebuilt, nes_bytes);
}

#[test]
fn rescan_cache_makes_second_run_a_noop() {
    let mut world = setup();
    let first = run(&mut world);
    assert_eq!(first.files_stored, 3);

    let count_store_files = |store_root: &Path| -> usize {
        let mut n = 0;
        let mut stack = vec![store_root.to_owned()];
        while let Some(dir) = stack.pop() {
            for entry in fs::read_dir(&dir).expect("read_dir") {
                let path = entry.expect("entry").path();
                if path.is_dir() {
                    stack.push(path);
                } else {
                    n += 1;
                }
            }
        }
        n
    };
    let root = world._dir.path().join("store");
    let before = count_store_files(&root);

    let second = run(&mut world);
    assert_eq!(second.files_unchanged, 3);
    assert_eq!(second.files_stored, 0);
    assert_eq!(second.files_already_present, 0);
    assert_eq!(second.members_claimed, 0);
    assert_eq!(count_store_files(&root), before, "store untouched");
}

#[test]
fn re_ingest_after_cache_loss_is_idempotent() {
    let mut world = setup();
    let first = run(&mut world);
    assert_eq!(first.errors, vec![]);

    world.db.truncate_cache().expect("truncate");
    let second = run(&mut world);
    assert_eq!(second.errors, vec![], "no errors: {:?}", second.errors);
    assert_eq!(second.files_already_present, 3);
    assert_eq!(second.files_stored, 0);
    assert_eq!(second.members_claimed, 2);
    assert_eq!(second.detector_hits, 1);

    // Claims are back and still verified.
    let b_hash = Blake3::compute(B_ROM);
    let (_, verify) = recipe_for(&world, &b_hash);
    assert_eq!(verify, VerifyState::Verified);
}

#[test]
fn corrupt_zip_is_stored_but_reported() {
    let mut world = setup();
    // Looks like a zip (magic) but has no central directory.
    let corrupt = b"PK\x03\x04 nothing else of substance".to_vec();
    fs::write(world.src.join("corrupt.zip"), &corrupt).expect("write");

    let report = run(&mut world);
    assert_eq!(report.errors.len(), 1);
    assert!(report.errors[0].0.ends_with("corrupt.zip"));
    assert!(report.errors[0].1.contains("end-of-central-directory"));
    // The literal is still safely stored and aliased.
    assert!(world.store.has(Namespace::Data, &Blake3::compute(&corrupt)));
}

#[test]
fn encrypted_and_zero_byte_members() {
    let mut world = setup();
    let mut zb = ZipBuilder::new();
    zb.add("secret.rom", b"cannot have this", false, 0x0001); // encrypted flag
    zb.add("empty.rom", b"", false, 0);
    fs::write(world.src.join("mixed.zip"), zb.finish()).expect("write");

    let report = run(&mut world);
    assert_eq!(report.errors, vec![], "no errors: {:?}", report.errors);
    let skips: Vec<_> = report
        .member_skips
        .iter()
        .map(|(_, name, reason)| (name.as_str(), reason.as_str()))
        .collect();
    assert!(skips.contains(&("secret.rom", "encrypted")));

    // Zero-byte member: claimed, grounded by a stored empty literal.
    let empty_hash = Blake3::compute(b"");
    assert!(world.store.has(Namespace::Data, &empty_hash));
    assert!(world.db.get_blob_id(&empty_hash).expect("query").is_some());
}

/// A member whose deflate stream inflates far past its declared size
/// (the classic bomb shape) is refused after declared+1 bytes — the
/// claim is skipped, the container stays a harmless literal.
#[test]
fn zip_bomb_lying_size_is_refused_cheaply() {
    let mut world = setup();
    // 8 MiB of zeros deflates to ~8 KiB; then lie in both headers that
    // it inflates to 10 bytes.
    let huge = vec![0u8; 8 << 20];
    let mut zb = ZipBuilder::new();
    zb.add("innocent.rom", &huge, true, 0);
    let mut bytes = zb.finish();
    let lie = 10u32.to_le_bytes();
    bytes[22..26].copy_from_slice(&lie); // local header uncomp_size
    let eocd = bytes.len() - 22;
    let cd_off = u32::from_le_bytes(bytes[eocd + 16..eocd + 20].try_into().unwrap()) as usize;
    bytes[cd_off + 24..cd_off + 28].copy_from_slice(&lie); // CD uncomp_size
    let bomb_path = world.src.parent().unwrap().join("bomb.zip");
    fs::write(&bomb_path, &bytes).expect("write");

    let report = {
        let mut ingester = Ingester::new(&world.store, &mut world.db, &world.detectors);
        ingester.ingest(&[bomb_path])
    };
    let skips: Vec<_> = report
        .member_skips
        .iter()
        .map(|(_, name, reason)| (name.as_str(), reason.as_str()))
        .collect();
    assert!(
        skips
            .iter()
            .any(|(n, r)| *n == "innocent.rom" && r.contains("bomb-shaped")),
        "expected bomb refusal, got {skips:?}"
    );
    assert_eq!(report.members_claimed, 0);
    // Custody unharmed: the container literal is stored and safe.
    assert!(world.store.has(Namespace::Data, &Blake3::compute(&bytes)));
}

/// Entries sharing raw data ranges (42.zip's trick: thousands of claims
/// over the same bytes) poison the whole directory — no claims minted.
#[test]
fn overlapping_members_refuse_all_claims() {
    let mut world = setup();
    let mut zb = ZipBuilder::new();
    zb.add("one.rom", b"first member data", false, 0);
    zb.add("two.rom", b"secnd member data", false, 0); // same length
    let mut bytes = zb.finish();
    // Point the SECOND central entry's local header at the first's:
    // both members now claim the same raw span.
    let eocd = bytes.len() - 22;
    let cd_off = u32::from_le_bytes(bytes[eocd + 16..eocd + 20].try_into().unwrap()) as usize;
    let first_entry_len = 46 + "one.rom".len();
    let second_local_offset_at = cd_off + first_entry_len + 42;
    bytes[second_local_offset_at..second_local_offset_at + 4].copy_from_slice(&0u32.to_le_bytes());
    let overlap_path = world.src.parent().unwrap().join("overlap.zip");
    fs::write(&overlap_path, &bytes).expect("write");

    let report = {
        let mut ingester = Ingester::new(&world.store, &mut world.db, &world.detectors);
        ingester.ingest(&[overlap_path])
    };
    assert_eq!(report.members_claimed, 0);
    assert!(
        report
            .errors
            .iter()
            .any(|(_, e)| e.contains("bomb-shaped") && e.contains("overlap")),
        "expected overlap refusal, got {:?}",
        report.errors
    );
    assert!(world.store.has(Namespace::Data, &Blake3::compute(&bytes)));
}

/// The zipped-dat probe (the server's drop-surface classifier): the
/// sole member of a single-member zip comes out bounded by `limit`;
/// anything with a different shape answers None; a lying declared
/// size errors instead of yielding short bytes.
#[test]
fn read_sole_member_is_single_member_only_and_bounded() {
    use datboi_ingest::zip::{ZipError, read_sole_member};

    let payload = b"the sole member's bytes";
    let mut zb = ZipBuilder::new();
    zb.add("only.dat", payload, true, 0);
    let mut cursor = std::io::Cursor::new(zb.finish());

    // Full read: size-verified bytes.
    let m = read_sole_member(&mut cursor, 1 << 20)
        .expect("parse")
        .expect("sole member");
    assert_eq!(
        (m.bytes.as_slice(), m.uncomp_size),
        (payload.as_slice(), payload.len() as u64)
    );

    // Sniff read: a prefix, never more than asked — the declared size
    // still rides along so the caller can plan the full read.
    let m = read_sole_member(&mut cursor, 4)
        .expect("parse")
        .expect("sole member");
    assert_eq!(
        (m.bytes.as_slice(), m.uncomp_size),
        (&payload[..4], payload.len() as u64)
    );

    // Two members: a ROM container, not the zipped-dat shape.
    let mut zb = ZipBuilder::new();
    zb.add("a.bin", b"aaaa", false, 0);
    zb.add("b.bin", b"bbbb", false, 0);
    let mut cursor = std::io::Cursor::new(zb.finish());
    assert!(
        read_sole_member(&mut cursor, 1 << 20)
            .expect("parse")
            .is_none()
    );

    // A declared size the data can't honor is an error, not short
    // bytes (the bomb test's lie, one member, full read).
    let mut zb = ZipBuilder::new();
    zb.add("liar.dat", payload, false, 0);
    let mut bytes = zb.finish();
    let lie = ((payload.len() + 7) as u32).to_le_bytes();
    bytes[22..26].copy_from_slice(&lie); // local header uncomp_size
    let eocd = bytes.len() - 22;
    let cd_off = u32::from_le_bytes(bytes[eocd + 16..eocd + 20].try_into().unwrap()) as usize;
    bytes[cd_off + 24..cd_off + 28].copy_from_slice(&lie); // CD uncomp_size
    let mut cursor = std::io::Cursor::new(bytes);
    assert!(matches!(
        read_sole_member(&mut cursor, 1 << 20),
        Err(ZipError::MemberSizeMismatch(name)) if name == "liar.dat"
    ));
}

#[test]
fn zip_member_data_offsets_honor_local_headers() {
    // Local header with a longer extra field than the central directory
    // says: data_start must come from the local header.
    let mut zb = ZipBuilder::new();
    zb.add("x.rom", b"payload", false, 0);
    let mut bytes = zb.finish();
    // Splice 4 extra bytes into the local extra field by hand: bump the
    // local extra_len and shift everything after the local name.
    bytes[28] = 4; // local header extra_len (offset 28 of first LFH)
    let insert_at = 30 + "x.rom".len();
    for _ in 0..4 {
        bytes.insert(insert_at, 0xEE);
    }
    // Fix up CD local-header offset shifts (entry started at 0: unaffected)
    // and EOCD cd_offset (+4).
    let eocd = bytes.len() - 22;
    let cd_off = u32::from_le_bytes(bytes[eocd + 16..eocd + 20].try_into().unwrap()) + 4;
    bytes[eocd + 16..eocd + 20].copy_from_slice(&cd_off.to_le_bytes());

    let mut cursor = std::io::Cursor::new(&bytes);
    cursor.seek(SeekFrom::Start(0)).unwrap();
    let parsed = datboi_ingest::zip::parse_members(&mut cursor).expect("parse");
    assert_eq!(parsed.members.len(), 1);
    let m = &parsed.members[0];
    assert_eq!(m.data_start, (30 + "x.rom".len() + 4) as u64);
    let data = &bytes[m.data_start as usize..(m.data_start + m.comp_size) as usize];
    assert_eq!(data, b"payload");
}
