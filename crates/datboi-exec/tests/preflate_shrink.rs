//! The D53 wild-zip shrink, end to end: a zip container gets split by the
//! preflate sweep into plaintext + corrections + skeleton, the recipes
//! license (D25 replay — the executor runs the `xf-preflate` component
//! inside the assemble operator tree), the container evicts, and the
//! original bytes still stream back bit-exact and serve verified ranges.

use std::io::{Read, Write as _};

use datboi_core::hash::Blake3;
use datboi_exec::{ExecConfig, Executor};
use datboi_index::{Db, Residency};
use datboi_ingest::analyzers::PreflateZipAnalyzer;
use datboi_ingest::refine::run_sweep;
use datboi_store_fs::{Namespace as StoreNs, Store};
use flate2::Compression;
use flate2::write::DeflateEncoder;

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

fn deflate(payload: &[u8], level: u32) -> Vec<u8> {
    let mut enc = DeflateEncoder::new(Vec::new(), Compression::new(level));
    enc.write_all(payload).expect("deflate");
    enc.finish().expect("finish")
}

/// Minimal two-member zip (both DEFLATE), local headers + central dir.
fn zip_two_members(members: &[(&str, &[u8], &[u8])]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut central = Vec::new();
    for (name, payload, compressed) in members {
        let crc = {
            let mut h = crc32fast::Hasher::new();
            h.update(payload);
            h.finalize()
        };
        let header_offset = u32::try_from(out.len()).expect("small");
        out.extend_from_slice(b"PK\x03\x04");
        out.extend_from_slice(&20u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&8u16.to_le_bytes());
        out.extend_from_slice(&[0; 4]);
        out.extend_from_slice(&crc.to_le_bytes());
        out.extend_from_slice(&u32::try_from(compressed.len()).expect("small").to_le_bytes());
        out.extend_from_slice(&u32::try_from(payload.len()).expect("small").to_le_bytes());
        out.extend_from_slice(&u16::try_from(name.len()).expect("small").to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(compressed);

        central.extend_from_slice(b"PK\x01\x02");
        central.extend_from_slice(&20u16.to_le_bytes());
        central.extend_from_slice(&20u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&8u16.to_le_bytes());
        central.extend_from_slice(&[0; 4]);
        central.extend_from_slice(&crc.to_le_bytes());
        central.extend_from_slice(&u32::try_from(compressed.len()).expect("small").to_le_bytes());
        central.extend_from_slice(&u32::try_from(payload.len()).expect("small").to_le_bytes());
        central.extend_from_slice(&u16::try_from(name.len()).expect("small").to_le_bytes());
        central.extend_from_slice(&[0; 12]);
        central.extend_from_slice(&header_offset.to_le_bytes());
        central.extend_from_slice(name.as_bytes());
    }
    let cd_offset = u32::try_from(out.len()).expect("small");
    let cd_len = u32::try_from(central.len()).expect("small");
    let n = u16::try_from(members.len()).expect("small");
    out.extend_from_slice(&central);
    out.extend_from_slice(b"PK\x05\x06");
    out.extend_from_slice(&[0; 4]);
    out.extend_from_slice(&n.to_le_bytes());
    out.extend_from_slice(&n.to_le_bytes());
    out.extend_from_slice(&cd_len.to_le_bytes());
    out.extend_from_slice(&cd_offset.to_le_bytes());
    out.extend_from_slice(&[0; 2]);
    out
}

#[test]
fn preflate_sweep_licenses_evicts_and_rebuilds_bit_exact() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Store::open(dir.path().join("store")).expect("store");
    let mut db = Db::open(dir.path()).expect("db");

    // Two members, different data and levels — multi-member layout math
    // and per-member recipes both get exercised.
    let rom_a = pattern(3 << 20, 0xAAAA_BBBB_CCCC_DDDD);
    let rom_b = pattern(1 << 20 | 137, 0x1111_2222_3333_4444);
    let comp_a = deflate(&rom_a, 9);
    let comp_b = deflate(&rom_b, 6);
    let container = zip_two_members(&[
        ("a.rom", &rom_a, &comp_a),
        ("b.rom", &rom_b, &comp_b),
    ]);
    let container_hash = Blake3::compute(&container);
    store
        .put(StoreNs::Data, container_hash, container.as_slice())
        .expect("put");
    db.upsert_blob(
        &container_hash,
        Some(container.len() as u64),
        datboi_index::Namespace::Data,
        Residency::Resident,
    )
    .expect("row");

    // Sweep: recipes minted, provenance positive.
    let sweep = run_sweep(&mut db, &store, &mut PreflateZipAnalyzer::new(), 1000).expect("sweep");
    assert_eq!(sweep.errors.len(), 0, "{:?}", sweep.errors);
    assert_eq!(sweep.positive, 1, "container split");

    // Member plaintexts became first-class resident blobs.
    let a_hash = Blake3::compute(&rom_a);
    let b_hash = Blake3::compute(&rom_b);
    assert!(store.has(StoreNs::Data, &a_hash));
    assert!(store.has(StoreNs::Data, &b_hash));

    // Before licensing, the planner must be able to SAY why nothing can
    // drop yet — not just return a bare false.
    let exec = Executor::new(&store, ExecConfig::default()).expect("executor");
    let why = exec
        .explain_eviction(&db, &container_hash)
        .expect("explain");
    assert!(
        why.iter().any(|l| l.contains("not yet licensed")),
        "pre-license explanation names the missing replay: {why:?}"
    );

    // Evict with licensing: the container's assemble route replays —
    // which runs the xf-preflate component for both members — and the
    // container literal drops.
    let report = exec.evict_covered(&db, 0, true).expect("planner");
    assert!(report.evicted >= 1, "container evicted: {report:?}");
    assert!(!store.has(StoreNs::Data, &container_hash), "literal gone");

    // The store now holds plaintext instead of the container: the wild
    // zip pays for its plaintext (deduplicable) + corrections (tiny),
    // not for its compressed self.
    // Bit-exact full stream through the recipe route.
    let mut streamed = Vec::new();
    exec.open_stream(&db, &container_hash)
        .expect("route")
        .read_to_end(&mut streamed)
        .expect("read");
    assert_eq!(streamed, container, "container rebuilds bit-exact");

    // Verified range reads through the assemble route still work —
    // including a range inside a member's data (materializes that member
    // through the wasm recreate, not the whole container).
    let got = exec
        .serve_range(&db, &container_hash, 100, 512)
        .expect("range");
    assert_eq!(got, &container[100..612]);

    assert_eq!(
        db.blob_by_hash(&container_hash)
            .expect("q")
            .expect("row")
            .residency,
        Residency::EvictedCovered
    );
}
