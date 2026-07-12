//! D72/D73 end to end against a live daemon: the automatic
//! ingest → refine → license → evict motion, and the orphan
//! mark → review → keep → apply lifecycle with its guard and
//! delete-time re-verification.

use std::net::SocketAddr;
use std::str::FromStr as _;
use std::time::{Duration, Instant};

use datboi_core::hash::Blake3;
use datboi_index::Db;
use datboi_server::{Config, Server};
use datboi_store_fs::Store;

struct Fixture {
    addr: SocketAddr,
    db_dir: std::path::PathBuf,
    _root: tempfile::TempDir,
}

/// A refine-enabled daemon over a universe whose GC policy the test
/// author already wrote into state.db (config is read live, but
/// setting it before the first wake keeps the timeline simple).
fn fixture(configure: impl FnOnce(&Db)) -> Fixture {
    let root = tempfile::tempdir().expect("tempdir");
    let store_root = root.path().join("store");
    let db_dir = root.path().join("db");
    drop(Store::open(&store_root).expect("store"));
    {
        let db = Db::open(&db_dir).expect("db");
        configure(&db);
    }
    let server = Server::bind(&Config {
        store_root,
        db_dir: db_dir.clone(),
        listen: SocketAddr::from_str("127.0.0.1:0").expect("addr"),
        nfs_listen: None,
        detectors_dir: None,
        refine: true,
    })
    .expect("bind");
    let addr = server.local_addr().expect("addr");
    std::thread::spawn(move || server.serve());
    Fixture {
        addr,
        db_dir,
        _root: root,
    }
}

fn get(addr: SocketAddr, path: &str) -> (u16, serde_json::Value) {
    let agent = ureq::AgentBuilder::new().redirects(0).build();
    let req = agent.get(&format!("http://{addr}{path}"));
    for _ in 0..50 {
        match req.clone().call() {
            Ok(resp) => return (resp.status(), parse(resp)),
            Err(ureq::Error::Status(code, resp)) => return (code, parse(resp)),
            Err(ureq::Error::Transport(_)) => std::thread::sleep(Duration::from_millis(40)),
        }
    }
    panic!("server never came up at {addr}");
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

fn parse(resp: ureq::Response) -> serde_json::Value {
    let text = resp.into_string().expect("body");
    serde_json::from_str(&text).unwrap_or(serde_json::Value::String(text))
}

/// Upload + ingest one file; wait for the ingest job to finish.
fn ingest(addr: SocketAddr, name: &str, bytes: &[u8]) {
    let (status, v) = post_bytes(addr, &format!("/v1/ingest/uploads?name={name}"), bytes);
    assert_eq!(status, 200, "{v}");
    let token = v["upload"].as_str().expect("token").to_owned();
    let (status, v) = post_json(
        addr,
        "/v1/ingest",
        &format!("{{\"uploads\":[{}]}}", serde_json::json!(token)),
    );
    assert_eq!(status, 200, "{v}");
    let job = v["job"].as_i64().expect("job");
    wait_for(Duration::from_secs(10), "ingest job", || {
        let (_, v) = get(addr, &format!("/v1/jobs/{job}"));
        (v["state"] == "done").then_some(())
    });
}

fn wait_for<T>(timeout: Duration, what: &str, mut probe: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(value) = probe() {
            return value;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Same single-DEFLATE-member zip the refine e2e uses.
fn deflate_zip(payload: &[u8]) -> Vec<u8> {
    use std::io::Write as _;
    let compressed = {
        let mut enc = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::new(6));
        enc.write_all(payload).expect("deflate");
        enc.finish().expect("finish")
    };
    let crc = {
        let mut h = crc32fast::Hasher::new();
        h.update(payload);
        h.finalize()
    };
    let name = b"game.bin";
    let (nlen, csize, usize_) = (
        u16::try_from(name.len()).unwrap(),
        u32::try_from(compressed.len()).unwrap(),
        u32::try_from(payload.len()).unwrap(),
    );
    let mut out = Vec::new();
    out.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
    out.extend_from_slice(&20u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&8u16.to_le_bytes());
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

/// D72 end to end: with an absolute 1-byte watermark, dropping a zip is
/// ALL it takes — the daemon refines it, licenses the rebuild route,
/// and evicts the container literal, no CLI anywhere. The plaintext
/// stays resident (the bytes dats name; nothing covers them).
#[test]
fn watermark_evicts_licensed_container_automatically() {
    let f = fixture(|db| {
        // Any fs is over a 1-byte high-water; evict as much as licensing
        // allows (low = 1 byte too).
        db.config_set("evict:high-water", b"1").expect("cfg");
        db.config_set("evict:low-water", b"1").expect("cfg");
    });
    let payload: Vec<u8> = (0..100_000u32)
        .map(|i| (i % 251) as u8 ^ (i / 997) as u8)
        .collect();
    let zip = deflate_zip(&payload);
    let container = Blake3::compute(&zip);

    ingest(f.addr, "game.zip", &zip);
    // Ingest wake → preflate drain → licensing replay → watermark drop,
    // all one motion. The blob inspector reports the flip.
    wait_for(Duration::from_secs(60), "container eviction", || {
        let (status, v) = get(f.addr, &format!("/v1/blobs/{}", container.to_hex()));
        assert_eq!(status, 200, "{v}");
        (v["residency"] == "evicted_covered").then_some(())
    });
}

/// D73 end to end over the API: junk ingests, analyzers conclude
/// negative, the sweep marks it, review shows it with provenance, the
/// guard refuses apply while held, keep-marks exclude, and apply
/// deletes with delete-time re-verification.
#[test]
fn orphan_review_keep_and_apply_lifecycle() {
    let f = fixture(|db| {
        db.config_set("evict:high-water", b"off").expect("cfg");
        db.config_set("gc:grace-secs", b"0").expect("cfg");
    });
    // Junk: not a container, not CD-shaped, below the chunk threshold —
    // every analyzer concludes Negative, so nothing ever references it.
    let junk: Vec<u8> = (0..64_000u32).map(|i| (i * 7 % 253) as u8).collect();
    let hash = Blake3::compute(&junk);
    ingest(f.addr, "mystery.bin", &junk);

    // Wait for the analyzers to finish (queued blobs are never marked —
    // their references may not exist yet).
    let db = Db::open(&f.db_dir).expect("open alongside daemon");
    wait_for(Duration::from_secs(30), "analysis fixpoint", || {
        let queued: i64 = db
            .cache()
            .query_row("SELECT COUNT(*) FROM sweep_queue", [], |r| r.get(0))
            .expect("q");
        (queued == 0).then_some(())
    });
    // The ambient mark sweep runs on a 30-minute clock; drive one now
    // (same call, second connection — WAL arbitrates).
    let mut db = db;
    db.sweep_orphan_marks(&[], 1).expect("mark sweep");

    let (status, v) = get(f.addr, "/v1/gc/orphans");
    assert_eq!(status, 200, "{v}");
    let hex = hash.to_hex();
    let mine = v["orphans"]
        .as_array()
        .expect("orphans")
        .iter()
        .find(|o| o["hash"] == hex.as_str())
        .unwrap_or_else(|| panic!("junk not surfaced: {v}"))
        .clone();
    assert_eq!(mine["kept"], false);
    assert_eq!(mine["size"], 64_000);
    // Provenance wears the CLIENT's name, never the staging path.
    assert_eq!(
        mine["sources"],
        serde_json::json!(["mystery.bin"]),
        "orphan provenance leaked a staging path: {mine}"
    );

    // Guard held elsewhere → apply refuses with 503, deletes nothing.
    let holder = datboi_index::GuardHolder([9; 16]);
    assert!(db.claim_gc_guard(&holder, now(), 600).expect("claim"));
    let (status, v) = post_json(f.addr, "/v1/gc/orphans/apply", "{}");
    assert_eq!(status, 503, "{v}");
    db.release_gc_guard(&holder).expect("release");

    // Keep-mark excludes it from apply (skipped, not deleted).
    let (status, _) = post_json(
        f.addr,
        "/v1/gc/keep",
        &format!("{{\"hash\":\"{hex}\",\"keep\":true}}"),
    );
    assert_eq!(status, 200);
    let (status, v) = post_json(f.addr, "/v1/gc/orphans/apply", "{}");
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["deleted"], 0, "{v}");
    assert_eq!(v["skipped"], 1, "{v}");

    // Unkeep → apply deletes it: rows gone, bytes gone, review empty.
    let (status, _) = post_json(
        f.addr,
        "/v1/gc/keep",
        &format!("{{\"hash\":\"{hex}\",\"keep\":false}}"),
    );
    assert_eq!(status, 200);
    let (status, v) = post_json(f.addr, "/v1/gc/orphans/apply", "{}");
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["deleted"], 1, "{v}");
    assert_eq!(v["bytes_reclaimed"], 64_000, "{v}");
    let (status, v) = get(f.addr, &format!("/v1/blobs/{hex}"));
    assert_eq!(status, 404, "deleted blob still answers: {v}");
    let (_, v) = get(f.addr, "/v1/gc/orphans");
    assert!(v["orphans"].as_array().expect("orphans").is_empty(), "{v}");
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}
