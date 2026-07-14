//! The M2 exit gate (roadmap.md, D50): a multi-GB zip member replays
//! in bounded memory, verified, both sequential and seeked — the workload
//! D46 built the streaming world for (a single-member Redump-scale
//! DEFLATE stream that whole-buffer @1 could never replay).
//!
//! Default run uses a CI-sized member (64 MiB) so the gate is always on.
//! The REAL exit test is `DATBOI_GATE_FULL=1 cargo test -p datboi-exec
//! --test gate --release` — a ~3.9 GiB member (the zip32 ceiling; zip64
//! is out of M1 ingest scope by design). Peak RSS is asserted via
//! VmHWM, so accidental whole-buffering anywhere in the path fails loud.

use std::fs::{self, File};
use std::io::{BufWriter, Read, Write};

use datboi_core::hash::Blake3;
use datboi_exec::{ExecConfig, Executor};
use datboi_index::{Db, Residency, VerifyState};
use datboi_ingest::Ingester;
use datboi_store_fs::{Namespace as StoreNs, Store, obao};
use flate2::Compression;
use flate2::write::DeflateEncoder;

/// Deterministic, DEFLATE-friendly pattern: a fixed pseudo-random 4 KiB
/// tile, salted per MiB so the stream isn't trivially one repeated block.
struct PatternReader {
    tile: Vec<u8>,
    pos: u64,
    len: u64,
}

impl PatternReader {
    fn new(len: u64) -> Self {
        let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
        let tile = (0..4096)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                (state >> 24) as u8
            })
            .collect();
        Self { tile, pos: 0, len }
    }

    fn byte_at(&self, pos: u64) -> u8 {
        self.tile[(pos % 4096) as usize] ^ ((pos >> 20) as u8)
    }

    /// The expected bytes of `range` without materializing anything.
    fn slice(len: u64, range: std::ops::Range<u64>) -> Vec<u8> {
        let r = Self::new(len);
        range.map(|p| r.byte_at(p)).collect()
    }
}

impl Read for PatternReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = usize::try_from((self.len - self.pos).min(buf.len() as u64)).expect("bounded");
        for (i, b) in buf[..n].iter_mut().enumerate() {
            *b = self.byte_at(self.pos + i as u64);
        }
        self.pos += n as u64;
        Ok(n)
    }
}

fn member_len() -> u64 {
    if let Ok(v) = std::env::var("DATBOI_GATE_MEMBER_BYTES") {
        return v
            .parse()
            .expect("DATBOI_GATE_MEMBER_BYTES must be a byte count");
    }
    if std::env::var("DATBOI_GATE_FULL").is_ok_and(|v| v == "1") {
        // Just under the zip32 size ceiling (~3.9 GiB): the biggest
        // member the M1 ingest path can claim.
        3_900_000_000
    } else {
        64 << 20
    }
}

/// Write a single-member DEFLATE zip of `len` pattern bytes, streaming
/// (two passes: compress to a payload temp while hashing, then splice
/// headers + payload). Returns (member crc32, compressed size).
fn write_zip(zip_path: &std::path::Path, payload_path: &std::path::Path, len: u64) -> (u32, u64) {
    let name = b"big.iso";

    // Pass 1: deflate the pattern to the payload file, computing crc32.
    let payload = File::create(payload_path).expect("payload temp");
    let mut enc = DeflateEncoder::new(BufWriter::new(payload), Compression::fast());
    let mut src = PatternReader::new(len);
    let mut crc = crc32fast::Hasher::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = src.read(&mut buf).expect("pattern");
        if n == 0 {
            break;
        }
        crc.update(&buf[..n]);
        enc.write_all(&buf[..n]).expect("deflate");
    }
    enc.finish()
        .expect("deflate finish")
        .flush()
        .expect("flush");
    let crc = crc.finalize();
    let comp_size = fs::metadata(payload_path).expect("meta").len();
    assert!(len < u64::from(u32::MAX), "zip32 ceiling");
    assert!(comp_size < u64::from(u32::MAX), "zip32 ceiling");

    // Pass 2: assemble the zip around the payload.
    let mut zip = BufWriter::new(File::create(zip_path).expect("zip file"));
    let mut local = Vec::new();
    local.extend_from_slice(b"PK\x03\x04");
    local.extend_from_slice(&20u16.to_le_bytes()); // version needed
    local.extend_from_slice(&0u16.to_le_bytes()); // flags
    local.extend_from_slice(&8u16.to_le_bytes()); // deflate
    local.extend_from_slice(&[0; 4]); // dos time+date
    local.extend_from_slice(&crc.to_le_bytes());
    local.extend_from_slice(&u32::try_from(comp_size).expect("checked").to_le_bytes());
    local.extend_from_slice(&u32::try_from(len).expect("checked").to_le_bytes());
    local.extend_from_slice(&u16::try_from(name.len()).expect("short").to_le_bytes());
    local.extend_from_slice(&0u16.to_le_bytes()); // extra len
    local.extend_from_slice(name);
    zip.write_all(&local).expect("local header");
    std::io::copy(&mut File::open(payload_path).expect("payload"), &mut zip).expect("payload copy");

    let cd_offset = local.len() as u64 + comp_size;
    let mut central = Vec::new();
    central.extend_from_slice(b"PK\x01\x02");
    central.extend_from_slice(&20u16.to_le_bytes()); // made by
    central.extend_from_slice(&20u16.to_le_bytes()); // needed
    central.extend_from_slice(&0u16.to_le_bytes()); // flags
    central.extend_from_slice(&8u16.to_le_bytes()); // deflate
    central.extend_from_slice(&[0; 4]); // time+date
    central.extend_from_slice(&crc.to_le_bytes());
    central.extend_from_slice(&u32::try_from(comp_size).expect("checked").to_le_bytes());
    central.extend_from_slice(&u32::try_from(len).expect("checked").to_le_bytes());
    central.extend_from_slice(&u16::try_from(name.len()).expect("short").to_le_bytes());
    central.extend_from_slice(&[0; 2]); // extra
    central.extend_from_slice(&[0; 2]); // comment
    central.extend_from_slice(&[0; 2]); // disk start
    central.extend_from_slice(&[0; 2]); // internal attrs
    central.extend_from_slice(&[0; 4]); // external attrs
    central.extend_from_slice(&0u32.to_le_bytes()); // local offset
    central.extend_from_slice(name);
    zip.write_all(&central).expect("central dir");
    zip.write_all(b"PK\x05\x06").expect("eocd");
    zip.write_all(&[0; 4]).expect("disks");
    zip.write_all(&1u16.to_le_bytes()).expect("count");
    zip.write_all(&1u16.to_le_bytes()).expect("count");
    zip.write_all(&u32::try_from(central.len()).expect("small").to_le_bytes())
        .expect("cd size");
    zip.write_all(&u32::try_from(cd_offset).expect("checked").to_le_bytes())
        .expect("cd offset");
    zip.write_all(&[0; 2]).expect("comment len");
    zip.flush().expect("flush");
    (crc, comp_size)
}

/// Peak resident set (kB) of this process so far, from VmHWM.
fn peak_rss_kb() -> Option<u64> {
    let status = fs::read_to_string("/proc/self/status").ok()?;
    let line = status.lines().find(|l| l.starts_with("VmHWM:"))?;
    line.split_whitespace().nth(1)?.parse().ok()
}

#[test]
fn big_member_replays_bounded_verified_sequential_and_seeked() {
    let len = member_len();
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Store::open(dir.path().join("store")).expect("store");
    let mut db = Db::open(dir.path()).expect("db");

    // Build + ingest the container (members become claims, no bytes).
    let zip_path = dir.path().join("big.zip");
    let payload_path = dir.path().join("payload.deflate");
    write_zip(&zip_path, &payload_path, len);
    fs::remove_file(&payload_path).expect("cleanup");
    let report = Ingester::new(&store, &mut db, &[]).ingest(&[&zip_path]);
    assert_eq!(report.errors.len(), 0, "{:?}", report.errors);
    assert_eq!(report.members_claimed, 1);
    fs::remove_file(&zip_path).expect("source no longer needed");

    // Find the member's identity + recipe.
    let member_hash = {
        // The only Absent data blob is the member claim.
        let mut stmt = db
            .cache()
            .prepare("SELECT hash FROM blob WHERE residency = 2 AND namespace = 0")
            .expect("q");
        let rows: Vec<(Vec<u8>,)> = stmt
            .query_map([], |r| Ok((r.get::<_, Vec<u8>>(0)?,)))
            .expect("q")
            .collect::<Result<Vec<_>, _>>()
            .expect("q");
        assert_eq!(rows.len(), 1);
        Blake3(rows[0].0.clone().try_into().expect("32 bytes"))
    };
    let member_id = db.get_blob_id(&member_hash).expect("q").expect("indexed");
    let recipes = db.recipes_for_output(member_id).expect("q");
    assert_eq!(recipes.len(), 1);
    let recipe_id = recipes[0].recipe_id;
    assert_eq!(recipes[0].op_name, "deflate-decompress@1");

    // SEQUENTIAL: replay the member — one streaming pass, hash-verified,
    // outboard built (D25 licensing).
    let exec = Executor::new(
        &store,
        ExecConfig {
            spill_dir: Some(dir.path().to_owned()),
            ..ExecConfig::default()
        },
    )
    .expect("executor");
    exec.replay(&db, recipe_id).expect("bounded replay");
    assert_eq!(
        db.recipe_by_id(recipe_id).expect("q").verify,
        VerifyState::ReplayedLocal
    );
    assert!(store.has(StoreNs::Data, &member_hash));
    let sidecar = store
        .get_obao(StoreNs::Data, &member_hash)
        .expect("q")
        .expect("outboard built during replay");
    assert_eq!(sidecar.len() as u64, obao::outboard_size(len));

    // Resident verified range reads at group boundaries ±1.
    let g = obao::GROUP_BYTES;
    for offset in [0, g - 1, g, g + 1, len / 2, len - g - 1, len - 1] {
        let take = 64.min(len - offset);
        let got = store
            .read_range_verified(StoreNs::Data, &member_hash, offset, take)
            .expect("verified read");
        assert_eq!(
            got,
            PatternReader::slice(len, offset..offset + take),
            "literal range at {offset}"
        );
    }

    // SEEKED THROUGH THE RECIPE: evict the literal (planner-style: bytes
    // gone, outboard kept — D49 rule 1), then serve ranges through the
    // opaque route (spill) with mandatory output-bao verification.
    let path = dir
        .path()
        .join("store")
        .join(datboi_store_fs::layout::blob_path(
            StoreNs::Data,
            &member_hash,
        ));
    fs::remove_file(&path).expect("evict literal");
    db.set_residency(member_id, Residency::EvictedCovered)
        .expect("residency");

    // One wide range crossing several group boundaries, one at the very
    // start, one ending exactly at EOF. (Each serve of an opaque route
    // spills the whole member — that is the accepted cost the residency
    // planner weighs; correctness is what's on trial here.)
    let wide_start = (len / 2) - (len / 2) % g - 1;
    for (offset, take) in [
        (0u64, 1024u64),
        (wide_start, (3 * g + 2).min(len - wide_start)),
        (len - 513, 513),
    ] {
        let got = exec
            .serve_range(&db, &member_hash, offset, take)
            .expect("recipe-served verified range");
        assert_eq!(
            got,
            PatternReader::slice(len, offset..offset + take),
            "recipe range at {offset}"
        );
    }

    // Bounded memory: nothing in the path may scale RSS with member
    // size. 512 MiB covers wasmtime engines, SQLite, outboard buffers,
    // and test scaffolding with a wide margin — a whole-buffer bug at
    // the full 3.9 GiB size blows straight through it.
    if let Some(kb) = peak_rss_kb() {
        assert!(
            kb < 512 * 1024,
            "peak RSS {kb} kB — something buffered the member"
        );
    }
}
