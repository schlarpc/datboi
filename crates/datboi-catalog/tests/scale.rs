//! MAME-at-scale parse+import validation (roadmap prototype 4): the
//! schema and import path must swallow a full listxml (~50k machines,
//! hundreds of thousands of claims) before they're load-bearing.
//!
//! Two tiers: an always-on smoke at 2k machines (correctness: row counts
//! survive the parse→import→audit pipeline), and an `#[ignore]`d full-scale
//! run at 50k that prints wall-clock numbers — run it manually with
//! `cargo test -p datboi-catalog --release --test scale -- --ignored`.

use std::fmt::Write as _;
use std::time::Instant;

use datboi_catalog::{ImportOptions, audit, import_dat};
use datboi_index::Db;
use datboi_store_fs::Store;

/// Deterministic hex digits for synthetic hashes (no rand dependency).
fn hex_bytes(seed: u64, len: usize) -> String {
    let mut out = String::with_capacity(len * 2);
    let mut state = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    for _ in 0..len {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        let _ = write!(out, "{:02x}", (state >> 24) as u8);
    }
    out
}

/// A synthetic listxml with MAME-like shape: ~1/3 of machines are clones
/// (cloneof/romof to their parent), each machine carries 2–6 roms, every
/// 10th machine has a disk, every 25th rom claim is a nodump.
fn synth_listxml(machines: usize) -> (String, u64) {
    let mut xml = String::with_capacity(machines * 400);
    xml.push_str("<?xml version=\"1.0\"?>\n<mame build=\"0.999 (synthetic)\">\n");
    let mut claims: u64 = 0;
    for m in 0..machines {
        let name = format!("mach{m:05}");
        let parent = m - (m % 3); // thirds: parent, clone, clone
        let clone_attr = if m % 3 != 0 {
            format!(" cloneof=\"mach{parent:05}\" romof=\"mach{parent:05}\"")
        } else {
            String::new()
        };
        let _ = write!(
            xml,
            "<machine name=\"{name}\"{clone_attr}>\n<description>Machine {m}</description>\n"
        );
        let roms = 2 + (m % 5);
        for r in 0..roms {
            let seed = (m * 8 + r) as u64;
            if seed % 25 == 24 {
                let _ = writeln!(
                    xml,
                    "<rom name=\"{name}.{r}\" size=\"{}\" status=\"nodump\"/>",
                    1024 + seed % 4096,
                );
            } else {
                let _ = writeln!(
                    xml,
                    "<rom name=\"{name}.{r}\" size=\"{}\" crc=\"{}\" sha1=\"{}\"/>",
                    1024 + seed % 4096,
                    hex_bytes(seed, 4),
                    hex_bytes(seed, 20),
                );
            }
            claims += 1;
        }
        if m % 10 == 0 {
            let _ = writeln!(
                xml,
                "<disk name=\"{name}\" sha1=\"{}\"/>",
                hex_bytes(m as u64 + 1_000_000, 20)
            );
            claims += 1;
        }
        xml.push_str("</machine>\n");
    }
    xml.push_str("</mame>\n");
    (xml, claims)
}

fn run_scale(machines: usize) {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Store::open(dir.path().join("store")).expect("store");
    let db_dir = dir.path().join("db");
    std::fs::create_dir_all(&db_dir).expect("db dir");
    let mut db = Db::open(&db_dir).expect("db");

    let t0 = Instant::now();
    let (xml, expected_claims) = synth_listxml(machines);
    let gen_t = t0.elapsed();

    let t1 = Instant::now();
    let report = import_dat(
        &store,
        &mut db,
        xml.as_bytes(),
        &ImportOptions {
            provider: Some("MAME"),
            system: Some("synthetic"),
            imported_at: 1_751_800_000,
        },
    )
    .expect("import");
    let import_t = t1.elapsed();

    assert_eq!(report.entries, machines as u64);
    assert_eq!(report.claims, expected_claims);

    let t2 = Instant::now();
    let audit = audit(&db, "MAME", "synthetic").expect("audit");
    let audit_t = t2.elapsed();
    assert_eq!(audit.totals.entries, machines as u64);
    // Nothing ingested: everything required is missing; nodumps excluded.
    assert_eq!(audit.totals.have_verified, 0);
    assert!(audit.totals.required > 0);
    assert_eq!(
        audit.totals.missing + audit.totals.probable,
        audit.totals.required
    );

    println!(
        "{machines} machines / {expected_claims} claims: gen {gen_t:.2?}, import {import_t:.2?} \
         ({:.0} claims/s), audit {audit_t:.2?}, xml {} MiB",
        expected_claims as f64 / import_t.as_secs_f64(),
        xml.len() >> 20,
    );
}

#[test]
fn listxml_smoke_2k() {
    run_scale(2_000);
}

/// The real prototype-4 gate. Run manually (release!):
/// `cargo test -p datboi-catalog --release --test scale -- --ignored --nocapture`
#[test]
#[ignore = "50k-machine scale run; minutes in debug — use --release"]
fn listxml_full_scale_50k() {
    run_scale(50_000);
}
