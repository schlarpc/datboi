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

/// The bare-NAS recovery drill (roadmap.md M1): mint a signed snapshot,
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

    // A no-op refinement sweep records provenance for every data blob
    // (negatives included) — the D50 exit criterion drives this drill.
    let sweep_out = u
        .cmd()
        .args(["sweep", "--json"])
        .output()
        .expect("sweep runs");
    assert!(sweep_out.status.success());
    let sweep: serde_json::Value = serde_json::from_slice(&sweep_out.stdout).unwrap();
    let analyzed = sweep["analyzed"].as_u64().unwrap();
    assert!(analyzed >= 3, "alpha, beta, and the dat blob at minimum");
    assert_eq!(sweep["negative"], analyzed, "noop concludes negative");
    assert_eq!(sweep["queue_remaining"], 0);

    // A second sweep finds nothing new to do — the fixpoint is at rest.
    let sweep2 = u
        .cmd()
        .args(["sweep", "--json"])
        .output()
        .expect("sweep runs");
    let sweep2: serde_json::Value = serde_json::from_slice(&sweep2.stdout).unwrap();
    assert_eq!(sweep2["enqueued"], 0);
    assert_eq!(sweep2["analyzed"], 0);

    // A defined + evaluated view must survive the disaster too: the
    // definition is config KV, the flip is a tag, both ride the payload.
    u.cmd()
        .args(["view", "define", "shelf", "test/drill"])
        .args(["--template", "{name}"])
        .assert()
        .success();
    let eval = u
        .cmd()
        .args(["view", "eval", "shelf", "--json"])
        .output()
        .expect("eval runs");
    let eval: serde_json::Value = serde_json::from_slice(&eval.stdout).unwrap();
    let view_snapshot = eval["snapshot"].as_str().unwrap().to_owned();

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
    let recover_out = u
        .cmd()
        .args(["recover", "--json"])
        .output()
        .expect("recover runs");
    assert!(recover_out.status.success());
    let recovered: serde_json::Value = serde_json::from_slice(&recover_out.stdout).unwrap();
    assert!(recovered["snapshot_seq"].as_u64().is_some());
    assert!(
        recovered["analysis_restored"].as_u64().unwrap() >= analyzed,
        "provenance rows come back from the snapshot batches (D48)"
    );

    let (after, code_after) = audit_json(&u, "test/drill");
    assert_eq!(code_after, code_before);
    assert_eq!(after, before, "audit must be byte-identical after recovery");

    // The view came back: same definition, same tagged snapshot.
    let manifest = u
        .cmd()
        .args(["view", "manifest", "shelf", "--json"])
        .output()
        .expect("manifest runs");
    assert!(manifest.status.success(), "view survives recovery");
    let m: serde_json::Value = serde_json::from_slice(&manifest.stdout).unwrap();
    assert_eq!(m["snapshot"], view_snapshot.as_str(), "same D33 flip");
    let relisted = u
        .cmd()
        .args(["view", "list", "--json"])
        .output()
        .expect("list runs");
    let l: serde_json::Value = serde_json::from_slice(&relisted.stdout).unwrap();
    assert_eq!(l["views"][0]["name"], "shelf");

    // The restored provenance means a fresh sweep re-pays NOTHING for
    // blobs analyzed before the disaster (D48's whole purpose). The
    // snapshot/batch objects minted along the way live in meta/, which is
    // never an analysis candidate, so they don't enqueue either.
    let sweep3 = u
        .cmd()
        .args(["sweep", "--json"])
        .output()
        .expect("sweep runs");
    let sweep3: serde_json::Value = serde_json::from_slice(&sweep3.stdout).unwrap();
    assert_eq!(
        sweep3["analyzed"], 0,
        "no analysis re-paid after recovery: {sweep3}"
    );

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

/// `dat diff` across two imported revisions: added / removed / renamed
/// (fingerprint-matched — no stable keys in this dat) / rehashed, with
/// diff(1)-style exit codes.
#[test]
fn dat_diff_categories() {
    let u = Universe::new();

    let keep = b"unchanged content".as_slice();
    let moved = b"renamed but identical".as_slice();
    let fixed_v1 = b"bad dump".as_slice();
    let fixed_v2 = b"good dump".as_slice();
    let dropped = b"gone in v2".as_slice();
    let fresh = b"new in v2".as_slice();

    let game = |name: &str, rom: &str| -> String {
        format!(r#"<game name="{name}"><description>{name}</description>{rom}</game>"#)
    };

    let v1 = dat_xml(
        "diffy",
        &format!(
            "{}{}{}{}",
            game("Keep", &rom_xml("Keep.gba", keep)),
            game("Old Name", &rom_xml("Old Name.gba", moved)),
            game("Fixed", &rom_xml("Fixed.gba", fixed_v1)),
            game("Dropped", &rom_xml("Dropped.gba", dropped)),
        ),
    );
    let v2 = dat_xml(
        "diffy",
        &format!(
            "{}{}{}{}",
            game("Keep", &rom_xml("Keep.gba", keep)),
            game("New Name", &rom_xml("New Name.gba", moved)),
            game("Fixed", &rom_xml("Fixed.gba", fixed_v2)),
            game("Fresh", &rom_xml("Fresh.gba", fresh)),
        ),
    );

    let import = |xml: &str, tag: &str| {
        let path = u.root.path().join(format!("{tag}.dat"));
        fs::write(&path, xml).unwrap();
        u.cmd()
            .args(["dat", "import"])
            .arg(&path)
            .args(["--provider", "test", "--system", "diffy"])
            .assert()
            .success();
    };

    // Only one revision: diff must refuse clearly.
    import(&v1, "v1");
    u.cmd()
        .args(["dat", "diff", "test/diffy"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("only one materialized revision"));

    import(&v2, "v2");
    let out = u
        .cmd()
        .args(["dat", "diff", "test/diffy", "--json"])
        .output()
        .expect("diff runs");
    assert_eq!(out.status.code(), Some(1), "changes -> exit 1");
    let diff: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(diff["added"], serde_json::json!(["Fresh"]));
    assert_eq!(diff["removed"], serde_json::json!(["Dropped"]));
    assert_eq!(
        diff["renamed"],
        serde_json::json!([{"from": "Old Name", "to": "New Name"}])
    );
    assert_eq!(
        diff["rehashed"],
        serde_json::json!([{"from": "Fixed", "to": "Fixed"}])
    );

    // Re-import v2: identical revisions diff empty, exit 0.
    import(&v2, "v2-again");
    u.cmd()
        .args(["dat", "diff", "test/diffy"])
        .assert()
        .code(0)
        .stdout(predicate::str::contains("no changes"));
}

/// CHD v5 header audit (D44): a stored .chd whose header declares the disk
/// claim's internal sha1 audits as `probable` — evidence, never
/// have-verified, because nothing decompressed the payload to check the
/// declaration.
#[test]
fn chd_header_match_is_probable() {
    let u = Universe::new();
    fs::create_dir_all(u.src()).unwrap();

    // A fake v5 CHD: real header, garbage payload. The declared internal
    // sha1 is what the dat references; the file's own hashes match nothing.
    let internal_sha1 = [0xCD; 20];
    let mut chd = datboi_formats::chd::synth_v5(1 << 20, [0xAB; 20], internal_sha1);
    chd.extend_from_slice(&vec![0x5A; 4096]);
    fs::write(u.src().join("game.chd"), &chd).unwrap();
    u.cmd().arg("ingest").arg(u.src()).assert().success();

    let dat_path = u.root.path().join("disks.dat");
    fs::write(
        &dat_path,
        dat_xml(
            "disks",
            &format!(
                r#"<game name="DiskGame"><description>DiskGame</description><disk name="game" sha1="{}"/></game>"#,
                hex(&internal_sha1),
            ),
        ),
    )
    .unwrap();
    u.cmd()
        .args(["dat", "import"])
        .arg(&dat_path)
        .args(["--provider", "test", "--system", "disks"])
        .assert()
        .success();

    let (audit, code) = audit_json(&u, "test/disks");
    assert_eq!(code, Some(1), "probable is not complete (D39)");
    let t = &audit["totals"];
    assert_eq!(t["required"], 1);
    assert_eq!(t["probable"], 1, "header match must grade as probable");
    assert_eq!(t["have_verified"], 0);
    assert_eq!(t["missing"], 0);
}

/// One-shot HTTP server: accepts a single connection, ignores the request,
/// returns `body` with 200. Returns the URL to fetch.
fn serve_once(body: Vec<u8>, content_type: &'static str) -> String {
    use std::io::Write as _;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut buf = [0u8; 4096];
        let _ = std::io::Read::read(&mut stream, &mut buf); // drain request head
        let head = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        stream.write_all(head.as_bytes()).unwrap();
        stream.write_all(&body).unwrap();
    });
    format!("http://127.0.0.1:{port}/datfile/")
}

/// `dat fetch`: HTTP → CAS → normal import path, for both a zipped dat
/// (the Redump shape) and a bare dat body.
#[test]
fn dat_fetch_imports_zipped_and_bare() {
    let u = Universe::new();
    let dat = dat_xml(
        "fetchy",
        &format!(
            r#"<game name="Net"><description>Net</description>{}</game>"#,
            rom_xml("Net.gba", b"fetched over http")
        ),
    );

    // Redump shape: a zip wrapping the dat.
    let url = serve_once(
        stored_zip(&[("fetchy (2026).dat", dat.as_bytes())]),
        "application/zip",
    );
    u.cmd()
        .args(["dat", "fetch"])
        .arg(&url)
        .args(["--provider", "test", "--system", "zipped"])
        .assert()
        .success()
        .stdout(predicate::str::contains("entries"));
    u.cmd().args(["audit", "test/zipped"]).assert().code(1); // imported; rom missing

    // Real Redump zips DEFLATE their member: build one by hand (local
    // header + deflate stream + central directory) and fetch it.
    let deflated = {
        use std::io::Write as _;
        let raw = dat.as_bytes();
        let mut enc =
            flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(raw).unwrap();
        let cdata = enc.finish().unwrap();
        let crc = {
            let mut h = crc32fast::Hasher::new();
            h.update(raw);
            h.finalize()
        };
        let name = b"fetchy.dat";
        let mut zip = Vec::new();
        zip.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
        zip.extend_from_slice(&20u16.to_le_bytes());
        zip.extend_from_slice(&0u16.to_le_bytes());
        zip.extend_from_slice(&8u16.to_le_bytes()); // DEFLATE
        zip.extend_from_slice(&0u32.to_le_bytes());
        zip.extend_from_slice(&crc.to_le_bytes());
        zip.extend_from_slice(&u32::try_from(cdata.len()).unwrap().to_le_bytes());
        zip.extend_from_slice(&u32::try_from(raw.len()).unwrap().to_le_bytes());
        zip.extend_from_slice(&u16::try_from(name.len()).unwrap().to_le_bytes());
        zip.extend_from_slice(&0u16.to_le_bytes());
        zip.extend_from_slice(name);
        zip.extend_from_slice(&cdata);
        let cd_start = zip.len();
        zip.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
        zip.extend_from_slice(&20u16.to_le_bytes()); // version made by
        zip.extend_from_slice(&20u16.to_le_bytes()); // version needed
        zip.extend_from_slice(&0u16.to_le_bytes());
        zip.extend_from_slice(&8u16.to_le_bytes()); // DEFLATE
        zip.extend_from_slice(&0u32.to_le_bytes());
        zip.extend_from_slice(&crc.to_le_bytes());
        zip.extend_from_slice(&u32::try_from(cdata.len()).unwrap().to_le_bytes());
        zip.extend_from_slice(&u32::try_from(raw.len()).unwrap().to_le_bytes());
        zip.extend_from_slice(&u16::try_from(name.len()).unwrap().to_le_bytes());
        zip.extend_from_slice(&[0u8; 12]); // extra/comment/disk/int+ext attrs
        zip.extend_from_slice(&0u32.to_le_bytes()); // local header offset
        zip.extend_from_slice(name);
        let cd_len = zip.len() - cd_start;
        zip.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
        zip.extend_from_slice(&[0u8; 4]); // disk numbers
        zip.extend_from_slice(&1u16.to_le_bytes());
        zip.extend_from_slice(&1u16.to_le_bytes());
        zip.extend_from_slice(&u32::try_from(cd_len).unwrap().to_le_bytes());
        zip.extend_from_slice(&u32::try_from(cd_start).unwrap().to_le_bytes());
        zip.extend_from_slice(&0u16.to_le_bytes());
        zip
    };
    let url = serve_once(deflated, "application/zip");
    u.cmd()
        .args(["dat", "fetch"])
        .arg(&url)
        .args(["--provider", "test", "--system", "deflated"])
        .assert()
        .success();
    u.cmd().args(["audit", "test/deflated"]).assert().code(1);

    // Bare dat body passes through unchanged.
    let url = serve_once(dat.into_bytes(), "text/xml");
    u.cmd()
        .args(["dat", "fetch"])
        .arg(&url)
        .args(["--provider", "test", "--system", "bare"])
        .assert()
        .success();
    u.cmd().args(["audit", "test/bare"]).assert().code(1);

    // Unknown shorthand is a clear error, not a surprise request.
    u.cmd()
        .args(["dat", "fetch", "nointro/gba"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("redump/<system-slug>"));
}

/// M4 selection + profiles: a 1G1R view over a flat dat (family
/// inference by base name), held-first pick upgrading after ingest, and
/// device-profile name sanitization.
#[test]
fn view_1g1r_and_profile() {
    let u = Universe::new();
    fs::create_dir_all(u.src()).unwrap();
    let usa = b"usa revision of the game".as_slice();
    let europe = b"european revision here!!".as_slice();
    let solo = b"a game with no clones".as_slice();
    // Only Europe + Solo held at first.
    fs::write(u.src().join("europe.gba"), europe).unwrap();
    fs::write(u.src().join("solo.gba"), solo).unwrap();
    u.cmd().arg("ingest").arg(u.src()).assert().success();

    let games = format!(
        r#"  <game name="Game, The (USA)"><description>g</description>{}</game>
  <game name="Game, The (Europe)"><description>g</description>{}</game>
  <game name="Solo (Japan)"><description>s</description>{}</game>"#,
        rom_xml("Game, The (USA).gba", usa),
        rom_xml("Game, The (Europe).gba", europe),
        // illegal-on-FAT characters in the claim name
        rom_xml("solo: the remaster?.gba", solo),
    );
    let dat_path = u.root.path().join("clones.dat");
    fs::write(&dat_path, dat_xml("Clones", &games)).unwrap();
    u.cmd()
        .args(["dat", "import"])
        .arg(&dat_path)
        .args(["--provider", "test", "--system", "clones"])
        .assert()
        .success();

    u.cmd()
        .args(["view", "define", "shelf", "test/clones"])
        .args(["--template", "{name}", "--1g1r"])
        .args(["--regions", "USA,Europe"])
        .args(["--profile", "everdrive"])
        .assert()
        .success();

    // USA is preferred but absent: the held Europe copy wins its family.
    let eval = u
        .cmd()
        .args(["view", "eval", "shelf", "--json"])
        .assert()
        .success();
    let out: serde_json::Value = serde_json::from_slice(&eval.get_output().stdout).expect("json");
    assert_eq!(out["families"], 2);
    assert_eq!(out["rows"], 2);
    let manifest = u
        .cmd()
        .args(["view", "manifest", "shelf", "--json"])
        .assert()
        .success();
    let m: serde_json::Value = serde_json::from_slice(&manifest.get_output().stdout).expect("json");
    let paths: Vec<&str> = m["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["path"].as_str().unwrap())
        .collect();
    assert!(
        paths.contains(&"Game, The (Europe).gba"),
        "held Europe beats absent USA: {paths:?}"
    );
    assert!(
        paths.contains(&"solo_ the remaster_.gba"),
        "profile scrubs FAT-hostile chars: {paths:?}"
    );

    // Ingest USA and re-evaluate: the pick upgrades to the preferred
    // region — that's what re-eval is for.
    fs::write(u.src().join("usa.gba"), usa).unwrap();
    u.cmd().arg("ingest").arg(u.src()).assert().success();
    u.cmd().args(["view", "eval", "shelf"]).assert().success();
    let manifest = u
        .cmd()
        .args(["view", "manifest", "shelf", "--json"])
        .assert()
        .success();
    let m: serde_json::Value = serde_json::from_slice(&manifest.get_output().stdout).expect("json");
    let paths: Vec<&str> = m["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["path"].as_str().unwrap())
        .collect();
    assert!(
        paths.contains(&"Game, The (USA).gba"),
        "preferred region wins once held: {paths:?}"
    );
    assert!(!paths.iter().any(|p| p.contains("Europe")), "{paths:?}");

    // Unknown profile is rejected at definition time.
    u.cmd()
        .args(["view", "define", "bad", "test/clones"])
        .args(["--profile", "betamax"])
        .assert()
        .code(2);
}

/// M4 SD sync: initial write, incremental no-op, holdings change +
/// re-eval + re-sync updates in place, --delete clears extraneous.
#[test]
fn view_sync_incremental() {
    let u = Universe::new();
    fs::create_dir_all(u.src()).unwrap();
    let alpha = b"alpha rom content".as_slice();
    let beta = b"beta rom content!".as_slice();
    fs::write(u.src().join("alpha.gba"), alpha).unwrap();
    u.cmd().arg("ingest").arg(u.src()).assert().success();

    let games = format!(
        r#"  <game name="Alpha"><description>a</description>{}</game>
  <game name="Beta"><description>b</description>{}</game>"#,
        rom_xml("alpha.gba", alpha),
        rom_xml("beta.gba", beta),
    );
    let dat_path = u.root.path().join("sync.dat");
    fs::write(&dat_path, dat_xml("Sync", &games)).unwrap();
    u.cmd()
        .args(["dat", "import"])
        .arg(&dat_path)
        .args(["--provider", "test", "--system", "sync"])
        .assert()
        .success();
    u.cmd()
        .args(["view", "define", "card", "test/sync"])
        .args(["--template", "{entry}/{name}"])
        .assert()
        .success();
    u.cmd().args(["view", "eval", "card"]).assert().success();

    let card = u.root.path().join("sdcard");

    // dry-run touches nothing
    let out = u
        .cmd()
        .args(["view", "sync", "card"])
        .arg(&card)
        .args(["--dry-run", "--json"])
        .assert()
        .success();
    let v: serde_json::Value = serde_json::from_slice(&out.get_output().stdout).unwrap();
    assert_eq!(v["written"], 1);
    assert!(!card.exists(), "dry-run must not create the target");

    // first sync writes; second is a no-op
    u.cmd()
        .args(["view", "sync", "card"])
        .arg(&card)
        .assert()
        .success();
    assert_eq!(fs::read(card.join("Alpha/alpha.gba")).unwrap(), alpha);
    let out = u
        .cmd()
        .args(["view", "sync", "card"])
        .arg(&card)
        .args(["--json"])
        .assert()
        .success();
    let v: serde_json::Value = serde_json::from_slice(&out.get_output().stdout).unwrap();
    assert_eq!(
        (v["written"].as_u64(), v["skipped"].as_u64()),
        (Some(0), Some(1))
    );

    // --verify catches silent same-size corruption on the card
    fs::write(card.join("Alpha/alpha.gba"), b"XXXXX rom content").unwrap();
    let out = u
        .cmd()
        .args(["view", "sync", "card"])
        .arg(&card)
        .args(["--verify", "--json"])
        .assert()
        .success();
    let v: serde_json::Value = serde_json::from_slice(&out.get_output().stdout).unwrap();
    assert_eq!(v["written"], 1, "corrupt file rewritten");
    assert_eq!(fs::read(card.join("Alpha/alpha.gba")).unwrap(), alpha);

    // new holdings + re-eval + sync --delete: Beta arrives, junk leaves
    fs::write(u.src().join("beta.gba"), beta).unwrap();
    u.cmd().arg("ingest").arg(u.src()).assert().success();
    u.cmd().args(["view", "eval", "card"]).assert().success();
    fs::create_dir_all(card.join("stale")).unwrap();
    fs::write(card.join("stale/junk.bin"), b"junk").unwrap();
    let out = u
        .cmd()
        .args(["view", "sync", "card"])
        .arg(&card)
        .args(["--delete", "--json"])
        .assert()
        .success();
    let v: serde_json::Value = serde_json::from_slice(&out.get_output().stdout).unwrap();
    assert_eq!(v["written"], 1, "beta written");
    assert_eq!(v["deleted"], 1, "junk removed");
    assert_eq!(fs::read(card.join("Beta/beta.gba")).unwrap(), beta);
    assert!(!card.join("stale").exists(), "emptied dir pruned");
}

/// M4 views: define → eval → manifest over a real ingest+dat, with the
/// D33 tag flip and a re-eval producing a new snapshot after holdings
/// change.
#[test]
fn view_define_eval_manifest() {
    let u = Universe::new();
    fs::create_dir_all(u.src()).unwrap();
    let alpha = b"alpha rom content".as_slice();
    let beta = b"beta rom content!".as_slice();
    fs::write(u.src().join("alpha.gba"), alpha).unwrap();
    u.cmd().arg("ingest").arg(u.src()).assert().success();

    let games = format!(
        r#"  <game name="Alpha"><description>Alpha</description>{}</game>
  <game name="Beta"><description>Beta</description>{}</game>"#,
        rom_xml("alpha.gba", alpha),
        rom_xml("beta.gba", beta),
    );
    let dat_path = u.root.path().join("fixture.dat");
    fs::write(&dat_path, dat_xml("Test GBA", &games)).unwrap();
    u.cmd()
        .args(["dat", "import"])
        .arg(&dat_path)
        .args(["--provider", "test", "--system", "gba"])
        .assert()
        .success();

    u.cmd()
        .args(["view", "define", "everdrive", "test/gba"])
        .args(["--template", "{entry}/{name}"])
        .assert()
        .success();

    // Only alpha is held: one row, one missing claim.
    let eval = u
        .cmd()
        .args(["view", "eval", "everdrive", "--json"])
        .assert()
        .success();
    let out: serde_json::Value = serde_json::from_slice(&eval.get_output().stdout).expect("json");
    assert_eq!(out["rows"], 1);
    assert_eq!(out["missing_claims"], 1);
    let snap1 = out["snapshot"].as_str().unwrap().to_owned();

    let manifest = u
        .cmd()
        .args(["view", "manifest", "everdrive", "--json"])
        .assert()
        .success();
    let m: serde_json::Value = serde_json::from_slice(&manifest.get_output().stdout).expect("json");
    assert_eq!(m["snapshot"], snap1.as_str());
    assert_eq!(m["rows"][0]["path"], "Alpha/alpha.gba");
    assert_eq!(m["rows"][0]["seek"], 0, "resident literal reads affinely");

    // Ingest beta; re-eval flips the tag to a NEW snapshot with 2 rows.
    fs::write(u.src().join("beta.gba"), beta).unwrap();
    u.cmd().arg("ingest").arg(u.src()).assert().success();
    let eval2 = u
        .cmd()
        .args(["view", "eval", "everdrive", "--json"])
        .assert()
        .success();
    let out2: serde_json::Value = serde_json::from_slice(&eval2.get_output().stdout).expect("json");
    assert_eq!(out2["rows"], 2);
    assert_eq!(out2["missing_claims"], 0);
    assert_ne!(
        out2["snapshot"],
        snap1.as_str(),
        "D33: new snapshot, tag flipped"
    );

    let list = u.cmd().args(["view", "list", "--json"]).assert().success();
    let l: serde_json::Value = serde_json::from_slice(&list.get_output().stdout).expect("json");
    assert_eq!(l["views"][0]["name"], "everdrive");
    assert_eq!(l["views"][0]["snapshot"], out2["snapshot"]);
}

/// D62 end to end through the real binary: define --image, eval,
/// `view image --out`, then fsck the exported bytes and re-mint for
/// idempotence. The clobber warning must always appear.
#[test]
fn view_image_end_to_end() {
    let u = Universe::new();
    fs::create_dir_all(u.src()).unwrap();
    let alpha = b"alpha rom content".as_slice();
    let beta = b"beta rom content, somewhat longer".as_slice();
    fs::write(u.src().join("alpha.gba"), alpha).unwrap();
    fs::write(u.src().join("beta.gba"), beta).unwrap();
    u.cmd().arg("ingest").arg(u.src()).assert().success();

    let games = format!(
        r#"  <game name="Alpha"><description>Alpha</description>{}</game>
  <game name="Beta"><description>Beta</description>{}</game>"#,
        rom_xml("alpha.gba", alpha),
        rom_xml("beta.gba", beta),
    );
    let dat_path = u.root.path().join("fixture.dat");
    fs::write(&dat_path, dat_xml("Test GBA", &games)).unwrap();
    u.cmd()
        .args(["dat", "import"])
        .arg(&dat_path)
        .args(["--provider", "test", "--system", "gba"])
        .assert()
        .success();

    u.cmd()
        .args(["view", "define", "card", "test/gba"])
        .args(["--template", "{name}", "--profile", "fat32"])
        .args(["--image", "--image-cluster-size", "512"])
        .assert()
        .success()
        .stdout(predicate::str::contains("image fat32"));
    u.cmd().args(["view", "eval", "card"]).assert().success();

    let img_path = u.root.path().join("card.img");
    let mint = u
        .cmd()
        .args(["view", "image", "card", "--json", "--out"])
        .arg(&img_path)
        .assert()
        .success()
        .stderr(predicate::str::contains("CLOBBERS on-device saves"));
    let m: serde_json::Value = serde_json::from_slice(&mint.get_output().stdout).expect("json");
    assert_eq!(m["rows"], 2);
    assert_eq!(m["obao_stored"], true);
    let size = m["size"].as_u64().expect("size");
    assert_eq!(fs::metadata(&img_path).expect("exported").len(), size);

    // The MBR default: partition table present at sector 0.
    let image = fs::read(&img_path).expect("read image");
    assert_eq!(&image[510..512], &[0x55, 0xAA]);
    assert_eq!(image[0x1BE + 4], 0x0C, "FAT32-LBA partition");

    // fsck the filesystem itself (fsck.vfat takes no offset: hand it
    // the partition slice). Skips gracefully when fsck.vfat is absent;
    // nix CI enforces via DATBOI_REQUIRE_FSCK.
    let slice_path = u.root.path().join("partition.img");
    fs::write(&slice_path, &image[1 << 20..]).expect("write slice");
    match Command::new("fsck.vfat")
        .arg("-n")
        .arg(&slice_path)
        .output()
    {
        Ok(out) => assert!(
            out.status.success(),
            "fsck.vfat: {}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            assert!(
                std::env::var_os("DATBOI_REQUIRE_FSCK").is_none_or(|v| v != "1"),
                "DATBOI_REQUIRE_FSCK=1 but fsck.vfat is not installed"
            );
            eprintln!("skipping fsck.vfat (not installed)");
        }
        Err(e) => panic!("running fsck.vfat: {e}"),
    }

    // Re-mint without eval: content-addressed no-op, same image hash.
    let remint = u
        .cmd()
        .args(["view", "image", "card", "--json"])
        .assert()
        .success();
    let m2: serde_json::Value = serde_json::from_slice(&remint.get_output().stdout).expect("json");
    assert_eq!(m2["image"], m["image"]);
    assert_eq!(m2["recipe"], m["recipe"]);
}

/// D60: per-analyzer enable/disable + opaque params in the config KV,
/// enforced at the sweep entry point.
#[test]
fn analyzer_policy_gates_sweeps() {
    let u = Universe::new();
    fs::create_dir_all(u.src()).unwrap();
    fs::write(u.src().join("blob.bin"), b"some bytes").unwrap();
    u.cmd().arg("ingest").arg(u.src()).assert().success();

    // Default: enabled; a noop sweep runs.
    u.cmd()
        .args(["sweep", "--analyzer", "noop"])
        .assert()
        .success();

    // Disable → the sweep is a no-op with a pointed message + exit 1.
    u.cmd()
        .args(["analyzer", "disable", "noop"])
        .assert()
        .success();
    u.cmd()
        .args(["sweep", "--analyzer", "noop"])
        .assert()
        .code(1)
        .stdout(predicate::str::contains("is disabled:"));

    // List reflects the state.
    let list = u
        .cmd()
        .args(["analyzer", "list", "--json"])
        .assert()
        .success();
    let l: serde_json::Value = serde_json::from_slice(&list.get_output().stdout).expect("json");
    let noop = l["analyzers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["family"] == "noop")
        .expect("noop row");
    assert_eq!(noop["enabled"], false);

    // Params round-trip (opaque; hex on the wire).
    u.cmd()
        .args(["analyzer", "set-params", "chunk", "deadbeef"])
        .assert()
        .success();
    let list = u
        .cmd()
        .args(["analyzer", "list", "--json"])
        .assert()
        .success();
    let l: serde_json::Value = serde_json::from_slice(&list.get_output().stdout).expect("json");
    let chunk = l["analyzers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["family"] == "chunk")
        .expect("chunk row");
    assert_eq!(chunk["params_hex"], "deadbeef");
    u.cmd()
        .args(["analyzer", "clear-params", "chunk"])
        .assert()
        .success();

    // Re-enable → sweeps run again.
    u.cmd()
        .args(["analyzer", "enable", "noop"])
        .assert()
        .success();
    u.cmd()
        .args(["sweep", "--analyzer", "noop"])
        .assert()
        .success();

    // Unknown family: clean refusal.
    u.cmd()
        .args(["analyzer", "disable", "nonesuch"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("unknown analyzer family"));
}

/// D57 end to end: strict mode picks the absent preferred region (its
/// gap is the want list); a linked retool clonelist merges a rename
/// into the family in both modes.
#[test]
fn strict_1g1r_and_clonelist() {
    let u = Universe::new();
    fs::create_dir_all(u.src()).unwrap();
    let europe = b"the european bytes here".as_slice();
    let renamed = b"the japanese rename!!!!".as_slice();
    fs::write(u.src().join("europe.gba"), europe).unwrap();
    fs::write(u.src().join("renamed.gba"), renamed).unwrap();
    u.cmd().arg("ingest").arg(u.src()).assert().success();

    let games = format!(
        r#"  <game name="Game, The (USA)"><description>g</description>{}</game>
  <game name="Game, The (Europe)"><description>g</description>{}</game>
  <game name="Gamu za Best (Japan)"><description>g</description>{}</game>"#,
        rom_xml("Game, The (USA).gba", b"usa bytes never ingested"),
        rom_xml("Game, The (Europe).gba", europe),
        rom_xml("Gamu za Best (Japan).gba", renamed),
    );
    let dat_path = u.root.path().join("strict.dat");
    fs::write(&dat_path, dat_xml("Strict", &games)).unwrap();
    u.cmd()
        .args(["dat", "import"])
        .arg(&dat_path)
        .args(["--provider", "test", "--system", "strict"])
        .assert()
        .success();

    // The clonelist merges the JP rename into the family.
    let clonelist = r#"{"variants": [{"group": "Game, The", "titles": [
        {"searchTerm": "Game, The"},
        {"searchTerm": "Gamu za Best"}
    ]}]}"#;
    let cl_path = u.root.path().join("clones.json");
    fs::write(&cl_path, clonelist).unwrap();
    u.cmd()
        .args(["dat", "clonelist", "test/strict"])
        .arg(&cl_path)
        .assert()
        .success()
        .stdout(predicate::str::contains("2 term(s)"));

    // Held-first: USA absent, Europe held -> Europe serves; the
    // clonelist keeps the JP rename OUT (one family, one row).
    u.cmd()
        .args(["view", "define", "held", "test/strict"])
        .args([
            "--template",
            "{name}",
            "--1g1r",
            "--regions",
            "USA,Europe,Japan",
        ])
        .assert()
        .success();
    let eval = u
        .cmd()
        .args(["view", "eval", "held", "--json"])
        .assert()
        .success();
    let out: serde_json::Value = serde_json::from_slice(&eval.get_output().stdout).expect("json");
    assert_eq!(out["families"], 1, "clonelist merged the rename");
    assert_eq!(out["rows"], 1);
    let manifest = u
        .cmd()
        .args(["view", "manifest", "held", "--json"])
        .assert()
        .success();
    let m: serde_json::Value = serde_json::from_slice(&manifest.get_output().stdout).expect("json");
    assert_eq!(m["rows"][0]["path"], "Game, The (Europe).gba");

    // Strict: USA wins even though absent -> zero rows; the gap shows
    // as a missing claim, not a silently different pick.
    u.cmd()
        .args(["view", "define", "pure", "test/strict"])
        .args(["--template", "{name}", "--1g1r", "--strict"])
        .args(["--regions", "USA,Europe,Japan"])
        .assert()
        .success();
    let eval = u
        .cmd()
        .args(["view", "eval", "pure", "--json"])
        .assert()
        .success();
    let out: serde_json::Value = serde_json::from_slice(&eval.get_output().stdout).expect("json");
    assert_eq!(out["families"], 1);
    assert_eq!(out["rows"], 0, "strict renders the absent winner as a gap");

    // --strict requires --1g1r.
    u.cmd()
        .args(["view", "define", "bad", "test/strict"])
        .args(["--template", "{name}", "--strict"])
        .assert()
        .failure();
}

/// D31 deferred set, end to end: one synthetic listxml (bios machine,
/// parent with merge-tagged bios rom + device_refs, clone with
/// merge-tagged parent rom, a device machine, one dangling ref)
/// rendered in all three merge modes.
#[test]
fn mame_merge_modes() {
    let u = Universe::new();
    fs::create_dir_all(u.src()).unwrap();
    let bios = b"neogeo bios rom bytes".as_slice();
    let parent = b"parent game rom bytes".as_slice();
    let child = b"clone-only rom bytes!".as_slice();
    let z80 = b"z80 device rom bytes!".as_slice();
    for (name, data) in [
        ("bios.rom", bios),
        ("parent.rom", parent),
        ("child.rom", child),
        ("z80.rom", z80),
    ] {
        fs::write(u.src().join(name), data).unwrap();
    }
    u.cmd().arg("ingest").arg(u.src()).assert().success();

    fn mame_rom(name: &str, content: &[u8], merge: Option<&str>) -> String {
        let mut hasher = AliasHasher::new();
        hasher.update(content);
        let t = hasher.finalize();
        let merge = merge.map_or(String::new(), |m| format!(r#" merge="{m}""#));
        format!(
            r#"<rom name="{name}"{merge} size="{}" crc="{}" sha1="{}"/>"#,
            t.size,
            hex(&t.crc32),
            hex(&t.sha1),
        )
    }
    let listxml = format!(
        r#"<?xml version="1.0"?>
<mame build="0.270">
  <machine name="neogeo" isbios="yes" runnable="no">
    <description>BIOS</description>
    {bios_rom}
  </machine>
  <machine name="parent" romof="neogeo">
    <description>Parent</description>
    {bios_merge}
    {parent_rom}
    <device_ref name="z80"/>
    <device_ref name="missing_dev"/>
  </machine>
  <machine name="child" cloneof="parent" romof="parent">
    <description>Child</description>
    {parent_merge}
    {child_rom}
    <device_ref name="z80"/>
  </machine>
  <machine name="z80" isdevice="yes" runnable="no">
    <description>Z80</description>
    {z80_rom}
  </machine>
</mame>
"#,
        bios_rom = mame_rom("bios.rom", bios, None),
        bios_merge = mame_rom("bios.rom", bios, Some("bios.rom")),
        parent_rom = mame_rom("parent.rom", parent, None),
        parent_merge = mame_rom("parent.rom", parent, Some("parent.rom")),
        child_rom = mame_rom("child.rom", child, None),
        z80_rom = mame_rom("z80.rom", z80, None),
    );
    let dat_path = u.root.path().join("mame.xml");
    fs::write(&dat_path, listxml).unwrap();
    u.cmd()
        .args(["dat", "import"])
        .arg(&dat_path)
        .args(["--provider", "mame", "--system", "arcade"])
        .assert()
        .success();

    let manifest_paths = |name: &str| -> Vec<String> {
        let manifest = u
            .cmd()
            .args(["view", "manifest", name, "--json"])
            .assert()
            .success();
        let m: serde_json::Value =
            serde_json::from_slice(&manifest.get_output().stdout).expect("json");
        m["rows"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["path"].as_str().unwrap().to_owned())
            .collect()
    };

    // Non-merged: standalone sets with device closure; no z80 set; the
    // dangling ref is counted, not fatal.
    u.cmd()
        .args(["view", "define", "full", "mame/arcade"])
        .args(["--template", "{entry}/{name}", "--mame-mode", "non-merged"])
        .assert()
        .success()
        .stdout(predicate::str::contains("mame non-merged"));
    let eval = u
        .cmd()
        .args(["view", "eval", "full", "--json"])
        .assert()
        .success();
    let out: serde_json::Value = serde_json::from_slice(&eval.get_output().stdout).expect("json");
    assert_eq!(out["dangling_device_refs"], 1);
    let mut paths = manifest_paths("full");
    paths.sort();
    assert_eq!(
        paths,
        vec![
            "child/child.rom",
            "child/parent.rom",
            "child/z80.rom",
            "neogeo/bios.rom",
            "parent/bios.rom",
            "parent/parent.rom",
            "parent/z80.rom",
        ],
        "standalone sets incl. inherited + device roms; no z80 set"
    );

    // Split: own roms only; devices and bios are their own sets.
    u.cmd()
        .args(["view", "define", "split", "mame/arcade"])
        .args(["--template", "{entry}/{name}", "--mame-mode", "split"])
        .assert()
        .success();
    u.cmd().args(["view", "eval", "split"]).assert().success();
    let mut paths = manifest_paths("split");
    paths.sort();
    assert_eq!(
        paths,
        vec![
            "child/child.rom",
            "neogeo/bios.rom",
            "parent/parent.rom",
            "z80/z80.rom",
        ],
        "merge-tagged roms live only in their parent sets"
    );

    // Merged: the clone folds into the parent set.
    u.cmd()
        .args(["view", "define", "merged", "mame/arcade"])
        .args(["--template", "{entry}/{name}", "--mame-mode", "merged"])
        .assert()
        .success();
    u.cmd().args(["view", "eval", "merged"]).assert().success();
    let mut paths = manifest_paths("merged");
    paths.sort();
    assert_eq!(
        paths,
        vec![
            "neogeo/bios.rom",
            "parent/child.rom",
            "parent/parent.rom",
            "z80/z80.rom",
        ],
        "clone roms fold into the parent set"
    );

    // Guardrails: mame mode on a non-listxml source refuses at eval;
    // --mame-mode + --1g1r refuses at parse time.
    u.cmd()
        .args(["view", "define", "bad", "mame/arcade"])
        .args(["--mame-mode", "merged", "--1g1r"])
        .assert()
        .failure();
    u.cmd()
        .args(["view", "define", "worse", "mame/arcade"])
        .args(["--mame-mode", "zipped"])
        .assert()
        .code(2);
}

/// The auth CLI surface (D30/D68) against the database directly: mint
/// an invite, accept it (the accessor the daemon endpoint calls — no
/// daemon in a CLI test), then drive grants, tokens, and sessions
/// through the CLI and watch the listings change.
#[test]
fn auth_cli_surface() {
    let u = Universe::new();

    // Mint an owner invite; the URL carries the token in the FRAGMENT
    // (never sent to servers, so never logged).
    let out = u
        .cmd()
        .args(["user", "invite", "--owner", "--expires-days", "3", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&out).expect("json");
    assert_eq!(v["role"], "owner");
    let token = v["token"].as_str().expect("token").to_owned();
    assert_eq!(
        v["url"].as_str().expect("url"),
        format!("http://127.0.0.1:2352/invite#{token}")
    );

    // No users yet — the invite is minted, not accepted.
    let out = u
        .cmd()
        .args(["user", "list", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&out).expect("json");
    assert_eq!(v["users"].as_array().expect("array").len(), 0);

    // Accept via the same accessor the daemon endpoint uses.
    {
        let db = datboi_index::Db::open(&u.db()).expect("open");
        let outcome = db
            .accept_invite(
                &datboi_server::auth::token_hash(&token),
                "mika",
                "$argon2id$fake-for-cli-test$",
                1_000,
            )
            .expect("accept");
        assert!(
            matches!(
                outcome,
                datboi_index::InviteOutcome::Accepted {
                    role: datboi_index::Role::Owner,
                    ..
                }
            ),
            "{outcome:?}"
        );
    }

    // user list shows the account with its role and grant count.
    u.cmd()
        .args(["user", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("mika  owner  0 grant(s)"));

    // grant / revoke (unknown view warns but records)
    u.cmd()
        .args(["user", "grant", "mika", "gba"])
        .assert()
        .success()
        .stderr(predicate::str::contains("no view named"));
    u.cmd()
        .args(["user", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("1 grant(s)"));
    u.cmd()
        .args(["user", "revoke", "mika", "gba"])
        .assert()
        .success();
    u.cmd()
        .args(["user", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("0 grant(s)"));

    // token: printed once; the session shows up and revokes by user
    let out = u
        .cmd()
        .args(["token", "--user", "mika", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&out).expect("json");
    assert_eq!(v["username"], "mika");
    assert_eq!(v["token"].as_str().expect("token").len(), 43);
    u.cmd()
        .args(["session", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("mika"));
    u.cmd()
        .args(["session", "revoke", "mika", "--json"])
        .assert()
        .success()
        .stdout(predicate::str::contains(r#""revoked":1"#));
    u.cmd()
        .args(["session", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("no sessions"));

    // unknown users are runtime errors (exit 2, house convention)
    u.cmd()
        .args(["token", "--user", "nobody"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("no such user"));
}

/// D74 CLI wiring: mutating commands stamp terminal ledger rows (the
/// ledger_stamp exhaustive match in main.rs); reads don't. Kinds and
/// states are the datboi-index enums.
#[test]
fn cli_commands_stamp_the_job_ledger() {
    use datboi_index::{JobKind, JobState};

    let world = Universe::new();
    let rom = world.src().join("thing.gba");
    std::fs::create_dir_all(world.src()).expect("src dir");
    std::fs::write(&rom, b"ledger-worthy bytes").expect("rom");

    world.cmd().args(["ingest"]).arg(&rom).assert().success();
    world
        .cmd()
        .args(["sweep", "--analyzer", "noop"])
        .assert()
        .success();
    world.cmd().args(["scrub"]).assert().success();
    // A read must NOT stamp.
    world.cmd().args(["status"]).assert().success();

    let db = datboi_index::Db::open(&world.db()).expect("db");
    let rows = db.recent_jobs(100).expect("rows");
    let kinds: Vec<JobKind> = rows.iter().map(|r| r.kind).collect();
    assert_eq!(
        kinds,
        [JobKind::Ingest, JobKind::Refine, JobKind::Scrub],
        "exactly the three mutating commands stamped: {rows:?}"
    );
    assert!(rows.iter().all(|r| r.state == JobState::Done), "{rows:?}");
    assert!(
        rows[0].name.starts_with("cli: ingest"),
        "names carry the cli: prefix: {rows:?}"
    );
    assert!(
        rows.iter().all(|r| r.finished_at.is_some()),
        "terminal-only rows, never running: {rows:?}"
    );
}
