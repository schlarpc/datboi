//! End-to-end HTTP semantics against a live daemon: a real store + index
//! with a tagged view snapshot, served over an ephemeral port. The
//! snapshot is hand-minted — the server contract starts at "resolve tag,
//! decode manifest, serve verified bytes", however the snapshot was made.

use std::net::SocketAddr;
use std::str::FromStr as _;

use datboi_core::hash::Blake3;
use datboi_core::viewsnap::{ViewRow, ViewSnapshot};
use datboi_index::{Db, Namespace as IxNs, Residency};
use datboi_server::{Config, Server};
use datboi_store_fs::{Namespace as StoreNs, Store};

const ALPHA: &[u8] = b"alpha rom content";
/// Big enough to cross the daemon's 8 MiB streaming window twice.
const BIG_LEN: usize = 20 << 20;

fn big_bytes() -> Vec<u8> {
    (0..BIG_LEN).map(|i| (i % 251) as u8).collect()
}

struct Fixture {
    addr: SocketAddr,
    snapshot: String,
    _root: tempfile::TempDir,
}

fn put_data(store: &Store, db: &Db, bytes: &[u8]) -> Blake3 {
    let hash = Blake3::compute(bytes);
    store
        .put_with_obao(StoreNs::Data, hash, bytes.len() as u64, bytes)
        .expect("put");
    db.upsert_blob(&hash, Some(bytes.len() as u64), IxNs::Data, Residency::Resident)
        .expect("index");
    hash
}

fn fixture() -> Fixture {
    let root = tempfile::tempdir().expect("tempdir");
    let store_root = root.path().join("store");
    let db_dir = root.path().join("db");
    std::fs::create_dir_all(&db_dir).expect("db dir");
    let store = Store::open(&store_root).expect("store");
    let db = Db::open(&db_dir).expect("db");

    let alpha = put_data(&store, &db, ALPHA);
    let deep = put_data(&store, &db, b"deep");
    let big = put_data(&store, &db, &big_bytes());

    let snap = ViewSnapshot {
        created_at: 1_780_000_000,
        view_name: "test".into(),
        sources: vec![],
        rows: vec![
            ViewRow {
                path: "Alpha/alpha.gba".into(),
                hash: alpha,
                size: ALPHA.len() as u64,
                seek: 0,
            },
            ViewRow {
                path: "Alpha/sub/deep.bin".into(),
                hash: deep,
                size: 4,
                seek: 0,
            },
            ViewRow {
                path: "big/huge.iso".into(),
                hash: big,
                size: BIG_LEN as u64,
                seek: 0,
            },
        ],
    };
    let encoded = snap.encode().expect("encode");
    let snap_hash = Blake3::compute(&encoded);
    store
        .put(StoreNs::Meta, snap_hash, encoded.as_slice())
        .expect("put snap");
    db.upsert_blob(&snap_hash, Some(encoded.len() as u64), IxNs::Meta, Residency::Resident)
        .expect("index snap");
    db.set_tag("view/test", &snap_hash, 1_780_000_000).expect("tag");
    drop(db); // the daemon opens its own handles

    let server = Server::bind(&Config {
        store_root,
        db_dir,
        listen: SocketAddr::from_str("127.0.0.1:0").expect("addr"),
    })
    .expect("bind");
    let addr = server.local_addr().expect("addr");
    std::thread::spawn(move || server.serve());
    Fixture {
        addr,
        snapshot: snap_hash.to_hex(),
        _root: root,
    }
}

/// GET with optional headers; returns (status, response). Never follows
/// redirects — the tests assert on them.
fn get(
    addr: SocketAddr,
    path: &str,
    headers: &[(&str, &str)],
) -> (u16, ureq::Response) {
    let agent = ureq::AgentBuilder::new().redirects(0).build();
    let mut req = agent.get(&format!("http://{addr}{path}"));
    for (k, v) in headers {
        req = req.set(k, v);
    }
    // The listener is bound before the fixture returns, but the accept
    // loop starts on another thread: retry briefly on connection errors.
    for _ in 0..50 {
        match req.clone().call() {
            Ok(resp) => return (resp.status(), resp),
            Err(ureq::Error::Status(code, resp)) => return (code, resp),
            Err(ureq::Error::Transport(_)) => {
                std::thread::sleep(std::time::Duration::from_millis(40));
            }
        }
    }
    panic!("server never came up at {addr}");
}

fn body(resp: ureq::Response) -> Vec<u8> {
    let mut out = Vec::new();
    use std::io::Read as _;
    resp.into_reader().read_to_end(&mut out).expect("read body");
    out
}

#[test]
fn http_surface_end_to_end() {
    let f = fixture();

    // health + discovery
    let (status, resp) = get(f.addr, "/healthz", &[]);
    assert_eq!((status, resp.into_string().expect("text").as_str()), (200, "ok"));
    let (status, resp) = get(f.addr, "/v1/views", &[]);
    assert_eq!(status, 200);
    let v: serde_json::Value =
        serde_json::from_str(&resp.into_string().expect("json")).expect("parse");
    assert_eq!(v["views"][0]["name"], "test");
    assert_eq!(v["views"][0]["snapshot"], f.snapshot.as_str());

    // listings: html + json, root + nested
    let (status, resp) = get(f.addr, "/view/test/", &[]);
    assert_eq!(status, 200);
    let page = resp.into_string().expect("html");
    assert!(page.contains("Alpha/") && page.contains("big/"), "{page}");
    let (status, resp) = get(f.addr, "/view/test/Alpha/?json", &[]);
    assert_eq!(status, 200);
    let v: serde_json::Value =
        serde_json::from_str(&resp.into_string().expect("json")).expect("parse");
    assert_eq!(v["dirs"][0], "sub");
    assert_eq!(v["files"][0]["name"], "alpha.gba");
    assert_eq!(v["files"][0]["size"], 17);
    assert_eq!(v["snapshot"], f.snapshot.as_str());

    // directory without slash canonicalizes; view root without slash too
    let (status, resp) = get(f.addr, "/view/test/Alpha", &[]);
    assert_eq!(status, 308);
    assert_eq!(resp.header("location"), Some("/view/test/Alpha/"));
    let (status, resp) = get(f.addr, "/view/test", &[]);
    assert_eq!(status, 308);
    assert_eq!(resp.header("location"), Some("/view/test/"));

    // full GET: bytes, strong ETag, range advertisement
    let (status, resp) = get(f.addr, "/view/test/Alpha/alpha.gba", &[]);
    assert_eq!(status, 200);
    let etag = resp.header("etag").expect("etag").to_owned();
    assert_eq!(etag, format!("\"{}\"", Blake3::compute(ALPHA).to_hex()));
    assert_eq!(resp.header("accept-ranges"), Some("bytes"));
    assert_eq!(resp.header("cache-control"), Some("no-cache"));
    assert_eq!(resp.header("datboi-snapshot"), Some(f.snapshot.as_str()));
    assert_eq!(body(resp), ALPHA);

    // conditional revalidation
    let (status, _) = get(
        f.addr,
        "/view/test/Alpha/alpha.gba",
        &[("If-None-Match", &etag)],
    );
    assert_eq!(status, 304);

    // ranges: bounded, open-ended, suffix, unsatisfiable
    let (status, resp) = get(
        f.addr,
        "/view/test/Alpha/alpha.gba",
        &[("Range", "bytes=2-5")],
    );
    assert_eq!(status, 206);
    assert_eq!(resp.header("content-range"), Some("bytes 2-5/17"));
    assert_eq!(body(resp), &ALPHA[2..6]);
    let (status, resp) = get(
        f.addr,
        "/view/test/Alpha/alpha.gba",
        &[("Range", "bytes=10-")],
    );
    assert_eq!(status, 206);
    assert_eq!(body(resp), &ALPHA[10..]);
    let (status, resp) = get(
        f.addr,
        "/view/test/Alpha/alpha.gba",
        &[("Range", "bytes=-4")],
    );
    assert_eq!(status, 206);
    assert_eq!(resp.header("content-range"), Some("bytes 13-16/17"));
    assert_eq!(body(resp), &ALPHA[13..]);
    let (status, resp) = get(
        f.addr,
        "/view/test/Alpha/alpha.gba",
        &[("Range", "bytes=99-")],
    );
    assert_eq!(status, 416);
    assert_eq!(resp.header("content-range"), Some("bytes */17"));

    // If-Range with a stale validator degrades to 200
    let (status, resp) = get(
        f.addr,
        "/view/test/Alpha/alpha.gba",
        &[("Range", "bytes=2-5"), ("If-Range", "\"deadbeef\"")],
    );
    assert_eq!(status, 200);
    assert_eq!(body(resp).len(), ALPHA.len());

    // immutable snapshot addressing
    let (status, resp) = get(
        f.addr,
        &format!("/snap/{}/Alpha/sub/deep.bin", f.snapshot),
        &[],
    );
    assert_eq!(status, 200);
    assert_eq!(
        resp.header("cache-control"),
        Some("public, max-age=31536000, immutable")
    );
    assert_eq!(body(resp), b"deep");
    let (status, _) = get(f.addr, "/snap/nothex/", &[]);
    assert_eq!(status, 400);

    // 404s: unknown view, unknown path, file-as-directory
    assert_eq!(get(f.addr, "/view/nope/", &[]).0, 404);
    assert_eq!(get(f.addr, "/view/test/Alpha/nope.bin", &[]).0, 404);
    assert_eq!(get(f.addr, "/view/test/Alpha/alpha.gba/", &[]).0, 404);

    // multi-window streaming: full body and a window-straddling range
    let big = big_bytes();
    let (status, resp) = get(f.addr, "/view/test/big/huge.iso", &[]);
    assert_eq!(status, 200);
    let got = body(resp);
    assert_eq!(got.len(), BIG_LEN);
    assert_eq!(got, big, "streamed body must be byte-exact");
    let (status, resp) = get(
        f.addr,
        "/view/test/big/huge.iso",
        &[("Range", "bytes=8388600-8388615")],
    );
    assert_eq!(status, 206);
    assert_eq!(body(resp), &big[8_388_600..8_388_616]);
}
