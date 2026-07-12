//! End-to-end web ingest: staged streaming uploads → background job →
//! polled report, against a live daemon over an empty universe.
//! Loopback is implicitly owner (D68); friend 403s are unit territory
//! (api.rs convention).

use std::net::SocketAddr;
use std::str::FromStr as _;
use std::time::{Duration, Instant};

use datboi_index::Db;
use datboi_server::{Config, Server};
use datboi_store_fs::Store;

struct Fixture {
    addr: SocketAddr,
    store_root: std::path::PathBuf,
    db_dir: std::path::PathBuf,
    _root: tempfile::TempDir,
}

fn fixture() -> Fixture {
    // Most tests want a quiescent database: no background analyzer
    // perturbing counts mid-assertion.
    fixture_with_refine(false)
}

fn fixture_with_refine(refine: bool) -> Fixture {
    let root = tempfile::tempdir().expect("tempdir");
    let store_root = root.path().join("store");
    let db_dir = root.path().join("db");
    // The daemon opens (and creates) its own handles; touching them
    // here first just mirrors the api.rs fixture shape.
    drop(Store::open(&store_root).expect("store"));
    drop(Db::open(&db_dir).expect("db"));
    let server = Server::bind(&Config {
        store_root: store_root.clone(),
        db_dir: db_dir.clone(),
        listen: SocketAddr::from_str("127.0.0.1:0").expect("addr"),
        nfs_listen: None,
        detectors_dir: None,
        refine,
    })
    .expect("bind");
    let addr = server.local_addr().expect("addr");
    std::thread::spawn(move || server.serve());
    Fixture {
        addr,
        store_root,
        db_dir,
        _root: root,
    }
}

/// GET; retries briefly while the accept loop warms up.
fn get(addr: SocketAddr, path: &str) -> (u16, serde_json::Value) {
    let agent = ureq::AgentBuilder::new().redirects(0).build();
    let req = agent.get(&format!("http://{addr}{path}"));
    for _ in 0..50 {
        match req.clone().call() {
            Ok(resp) => return (resp.status(), parse(resp)),
            Err(ureq::Error::Status(code, resp)) => return (code, parse(resp)),
            Err(ureq::Error::Transport(_)) => {
                std::thread::sleep(Duration::from_millis(40));
            }
        }
    }
    panic!("server never came up at {addr}");
}

/// POST raw bytes with a Content-Length (the sized upload path).
fn post_bytes(addr: SocketAddr, path: &str, body: &[u8]) -> (u16, serde_json::Value) {
    let agent = ureq::AgentBuilder::new().redirects(0).build();
    let req = agent
        .post(&format!("http://{addr}{path}"))
        .set("Content-Type", "application/octet-stream");
    match req.send_bytes(body) {
        Ok(resp) => (resp.status(), parse(resp)),
        Err(ureq::Error::Status(code, resp)) => (code, parse(resp)),
        Err(e) => panic!("POST {path} transport error: {e}"),
    }
}

/// POST from a reader with no Content-Length — ureq sends chunked,
/// exercising the headroom-guard bypass and the stream writer.
fn post_chunked(addr: SocketAddr, path: &str, body: Vec<u8>) -> (u16, serde_json::Value) {
    let agent = ureq::AgentBuilder::new().redirects(0).build();
    let req = agent
        .post(&format!("http://{addr}{path}"))
        .set("Content-Type", "application/octet-stream");
    match req.send(std::io::Cursor::new(body)) {
        Ok(resp) => (resp.status(), parse(resp)),
        Err(ureq::Error::Status(code, resp)) => (code, parse(resp)),
        Err(e) => panic!("POST {path} transport error: {e}"),
    }
}

fn post_json(addr: SocketAddr, path: &str, body: &str) -> (u16, serde_json::Value) {
    let agent = ureq::AgentBuilder::new().redirects(0).build();
    let req = agent
        .post(&format!("http://{addr}{path}"))
        .set("Content-Type", "application/json");
    match req.send_string(body) {
        Ok(resp) => (resp.status(), parse(resp)),
        Err(ureq::Error::Status(code, resp)) => (code, parse(resp)),
        Err(e) => panic!("POST {path} transport error: {e}"),
    }
}

fn parse(resp: ureq::Response) -> serde_json::Value {
    let text = resp.into_string().expect("body");
    serde_json::from_str(&text).unwrap_or(serde_json::Value::String(text))
}

/// Poll the job until it leaves `running` (bounded — ingest of a few
/// KB must not take ten seconds).
fn wait_done(addr: SocketAddr, job: i64) -> serde_json::Value {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let (status, v) = get(addr, &format!("/v1/jobs/{job}"));
        assert_eq!(status, 200, "{v}");
        if v["state"] != "running" {
            return v;
        }
        assert!(Instant::now() < deadline, "job never finished: {v}");
        std::thread::sleep(Duration::from_millis(50));
    }
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

#[test]
fn upload_ingest_and_report_end_to_end() {
    let f = fixture();
    let rom = b"alpha rom content";
    let zip = stored_zip(&[("inner.gba", b"zip member content" as &[u8])]);

    // Stage a loose ROM (sized) and a zip (chunked, no Content-Length).
    let (status, v) = post_bytes(f.addr, "/v1/ingest/uploads?name=roms%2Falpha.gba", rom);
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["bytes"], rom.len() as u64);
    let rom_token = v["upload"].as_str().expect("token").to_owned();

    let (status, v) = post_chunked(
        f.addr,
        "/v1/ingest/uploads?name=roms%2Fpack.zip",
        zip.clone(),
    );
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["bytes"], zip.len() as u64);
    let zip_token = v["upload"].as_str().expect("token").to_owned();

    // Both staged files exist until the job spends them.
    let tmp = f.store_root.join("tmp");
    assert_eq!(std::fs::read_dir(&tmp).expect("tmp").count(), 2);

    // Start the job.
    let (status, v) = post_json(
        f.addr,
        "/v1/ingest",
        &format!(r#"{{"uploads":["{rom_token}","{zip_token}"]}}"#),
    );
    assert_eq!(status, 200, "{v}");
    let job = v["job"].as_i64().expect("job id");

    // Tokens are spent all-or-nothing: reuse refuses.
    let (status, v) = post_json(
        f.addr,
        "/v1/ingest",
        &format!(r#"{{"uploads":["{rom_token}"]}}"#),
    );
    assert_eq!(status, 400, "{v}");

    let done = wait_done(f.addr, job);
    assert_eq!(done["state"], "done", "{done}");
    assert_eq!(done["progress"], 100);
    assert_eq!(done["files_total"], 2);
    assert_eq!(done["files_done"], 2);
    assert_eq!(done["bytes_done"], (rom.len() + zip.len()) as u64);
    assert!(done["finished_at"].is_i64(), "{done}");
    assert!(done.get("current").is_none(), "finished: no current file");
    let report = &done["report"];
    assert_eq!(report["files_scanned"], 2);
    assert_eq!(report["files_stored"], 2, "{report}");
    assert_eq!(report["members_claimed"], 1, "the zip member");
    assert_eq!(report["errors"], serde_json::json!([]));
    assert_eq!(report["member_skips"], serde_json::json!([]));
    // No dat imported: nothing to satisfy, and the report says so
    // rather than omitting the section.
    assert_eq!(done["matched"], serde_json::json!([]));
    assert_eq!(done["matched_total"], 0, "{done}");

    // The tray row: finished job, honest name, 100%.
    let (status, v) = get(f.addr, "/v1/jobs");
    assert_eq!(status, 200);
    let row = &v["jobs"][0];
    assert_eq!(row["id"], job);
    assert_eq!(row["name"], "ingest — 2 files");
    assert_eq!(
        (row["progress"].as_u64(), row["state"].as_str()),
        (Some(100), Some("done"))
    );

    // The bytes landed: rom + zip container + the claimed member's
    // index row (the container holds its bytes, D35 — but the member
    // is a first-class blob row).
    let (_, v) = get(f.addr, "/v1/storage");
    assert_eq!(v["blob_count"], 3, "{v}");

    // Staged copies are spent — tmp/ is empty again.
    assert_eq!(std::fs::read_dir(&tmp).expect("tmp").count(), 0);

    // Re-uploading the same ROM dedupes as already-present.
    let (_, v) = post_bytes(f.addr, "/v1/ingest/uploads?name=again%2Falpha.gba", rom);
    let token = v["upload"].as_str().expect("token").to_owned();
    let (_, v) = post_json(
        f.addr,
        "/v1/ingest",
        &format!(r#"{{"uploads":["{token}"]}}"#),
    );
    let done = wait_done(f.addr, v["job"].as_i64().expect("id"));
    assert_eq!(done["report"]["files_already_present"], 1, "{done}");
    assert_eq!(done["report"]["files_stored"], 0);
    let (_, v) = get(f.addr, "/v1/storage");
    assert_eq!(v["blob_count"], 3, "no new blob");
}

/// The regression this pins: ingest must finish the thought — link new
/// blobs to identities and refresh the D39 rollups — not leave a
/// hash-matching upload dark until an unrelated dat import/view eval
/// happens by.
#[test]
fn ingested_member_lights_up_the_shelf() {
    let f = fixture();
    const MEMBER: &[u8] = b"mario kart rom bytes";

    // A one-claim dat whose hashes match MEMBER exactly.
    let mut hasher = datboi_core::alias::AliasHasher::new();
    hasher.update(MEMBER);
    let tuple = hasher.finalize();
    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }
    let dat = format!(
        r#"<?xml version="1.0"?>
<!DOCTYPE datafile PUBLIC "-//Logiqx//DTD ROM Management Datafile//EN" "http://www.logiqx.com/Dats/datafile.dtd">
<datafile><header><name>nds</name><description>Test dat</description><version>r1</version><author>no-intro</author></header>
<game name="Mario Kart DS (USA)"><description>Mario Kart DS</description><rom name="Mario Kart DS (USA).nds" size="{size}" crc="{crc}" sha1="{sha1}"/></game>
</datafile>"#,
        size = MEMBER.len(),
        crc = hex(&tuple.crc32),
        sha1 = hex(&tuple.sha1),
    );
    let (status, v) = post_bytes(
        f.addr,
        "/v1/dats/import?provider=no-intro&system=nds",
        dat.as_bytes(),
    );
    assert_eq!(status, 200, "{v}");

    // Nothing ingested yet: the shelf is red.
    let (status, v) = get(f.addr, "/v1/systems");
    assert_eq!(status, 200);
    let systems = v["systems"].as_array().expect("systems");
    assert_eq!(systems.len(), 1);
    assert_eq!(systems[0]["counts"]["missing"], 1, "{v}");

    // Upload + ingest a zip holding the claimed member.
    let zip = stored_zip(&[("game.nds", MEMBER)]);
    let (status, v) = post_bytes(f.addr, "/v1/ingest/uploads?name=pack.zip", &zip);
    assert_eq!(status, 200, "{v}");
    let token = v["upload"].as_str().expect("token").to_owned();
    let (status, v) = post_json(
        f.addr,
        "/v1/ingest",
        &format!(r#"{{"uploads":["{token}"]}}"#),
    );
    assert_eq!(status, 200, "{v}");
    let done = wait_done(f.addr, v["job"].as_i64().expect("job id"));
    assert_eq!(done["state"], "done", "{done}");
    assert_eq!(done["report"]["members_claimed"], 1, "{done}");
    // The user-vocabulary report: the job names the entry it newly
    // satisfied, not just blob counts.
    assert_eq!(done["matched_total"], 1, "{done}");
    assert_eq!(done["matched"][0]["name"], "Mario Kart DS (USA)");
    assert_eq!(done["matched"][0]["source"], "no-intro/nds");

    // The shelf lit up without any dat import/view eval in between:
    // the zip member's LocalIngest recipe counts as have(verified).
    let (status, v) = get(f.addr, "/v1/systems");
    assert_eq!(status, 200);
    let counts = &v["systems"][0]["counts"];
    assert_eq!(counts["verified"], 1, "{v}");
    assert_eq!(counts["missing"], 0, "{v}");
}

/// The unified drop surface: one job classifies every staged file by
/// content — loose dat, zipped dat, or ROM — imports the dats, and
/// runs the pipeline over the rest. The boundary case that must not
/// wobble: a multi-member zip is a ROM container, never a zipped dat.
#[test]
fn job_classifies_dats_zipped_dats_and_roms_by_content() {
    let f = fixture();

    /// One-game Logiqx dat (the tests/api.rs template) with a distinct
    /// header identity per call — provider resolves from the author.
    fn logiqx(system: &str, author: &str, game: &str) -> String {
        format!(
            r#"<?xml version="1.0"?>
<!DOCTYPE datafile PUBLIC "-//Logiqx//DTD ROM Management Datafile//EN" "http://www.logiqx.com/Dats/datafile.dtd">
<datafile><header><name>{system}</name><description>Test dat</description><version>r1</version><author>{author}</author></header>
<game name="{game}"><description>{game}</description><rom name="{game}.bin" size="5" crc="12345678" sha1="000102030405060708090a0b0c0d0e0f10111213"/></game>
</datafile>"#
        )
    }
    let gba_dat = logiqx("gba", "no-intro", "Alpha (USA)");
    let psx_dat = logiqx("psx", "redump", "Beta (Japan)");
    let psx_zip = stored_zip(&[("psx.dat", psx_dat.as_bytes())]);
    let rom_zip = stored_zip(&[
        ("one.gba", b"member one bytes" as &[u8]),
        ("two.gba", b"member two bytes"),
    ]);

    let mut tokens = Vec::new();
    for (name, bytes) in [
        ("alpha.gba", b"loose rom bytes" as &[u8]),
        ("gba.dat", gba_dat.as_bytes()),
        ("psx.zip", psx_zip.as_slice()),
        ("pack.zip", rom_zip.as_slice()),
    ] {
        let (status, v) = post_bytes(f.addr, &format!("/v1/ingest/uploads?name={name}"), bytes);
        assert_eq!(status, 200, "{v}");
        tokens.push(v["upload"].as_str().expect("token").to_owned());
    }
    let (status, v) = post_json(
        f.addr,
        "/v1/ingest",
        &format!(r#"{{"uploads":["{}"]}}"#, tokens.join("\",\"")),
    );
    assert_eq!(status, 200, "{v}");
    let done = wait_done(f.addr, v["job"].as_i64().expect("job id"));
    assert_eq!(done["state"], "done", "{done}");
    assert_eq!(done["files_done"], 4);

    // The dat lane: both dats imported, wearing the client names and
    // the identities their headers resolved to.
    let report = &done["report"];
    let dats = report["dats_imported"].as_array().expect("dats lane");
    assert_eq!(dats.len(), 2, "{report}");
    let row = |path: &str| {
        dats.iter()
            .find(|d| d["path"] == path)
            .unwrap_or_else(|| panic!("no dat row for {path}: {report}"))
    };
    let gba = row("gba.dat");
    assert_eq!(
        (gba["provider"].as_str(), gba["system"].as_str()),
        (Some("no-intro"), Some("gba"))
    );
    assert_eq!(gba["entries"], 1);
    let psx = row("psx.zip");
    assert_eq!(
        (psx["provider"].as_str(), psx["system"].as_str()),
        (Some("redump"), Some("psx"))
    );
    assert_eq!(psx["entries"], 1);

    // Pipeline counters stay pure: only the ROM and the two-member
    // zip went through it, and the zip stayed a ROM container.
    assert_eq!(report["files_scanned"], 2, "{report}");
    assert_eq!(report["files_stored"], 2, "{report}");
    assert_eq!(report["members_claimed"], 2, "two-member zip is ROM ingest");
    assert_eq!(report["errors"], serde_json::json!([]));

    // The imports are live: both sources on the shelf.
    let (status, v) = get(f.addr, "/v1/systems");
    assert_eq!(status, 200);
    let mut sources: Vec<&str> = v["systems"]
        .as_array()
        .expect("systems")
        .iter()
        .map(|s| s["source"].as_str().expect("source"))
        .collect();
    sources.sort_unstable();
    assert_eq!(sources, ["no-intro/gba", "redump/psx"], "{v}");
}

/// The blob inspector walks the recipe DAG the ingest pipeline minted:
/// a zip member's routes_in edge points at its container, the
/// container's routes_out names the member, and provenance carries the
/// client's upload name.
#[test]
fn blob_inspector_walks_the_zip_dag() {
    let f = fixture();
    const MEMBER: &[u8] = b"zip member content";
    let zip = stored_zip(&[("inner.gba", MEMBER)]);
    let member_hex = datboi_core::hash::Blake3::compute(MEMBER).to_hex();
    let zip_hex = datboi_core::hash::Blake3::compute(&zip).to_hex();

    let (status, v) = post_bytes(f.addr, "/v1/ingest/uploads?name=packs%2Fpack.zip", &zip);
    assert_eq!(status, 200, "{v}");
    let token = v["upload"].as_str().expect("token").to_owned();
    let (_, v) = post_json(
        f.addr,
        "/v1/ingest",
        &format!(r#"{{"uploads":["{token}"]}}"#),
    );
    let done = wait_done(f.addr, v["job"].as_i64().expect("job id"));
    assert_eq!(done["state"], "done", "{done}");
    assert_eq!(done["report"]["members_claimed"], 1, "{done}");

    // The member: made FROM the container (a STORED member is an
    // assemble@1 slice; the container keeps the bytes, D35).
    let (status, v) = get(f.addr, &format!("/v1/blobs/{member_hex}"));
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["namespace"], "data");
    assert_eq!(v["size"], MEMBER.len() as u64);
    assert_eq!(v["routes_in"].as_array().expect("routes_in").len(), 1);
    let edge = &v["routes_in"][0];
    assert_eq!(edge["op"], "assemble@1");
    assert_eq!(edge["verify"], "verified", "verify-on-ingest (D4)");
    assert_eq!(
        edge["inputs"][0]["hash"],
        zip_hex.as_str(),
        "the edge points at the container: {v}"
    );
    assert_eq!(edge["outputs"][0]["hash"], member_hex.as_str());
    assert_eq!(edge["outputs"][0]["name"], "inner.gba");
    assert_eq!(v["routes_out"], serde_json::json!([]));

    // The container: the mirror edge, plus upload-name provenance.
    let (status, v) = get(f.addr, &format!("/v1/blobs/{zip_hex}"));
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["routes_in"], serde_json::json!([]));
    assert_eq!(v["routes_out"].as_array().expect("routes_out").len(), 1);
    let edge = &v["routes_out"][0];
    assert_eq!(edge["op"], "assemble@1");
    assert_eq!(edge["inputs"][0]["hash"], zip_hex.as_str());
    assert_eq!(edge["outputs"][0]["hash"], member_hex.as_str());
    assert_eq!(edge["outputs"][0]["name"], "inner.gba");
    // Provenance is the rescan-cache KEY — for a staged upload that is
    // the staging path (which embeds the client basename), not the
    // client's relative name (that lives in the job report only).
    let prov = v["provenance"][0]["path"].as_str().expect("path");
    assert!(prov.contains("pack.zip"), "{v}");

    // The listing's degree counts agree with the inspector's edges.
    let (_, v) = get(f.addr, &format!("/v1/blobs?q={member_hex}"));
    assert_eq!(v["total"], 1);
    assert_eq!(v["blobs"][0]["routes_in"], 1);
    assert_eq!(v["blobs"][0]["routes_out"], 0);
}

#[test]
fn upload_and_start_reject_bad_requests() {
    let f = fixture();

    // name is required and must be a sane relative path
    assert_eq!(post_bytes(f.addr, "/v1/ingest/uploads", b"x").0, 400);
    assert_eq!(
        post_bytes(f.addr, "/v1/ingest/uploads?name=%2Fetc%2Fpasswd", b"x").0,
        400
    );
    assert_eq!(
        post_bytes(f.addr, "/v1/ingest/uploads?name=a%2F..%2Fb.gba", b"x").0,
        400
    );
    // empty body
    let (status, v) = post_bytes(f.addr, "/v1/ingest/uploads?name=a.gba", b"");
    assert_eq!((status, v["error"].as_str()), (400, Some("empty upload")));

    // start: missing field, empty list, bogus token
    assert_eq!(post_json(f.addr, "/v1/ingest", "{}").0, 400);
    assert_eq!(post_json(f.addr, "/v1/ingest", r#"{"uploads":[]}"#).0, 400);
    let (status, v) = post_json(f.addr, "/v1/ingest", r#"{"uploads":["nope"]}"#);
    assert_eq!(status, 400);
    assert!(
        v["error"]
            .as_str()
            .expect("msg")
            .contains("unknown or expired"),
        "{v}"
    );

    // job detail misses
    assert_eq!(get(f.addr, "/v1/jobs/999").0, 404);
    assert_eq!(get(f.addr, "/v1/jobs/abc").0, 400);

    // nothing rejected left residue behind
    assert_eq!(
        std::fs::read_dir(f.store_root.join("tmp"))
            .expect("tmp")
            .count(),
        0
    );
}

/// Minimal zip with one DEFLATE member (the shape preflate rebuilds) —
/// the datboi-ingest refine fixture, trimmed to one call site.
fn deflate_zip(name: &[u8], payload: &[u8]) -> Vec<u8> {
    use std::io::Write as _;
    let compressed = {
        let mut enc =
            flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::new(6));
        enc.write_all(payload).expect("deflate");
        enc.finish().expect("finish")
    };
    let crc = {
        let mut h = crc32fast::Hasher::new();
        h.update(payload);
        h.finalize()
    };
    let (nlen, csize, usize_) = (
        u16::try_from(name.len()).unwrap(),
        u32::try_from(compressed.len()).unwrap(),
        u32::try_from(payload.len()).unwrap(),
    );
    let mut out = Vec::new();
    out.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
    out.extend_from_slice(&20u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&8u16.to_le_bytes()); // method: DEFLATE
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&crc.to_le_bytes());
    out.extend_from_slice(&csize.to_le_bytes());
    out.extend_from_slice(&usize_.to_le_bytes());
    out.extend_from_slice(&nlen.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(name);
    out.extend_from_slice(&compressed);
    let cd_offset = u32::try_from(out.len()).unwrap();
    out.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
    out.extend_from_slice(&20u16.to_le_bytes());
    out.extend_from_slice(&20u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&8u16.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&crc.to_le_bytes());
    out.extend_from_slice(&csize.to_le_bytes());
    out.extend_from_slice(&usize_.to_le_bytes());
    out.extend_from_slice(&nlen.to_le_bytes());
    out.extend_from_slice(&[0; 12]);
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(name);
    let cd_size = u32::try_from(out.len()).unwrap() - cd_offset;
    out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
    out.extend_from_slice(&[0; 4]);
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&cd_size.to_le_bytes());
    out.extend_from_slice(&cd_offset.to_le_bytes());
    out.extend_from_slice(&[0; 2]);
    out
}

/// D71 end to end: drop a deflate zip on the web surface with ambient
/// refinement ON, and the preflate drain follows the ingest without
/// any CLI involvement — a `refine — preflate` job appears in the
/// tray, concludes positive, and the container's rebuild recipes exist
/// in the index.
#[test]
fn ambient_refine_follows_ingest() {
    let f = fixture_with_refine(true);
    let payload: Vec<u8> = (0..100_000u32)
        .map(|i| (i % 251) as u8 ^ (i / 997) as u8)
        .collect();
    let zip = deflate_zip(b"game.bin", &payload);

    let (status, v) = post_bytes(f.addr, "/v1/ingest/uploads?name=game.zip", &zip);
    assert_eq!(status, 200, "{v}");
    let token = v["upload"].as_str().expect("token").to_owned();
    let (status, v) = post_json(
        f.addr,
        "/v1/ingest",
        &format!("{{\"uploads\":[{}]}}", serde_json::json!(token)),
    );
    assert_eq!(status, 200, "{v}");
    let ingest_job = v["job"].as_i64().expect("job id");
    let done = wait_done(f.addr, ingest_job);
    assert_eq!(done["state"], "done", "{done}");

    // The refine job is spawned by the daemon, not the client: poll the
    // tray until the preflate drain shows up and finishes.
    let deadline = Instant::now() + Duration::from_secs(30);
    let detail = loop {
        let (status, v) = get(f.addr, "/v1/jobs");
        assert_eq!(status, 200, "{v}");
        let preflate = v["jobs"].as_array().expect("jobs").iter().find(|j| {
            j["kind"] == "refine" && j["name"].as_str().is_some_and(|n| n.contains("preflate"))
        });
        if let Some(job) = preflate
            && job["state"] != "running"
        {
            assert_eq!(job["state"], "done", "{job}");
            assert_eq!(job["progress"], 100);
            let (status, detail) = get(f.addr, &format!("/v1/jobs/{}", job["id"]));
            assert_eq!(status, 200, "{detail}");
            break detail;
        }
        assert!(
            Instant::now() < deadline,
            "no finished preflate refine job appeared: {v}"
        );
        std::thread::sleep(Duration::from_millis(100));
    };
    let notes = detail["report"]["notes"].to_string();
    assert!(notes.contains("1 rebuildable"), "{detail}");

    // The claims are real: the container gained its preflate recreate
    // route (read through a second connection; WAL permits it).
    let db = Db::open(&f.db_dir).expect("open alongside daemon");
    let recreates: i64 = db
        .cache()
        .query_row(
            "SELECT COUNT(*) FROM recipe WHERE op_name = 'xf-preflate/recreate'",
            [],
            |r| r.get(0),
        )
        .expect("query");
    assert_eq!(recreates, 1, "one recreate recipe per split member");
}

/// D74 over the wire: a second daemon on the same db-dir serves the
/// first daemon's finished jobs — same ids, same report — where the
/// old in-memory registry served amnesia.
#[test]
fn job_history_survives_daemon_restart() {
    let f = fixture();
    let rom = b"history-worthy rom content";
    let (status, v) = post_bytes(f.addr, "/v1/ingest/uploads?name=keep%2Fme.gba", rom);
    assert_eq!(status, 200, "{v}");
    let token = v["upload"].as_str().expect("token").to_owned();
    let (status, v) = post_json(
        f.addr,
        "/v1/ingest",
        &format!("{{\"uploads\":[{}]}}", serde_json::json!(token)),
    );
    assert_eq!(status, 200, "{v}");
    let job = v["job"].as_i64().expect("job");
    let done = wait_done(f.addr, job);
    assert_eq!(done["state"], "done", "{done}");

    // "Restart": a second daemon over the same universe (the first
    // keeps running — WAL arbitrates; one-daemon-per-db-dir is the
    // production rule, not a test constraint).
    let second = Server::bind(&Config {
        store_root: f.store_root.clone(),
        db_dir: f.db_dir.clone(),
        listen: SocketAddr::from_str("127.0.0.1:0").expect("addr"),
        nfs_listen: None,
        detectors_dir: None,
        refine: false,
    })
    .expect("bind second");
    let addr2 = second.local_addr().expect("addr");
    std::thread::spawn(move || second.serve());

    let (status, v) = get(addr2, &format!("/v1/jobs/{job}"));
    assert_eq!(status, 200, "history lost across restart: {v}");
    assert_eq!(v["state"], "done", "{v}");
    assert_eq!(v["files_total"], 1, "{v}");
    assert_eq!(v["report"]["files_stored"], 1, "frozen report served: {v}");
    let (_, v) = get(addr2, "/v1/jobs");
    assert!(
        v["jobs"].as_array().expect("jobs").iter().any(|j| j["id"] == job),
        "tray list misses the historical job: {v}"
    );
}
