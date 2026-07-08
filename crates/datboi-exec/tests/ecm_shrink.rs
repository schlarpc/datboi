//! The ECM shrink, end to end: a raw CD image gets split by the sweep
//! into stripped payload + layout, the recipe licenses (running the
//! xf-ecm component), the image evicts, and it still streams back
//! bit-exact AND serves verified ranges through the component's
//! manifest-seekable seek path (D49).

use std::io::Read;

use datboi_core::hash::Blake3;
use datboi_exec::{ExecConfig, Executor};
use datboi_index::{Db, Residency};
use datboi_ingest::analyzers::EcmAnalyzer;
use datboi_ingest::refine::run_sweep;
use datboi_store_fs::{Namespace as StoreNs, Store};
use xf_ecm::{SECTOR, rebuild_sector, stripped_len};

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

/// A 200-sector mode 1 image with a 5-sector damaged span in the middle
/// (flipped ECC bytes -> those sectors stay literal).
fn image() -> Vec<u8> {
    let mut out = Vec::new();
    for i in 0u32..200 {
        let mut s = pattern(stripped_len(1).unwrap(), 0x5EC7_0000 + u64::from(i));
        s[..3].copy_from_slice(&[0, 2, (i % 75) as u8]);
        let mut sector = rebuild_sector(1, &s);
        if (90..95).contains(&i) {
            sector[2100] ^= 0xFF; // damage parity: not regenerable
        }
        out.extend_from_slice(&sector);
    }
    out
}

#[test]
fn ecm_sweep_licenses_evicts_and_serves_ranges() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Store::open(dir.path().join("store")).expect("store");
    let mut db = Db::open(dir.path()).expect("db");

    let bin = image();
    let bin_hash = Blake3::compute(&bin);
    store
        .put(StoreNs::Data, bin_hash, bin.as_slice())
        .expect("put");
    db.upsert_blob(
        &bin_hash,
        Some(bin.len() as u64),
        datboi_index::Namespace::Data,
        Residency::Resident,
    )
    .expect("row");

    let sweep = run_sweep(&mut db, &store, &mut EcmAnalyzer::new(), 1000).expect("sweep");
    assert_eq!(sweep.errors.len(), 0, "{:?}", sweep.errors);
    assert_eq!(sweep.positive, 1, "image split");

    // Evict with licensing: the recreate recipe replays (running the
    // component), the original literal drops.
    let exec = Executor::new(&store, ExecConfig::default()).expect("executor");
    let report = exec.evict_covered(&db, 0, true).expect("planner");
    assert!(report.evicted >= 1, "image evicted: {report:?}");
    assert!(!store.has(StoreNs::Data, &bin_hash), "literal gone");

    // Bit-exact full stream through the recipe route.
    let mut streamed = Vec::new();
    exec.open_stream(&db, &bin_hash)
        .expect("route")
        .read_to_end(&mut streamed)
        .expect("read");
    assert_eq!(streamed, bin, "image rebuilds bit-exact");

    // Verified ranges via the component's manifest-seekable path —
    // including windows inside the damaged (literal) span and across
    // sector boundaries.
    for (offset, len) in [
        (0u64, 16u64),
        (SECTOR as u64 - 4, 8),
        (91 * SECTOR as u64 + 2000, 400), // inside the damaged span
        (89 * SECTOR as u64 + 2300, 200), // regenerable -> damaged boundary
        (bin.len() as u64 - 64, 200),     // EOF clamp
    ] {
        let got = exec.serve_range(&db, &bin_hash, offset, len).expect("range");
        let start = usize::try_from(offset.min(bin.len() as u64)).expect("small");
        let end =
            usize::try_from(offset.saturating_add(len).min(bin.len() as u64)).expect("small");
        assert_eq!(got, &bin[start..end], "window {offset}+{len}");
    }
    assert!(
        !db.is_seek_quarantined(&EcmAnalyzer::component_hash())
            .expect("q"),
        "honest component stays trusted"
    );
    assert_eq!(
        db.blob_by_hash(&bin_hash)
            .expect("q")
            .expect("row")
            .residency,
        Residency::EvictedCovered
    );
}
