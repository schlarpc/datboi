//! End-to-end CLI test: ingest → dat import → audit → export → recover →
//! re-import → re-audit → scrub → status, in one tempdir universe.
//!
//! The recover/re-audit equivalence deliberately uses plain-file claims
//! only: zip-member claims ride on recipes, which recover re-enters as
//! Pending (no verification provenance) until lazy re-verification —
//! documented M1 scope in `datboi recover --help`.

use std::fs;
use std::path::Path;
use std::process::Command;

use assert_cmd::prelude::*;
use datboi_core::alias::AliasHasher;
use predicates::prelude::*;

struct Universe {
    root: tempfile::TempDir,
}

impl Universe {
    fn new() -> Self {
        Self {
            root: tempfile::tempdir().expect("tempdir"),
        }
    }
    fn store(&self) -> std::path::PathBuf {
        self.root.path().join("store")
    }
    fn db(&self) -> std::path::PathBuf {
        self.root.path().join("db")
    }
    fn src(&self) -> std::path::PathBuf {
        self.root.path().join("src")
    }
    fn cmd(&self) -> Command {
        let mut cmd = Command::cargo_bin("datboi").expect("binary");
        cmd.arg("--store")
            .arg(self.store())
            .arg("--db-dir")
            .arg(self.db());
        cmd
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn rom_xml(name: &str, content: &[u8]) -> String {
    let mut hasher = AliasHasher::new();
    hasher.update(content);
    let t = hasher.finalize();
    format!(
        r#"<rom name="{name}" size="{}" crc="{}" md5="{}" sha1="{}"/>"#,
        t.size,
        hex(&t.crc32),
        hex(&t.md5),
        hex(&t.sha1),
    )
}

/// Minimal STORED-only zip (mirrors the datboi-ingest fixture approach).
fn stored_zip(members: &[(&str, &[u8])]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut central = Vec::new();
    for (name, data) in members {
        let crc = {
            let mut h = crc32fast::Hasher::new();
            h.update(data);
            h.finalize()
        };
        let lho = u32::try_from(out.len()).unwrap();
        let (nlen, size) = (
            u16::try_from(name.len()).unwrap(),
            u32::try_from(data.len()).unwrap(),
        );
        // Local file header.
        out.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
        out.extend_from_slice(&20u16.to_le_bytes()); // version needed
        out.extend_from_slice(&0u16.to_le_bytes()); // flags
        out.extend_from_slice(&0u16.to_le_bytes()); // method: STORED
        out.extend_from_slice(&0u32.to_le_bytes()); // dos time+date
        out.extend_from_slice(&crc.to_le_bytes());
        out.extend_from_slice(&size.to_le_bytes()); // csize
        out.extend_from_slice(&size.to_le_bytes()); // usize
        out.extend_from_slice(&nlen.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes()); // extra len
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(data);
        // Central directory entry.
        central.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
        central.extend_from_slice(&20u16.to_le_bytes()); // made by
        central.extend_from_slice(&20u16.to_le_bytes()); // needed
        central.extend_from_slice(&0u16.to_le_bytes()); // flags
        central.extend_from_slice(&0u16.to_le_bytes()); // method
        central.extend_from_slice(&0u32.to_le_bytes()); // time+date
        central.extend_from_slice(&crc.to_le_bytes());
        central.extend_from_slice(&size.to_le_bytes());
        central.extend_from_slice(&size.to_le_bytes());
        central.extend_from_slice(&nlen.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes()); // extra
        central.extend_from_slice(&0u16.to_le_bytes()); // comment
        central.extend_from_slice(&0u16.to_le_bytes()); // disk
        central.extend_from_slice(&0u16.to_le_bytes()); // int attrs
        central.extend_from_slice(&0u32.to_le_bytes()); // ext attrs
        central.extend_from_slice(&lho.to_le_bytes());
        central.extend_from_slice(name.as_bytes());
    }
    let cd_offset = u32::try_from(out.len()).unwrap();
    let cd_size = u32::try_from(central.len()).unwrap();
    let count = u16::try_from(members.len()).unwrap();
    out.extend_from_slice(&central);
    out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes()); // disk
    out.extend_from_slice(&0u16.to_le_bytes()); // cd disk
    out.extend_from_slice(&count.to_le_bytes());
    out.extend_from_slice(&count.to_le_bytes());
    out.extend_from_slice(&cd_size.to_le_bytes());
    out.extend_from_slice(&cd_offset.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes()); // comment len
    out
}

fn dat_xml(name: &str, games: &str) -> String {
    format!(
        r#"<?xml version="1.0"?>
<datafile>
  <header>
    <name>{name}</name>
    <description>{name} fixture</description>
    <version>1</version>
    <author>test</author>
  </header>
{games}
</datafile>
"#
    )
}

fn audit_json(universe: &Universe, source: &str) -> (serde_json::Value, Option<i32>) {
    let output = universe
        .cmd()
        .args(["audit", source, "--json"])
        .output()
        .expect("audit runs");
    let value = serde_json::from_slice(&output.stdout).expect("audit emits json");
    (value, output.status.code())
}

#[test]
fn end_to_end() {
    let u = Universe::new();
    fs::create_dir_all(u.src()).unwrap();

    // Fixture tree: two plain roms, one zip (member outside the dat).
    let alpha = b"alpha rom content".as_slice();
    let beta = b"beta rom content!".as_slice();
    let member = b"zipped member payload".as_slice();
    fs::write(u.src().join("alpha.gba"), alpha).unwrap();
    fs::write(u.src().join("beta.gba"), beta).unwrap();
    fs::write(
        u.src().join("pack.zip"),
        stored_zip(&[("inner.bin", member)]),
    )
    .unwrap();

    // Ingest.
    u.cmd()
        .arg("ingest")
        .arg(u.src())
        .arg("--json")
        .assert()
        .success()
        .stdout(predicate::str::contains("\"members_claimed\":1"));

    // Dat: alpha + beta present, one missing, one nodump.
    let games = format!(
        r#"  <game name="Alpha"><description>Alpha</description>{}</game>
  <game name="Beta"><description>Beta</description>{}</game>
  <game name="Gone"><description>Gone</description><rom name="gone.gba" size="99" sha1="{}"/></game>
  <game name="Undumped"><description>Undumped</description><rom name="lost.gba" status="nodump"/></game>"#,
        rom_xml("alpha.gba", alpha),
        rom_xml("beta.gba", beta),
        "00".repeat(20),
    );
    let dat_path = u.root.path().join("fixture.dat");
    fs::write(&dat_path, dat_xml("Test GBA", &games)).unwrap();
    u.cmd()
        .args(["dat", "import"])
        .arg(&dat_path)
        .args(["--provider", "test", "--system", "gba"])
        .assert()
        .success()
        .stdout(predicate::str::contains("4 entries"));

    u.cmd()
        .args(["dat", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("test/gba"));

    // Audit: incomplete (Gone missing), exit code 1; exact counts.
    let (audit1, code) = audit_json(&u, "test/gba");
    assert_eq!(code, Some(1));
    let t = &audit1["totals"];
    assert_eq!(t["entries"], 4);
    assert_eq!(t["entries_complete"], 3); // Alpha, Beta, Undumped (nodump ⇒ trivially complete)
    assert_eq!(t["have_verified"], 2);
    assert_eq!(t["missing"], 1);
    assert_eq!(audit1["complete"], false);

    // --missing lists only the incomplete entry.
    u.cmd()
        .args(["audit", "test/gba", "--missing"])
        .assert()
        .code(1)
        .stdout(predicate::str::contains("Gone").and(predicate::str::contains("Alpha").not()));

    // Export round-trips through the CLI.
    let exported = u.root.path().join("exported.dat");
    u.cmd()
        .args(["export", "dat", "test/gba", "-o"])
        .arg(&exported)
        .assert()
        .success();
    let exported_text = fs::read_to_string(&exported).unwrap();
    assert!(exported_text.contains("\"Alpha\""));
    assert!(exported_text.contains("nodump"));

    // A complete source exits 0.
    let mini_path = u.root.path().join("mini.dat");
    fs::write(
        &mini_path,
        dat_xml(
            "Mini",
            &format!(
                r#"  <game name="Alpha"><description>Alpha</description>{}</game>"#,
                rom_xml("alpha.gba", alpha)
            ),
        ),
    )
    .unwrap();
    u.cmd()
        .args(["dat", "import"])
        .arg(&mini_path)
        .args(["--provider", "test", "--system", "mini"])
        .assert()
        .success();
    u.cmd().args(["audit", "test/mini"]).assert().code(0);

    // Recover: blob + recipe indexes rebuilt, catalog gone until re-import.
    u.cmd()
        .arg("recover")
        .assert()
        .success()
        .stdout(predicate::str::contains("recipes indexed"))
        .stdout(predicate::str::contains("dat import"));
    u.cmd().args(["audit", "test/gba"]).assert().code(2); // unknown source: catalog state is gone, as documented
    u.cmd()
        .args(["dat", "import"])
        .arg(&dat_path)
        .args(["--provider", "test", "--system", "gba"])
        .assert()
        .success();
    let (audit2, code2) = audit_json(&u, "test/gba");
    assert_eq!(code2, Some(1));
    assert_eq!(
        audit2["totals"], audit1["totals"],
        "audit equals pre-recover"
    );

    // Scrub: clean pass first, then detect a hand-corrupted blob.
    u.cmd().arg("scrub").assert().code(0);
    corrupt_one_data_blob(&u.store().join("data"));
    u.cmd()
        .arg("scrub")
        .assert()
        .code(1)
        .stdout(predicate::str::contains("CORRUPT"));

    // Status runs and reflects both namespaces.
    u.cmd()
        .args(["status", "--json"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"namespace\":\"meta\""));
}

/// Flip one byte in the middle of the largest data blob (never truncates).
fn corrupt_one_data_blob(data_root: &Path) {
    let mut victim: Option<(std::path::PathBuf, u64)> = None;
    for entry in walk(data_root) {
        let len = entry.metadata().map(|m| m.len()).unwrap_or(0);
        if len > 4 && victim.as_ref().is_none_or(|(_, best)| len > *best) {
            victim = Some((entry, len));
        }
    }
    let (path, len) = victim.expect("a data blob to corrupt");
    let mut bytes = fs::read(&path).unwrap();
    let mid = usize::try_from(len / 2).unwrap();
    bytes[mid] ^= 0xff;
    fs::write(&path, bytes).unwrap();
}

fn walk(root: &Path) -> Vec<std::path::PathBuf> {
    let mut files = Vec::new();
    let mut dirs = vec![root.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        let Ok(rd) = fs::read_dir(&dir) else { continue };
        for entry in rd.flatten() {
            let path = entry.path();
            if path.is_dir() {
                dirs.push(path);
            } else if path.extension().is_some_and(|e| e == "data") {
                files.push(path);
            }
        }
    }
    files
}

#[test]
fn missing_config_is_a_clear_error() {
    let mut cmd = Command::cargo_bin("datboi").expect("binary");
    cmd.env_remove("DATBOI_STORE")
        .env_remove("DATBOI_DB_DIR")
        .args(["status"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("DATBOI_STORE"));
}

#[test]
fn move_is_explicitly_unimplemented() {
    let u = Universe::new();
    fs::create_dir_all(u.src()).unwrap();
    u.cmd()
        .arg("ingest")
        .arg(u.src())
        .arg("--move")
        .assert()
        .code(2)
        .stderr(predicate::str::contains("not implemented"));
}

/// The bare-NAS recovery drill (90-roadmap.md M1): mint a signed snapshot,
/// nuke every database (keeping the identity key — the one non-CAS secret),
/// recover, and the audit must come back byte-identical with zero manual
/// re-imports.
#[test]
fn snapshot_recovery_drill() {
    let u = Universe::new();
    fs::create_dir_all(u.src()).unwrap();

    let alpha = vec![0xA1u8; 4096];
    let beta = vec![0xB2u8; 2048];
    fs::write(u.src().join("alpha.gba"), &alpha).unwrap();
    fs::write(u.src().join("beta.gba"), &beta).unwrap();
    u.cmd().arg("ingest").arg(u.src()).assert().success();

    let dat_path = u.root.path().join("drill.dat");
    fs::write(
        &dat_path,
        dat_xml(
            "drill",
            &format!(
                r#"<game name="Alpha"><description>Alpha</description>{}</game>
                   <game name="Beta"><description>Beta</description>{}</game>
                   <game name="Gone"><description>Gone</description>{}</game>"#,
                rom_xml("Alpha.gba", &alpha),
                rom_xml("Beta.gba", &beta),
                rom_xml("Gone.gba", b"never ingested"),
            ),
        ),
    )
    .unwrap();
    u.cmd()
        .args(["dat", "import"])
        .arg(&dat_path)
        .args(["--provider", "test", "--system", "drill"])
        .assert()
        .success();

    let (before, code_before) = audit_json(&u, "test/drill");
    assert_eq!(code_before, Some(1)); // Gone.gba is missing on purpose

    // Mint the snapshot (creates the identity key on first use).
    let snap_out = u
        .cmd()
        .args(["snapshot", "--json"])
        .output()
        .expect("snapshot runs");
    assert!(snap_out.status.success());
    let snap: serde_json::Value = serde_json::from_slice(&snap_out.stdout).unwrap();
    assert_eq!(snap["sequence"], 1);
    assert_eq!(snap["sources"], 1);

    // Nuke every database file; keep identity.key (out-of-band-backed-up).
    for entry in fs::read_dir(u.db()).unwrap() {
        let path = entry.unwrap().path();
        if path.file_name().is_some_and(|n| n != "identity.key") {
            fs::remove_file(&path).unwrap();
        }
    }

    // Recover replays the dat import from the snapshot automatically.
    u.cmd()
        .arg("recover")
        .assert()
        .success()
        .stdout(predicate::str::contains("snapshot used"))
        .stdout(predicate::str::contains("dats re-imported"))
        .stdout(predicate::str::contains("catalog restored"));

    let (after, code_after) = audit_json(&u, "test/drill");
    assert_eq!(code_after, code_before);
    assert_eq!(after, before, "audit must be byte-identical after recovery");

    // A re-mint after recovery writes no new alias-batch bytes: identical
    // rows shard to identical batches, which dedupe by content address.
    let again = u
        .cmd()
        .args(["snapshot", "--json"])
        .output()
        .expect("snapshot runs");
    assert!(again.status.success());
    let again: serde_json::Value = serde_json::from_slice(&again.stdout).unwrap();
    assert_eq!(again["sequence"], 2);
    assert_eq!(
        again["new_batch_blobs"], 0,
        "unchanged alias shards must dedupe to existing batch blobs"
    );
}
