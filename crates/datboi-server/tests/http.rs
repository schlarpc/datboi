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
    /// The daemon's database dir — tests may open a second handle (WAL
    /// allows it) the way the CLI does against a live daemon.
    db_dir: std::path::PathBuf,
    _root: tempfile::TempDir,
}

fn put_data(store: &Store, db: &Db, bytes: &[u8]) -> Blake3 {
    let hash = Blake3::compute(bytes);
    store
        .put_with_obao(StoreNs::Data, hash, bytes.len() as u64, bytes)
        .expect("put");
    db.upsert_blob(
        &hash,
        Some(bytes.len() as u64),
        IxNs::Data,
        Residency::Resident,
    )
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
    db.upsert_blob(
        &snap_hash,
        Some(encoded.len() as u64),
        IxNs::Meta,
        Residency::Resident,
    )
    .expect("index snap");
    db.set_tag("view/test", &snap_hash, 1_780_000_000)
        .expect("tag");
    drop(db); // the daemon opens its own handles

    let server = Server::bind(&Config {
        store_root,
        db_dir: db_dir.clone(),
        listen: SocketAddr::from_str("127.0.0.1:0").expect("addr"),
        nfs_listen: None,
        detectors_dir: None,
        // Tests want a quiescent database: no background analyzer
        // perturbing counts mid-assertion.
        refine: false,
        p2p: false,
    })
    .expect("bind");
    let addr = server.local_addr().expect("addr");
    std::thread::spawn(move || server.serve());
    Fixture {
        addr,
        snapshot: snap_hash.to_hex(),
        db_dir,
        _root: root,
    }
}

/// GET with optional headers; returns (status, response). Never follows
/// redirects — the tests assert on them.
fn get(addr: SocketAddr, path: &str, headers: &[(&str, &str)]) -> (u16, ureq::Response) {
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

/// PROPFIND with the given Depth; empty body = allprop.
fn propfind(addr: SocketAddr, path: &str, depth: &str) -> (u16, ureq::Response) {
    let agent = ureq::AgentBuilder::new().redirects(0).build();
    let req = agent
        .request("PROPFIND", &format!("http://{addr}{path}"))
        .set("Depth", depth);
    match req.call() {
        Ok(resp) => (resp.status(), resp),
        Err(ureq::Error::Status(code, resp)) => (code, resp),
        Err(e) => panic!("propfind transport error: {e}"),
    }
}

/// GET over a raw socket — no client-side URL normalization — and
/// return the status code.
fn raw_get_status(addr: SocketAddr, raw_path: &str) -> u16 {
    use std::io::{Read as _, Write as _};
    let mut sock = std::net::TcpStream::connect(addr).expect("connect");
    write!(
        sock,
        "GET {raw_path} HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n"
    )
    .expect("send");
    let mut response = Vec::new();
    sock.read_to_end(&mut response).expect("read");
    let head = std::str::from_utf8(&response[..response.len().min(64)]).expect("ascii head");
    head.split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .expect("status line")
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
    assert_eq!(
        (status, resp.into_string().expect("text").as_str()),
        (200, "ok")
    );
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

    // ---- hostile inputs ----

    // traversal shapes: lookups are exact string matches into a
    // canonical manifest — every dot-segment form just misses. Sent
    // over a raw socket because URL-standard clients (ureq included)
    // collapse dot segments (even %2e%2e forms) before sending.
    for path in [
        "/view/test/../../etc/passwd",
        "/view/test/%2e%2e/%2e%2e/etc/passwd",
        "/view/test/Alpha/%2e%2e/%2e%2e/secret",
        "/view/%2e%2e%2f%2e%2e%2fetc/",
        "/view/test/Alpha%00.gba",
        "/snap/%2e%2e/store",
    ] {
        let status = raw_get_status(f.addr, path);
        assert!(
            status == 404 || status == 400,
            "{path} must miss, got {status}"
        );
    }
    // control: the same transport serves a real file fine
    assert_eq!(raw_get_status(f.addr, "/view/test/Alpha/alpha.gba"), 200);

    // ranges at u64 boundaries: clamp or 416, never panic or overflow
    let (status, resp) = get(
        f.addr,
        "/view/test/Alpha/alpha.gba",
        &[("Range", "bytes=0-18446744073709551615")],
    );
    assert_eq!(status, 206);
    assert_eq!(body(resp).len(), ALPHA.len(), "last-pos clamps to EOF");
    let (status, _) = get(
        f.addr,
        "/view/test/Alpha/alpha.gba",
        &[("Range", "bytes=18446744073709551615-")],
    );
    assert_eq!(status, 416, "start past EOF");
    let (status, resp) = get(
        f.addr,
        "/view/test/Alpha/alpha.gba",
        // one past u64::MAX: unparseable → header ignored → 200
        &[("Range", "bytes=-18446744073709551616")],
    );
    assert_eq!(status, 200);
    assert_eq!(body(resp).len(), ALPHA.len());
    let (status, resp) = get(
        f.addr,
        "/view/test/Alpha/alpha.gba",
        &[("Range", "bytes=x-y,,,junk")],
    );
    assert_eq!(status, 200, "garbage Range is ignored per RFC 9110");
    drop(resp);

    // ---- WebDAV surface (same trees, protocol via dav-server) ----

    // PROPFIND depth 1 on the DAV root lists views as collections
    let (status, resp) = propfind(f.addr, "/dav/", "1");
    assert_eq!(status, 207);
    let xml = resp.into_string().expect("xml");
    assert!(xml.contains("test") && xml.contains("collection"), "{xml}");

    // PROPFIND on a file carries the size and the content-hash ETag
    let (status, resp) = propfind(f.addr, "/dav/test/Alpha/alpha.gba", "0");
    assert_eq!(status, 207);
    let xml = resp.into_string().expect("xml");
    assert!(xml.contains("17"), "getcontentlength: {xml}");
    assert!(
        xml.contains(&Blake3::compute(ALPHA).to_hex()),
        "content-hash etag: {xml}"
    );

    // GET + Range through the DAV mount uses the same verified path
    let (status, resp) = get(f.addr, "/dav/test/Alpha/alpha.gba", &[]);
    assert_eq!(status, 200);
    assert_eq!(body(resp), ALPHA);
    let (status, resp) = get(
        f.addr,
        "/dav/test/Alpha/alpha.gba",
        &[("Range", "bytes=2-5")],
    );
    assert_eq!(status, 206);
    assert_eq!(body(resp), &ALPHA[2..6]);

    // read-only: writes are rejected at the method set
    let agent = ureq::AgentBuilder::new().redirects(0).build();
    let put = agent
        .put(&format!("http://{}/dav/test/Alpha/alpha.gba", f.addr))
        .send_bytes(b"overwrite");
    match put {
        Err(ureq::Error::Status(code, _)) => assert_eq!(code, 405),
        other => panic!("PUT must be refused, got {other:?}"),
    }
    let mkcol = agent
        .request("MKCOL", &format!("http://{}/dav/test/newdir", f.addr))
        .call();
    match mkcol {
        Err(ureq::Error::Status(code, _)) => assert_eq!(code, 405),
        other => panic!("MKCOL must be refused, got {other:?}"),
    }

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

#[test]
fn embedded_web_ui_and_spa_fallback() {
    let f = fixture();

    // the SPA shell at / (the old plaintext root listing is gone — its
    // content is /v1/views, asserted above)
    let (status, resp) = get(f.addr, "/", &[]);
    assert_eq!(status, 200);
    assert_eq!(
        resp.header("content-type"),
        Some("text/html; charset=utf-8")
    );
    assert_eq!(resp.header("cache-control"), Some("no-cache"));
    let page = resp.into_string().expect("html");
    assert!(page.contains("<div id=\"app\""), "{page}");

    // a real hashed asset, name learned from the shell itself (vite
    // renames on every content change, so nothing here is static)
    let href = page
        .split('"')
        .find(|s| s.starts_with("/assets/"))
        .expect("index.html references a hashed asset")
        .to_owned();
    let (status, resp) = get(f.addr, &href, &[]);
    assert_eq!(status, 200);
    assert_eq!(
        resp.header("cache-control"),
        Some("public, max-age=31536000, immutable")
    );
    assert!(!body(resp).is_empty());

    // a stale hashed name is a real 404 — html would only mask it
    assert_eq!(get(f.addr, "/assets/nope-00000000.js", &[]).0, 404);

    // SPA fallback: unrouted paths belong to the client router
    let (status, resp) = get(f.addr, "/library/gba", &[]);
    assert_eq!(status, 200);
    assert!(
        resp.into_string()
            .expect("html")
            .contains("<div id=\"app\"")
    );

    // ...but the fallback must not swallow the daemon's namespaces
    assert_eq!(get(f.addr, "/healthz", &[]).0, 200);
    assert_eq!(get(f.addr, "/v1/nope", &[]).0, 404);
}

/// POST a JSON body; returns (status, response). The fixture's accept
/// loop is warmed by the retrying `get` before any test calls this.
fn post_json(
    addr: SocketAddr,
    path: &str,
    body: &str,
    headers: &[(&str, &str)],
) -> (u16, ureq::Response) {
    let agent = ureq::AgentBuilder::new().redirects(0).build();
    let mut req = agent
        .post(&format!("http://{addr}{path}"))
        .set("Content-Type", "application/json");
    for (k, v) in headers {
        req = req.set(k, v);
    }
    match req.send_string(body) {
        Ok(resp) => (resp.status(), resp),
        Err(ureq::Error::Status(code, resp)) => (code, resp),
        Err(e) => panic!("post transport error: {e}"),
    }
}

/// Pull the session token out of a Set-Cookie header.
fn cookie_token(resp: &ureq::Response) -> String {
    let cookie = resp.header("set-cookie").expect("set-cookie").to_owned();
    assert!(cookie.contains("HttpOnly"), "{cookie}");
    assert!(cookie.contains("SameSite=Lax"), "{cookie}");
    assert!(cookie.contains("Path=/"), "{cookie}");
    cookie
        .split(';')
        .next()
        .and_then(|kv| kv.strip_prefix("datboi_session="))
        .expect("cookie names the session")
        .to_owned()
}

/// The D68 browser flow end-to-end over loopback: whoami, invite
/// acceptance (cookie set, session persisted), login, logout. The
/// non-loopback enforcement matrix is unit-tested in auth.rs — resolve
/// takes the peer address as a parameter for exactly that reason.
#[test]
fn auth_flow_over_http() {
    use datboi_server::auth::{mint_token, token_hash};

    let f = fixture();

    // loopback is implicitly owner (D68): no ceremony, full visibility
    let (status, resp) = get(f.addr, "/v1/auth/whoami", &[]);
    assert_eq!(status, 200);
    let v: serde_json::Value =
        serde_json::from_str(&resp.into_string().expect("json")).expect("parse");
    assert_eq!(
        (
            v["authenticated"].as_bool(),
            v["role"].as_str(),
            v["via"].as_str()
        ),
        (Some(true), Some("owner"), Some("loopback"))
    );

    // mint an invite the way the CLI does: straight into state.db,
    // second handle, daemon live (WAL tolerates the concurrent writer)
    let db = Db::open(&f.db_dir).expect("second handle");
    let invite = mint_token().expect("entropy");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_secs() as i64;
    db.mint_invite(
        &token_hash(&invite),
        None,
        datboi_index::Role::Friend,
        now + 7 * 24 * 60 * 60,
    )
    .expect("mint");

    // validation rejections
    let (status, _) = post_json(
        f.addr,
        "/v1/auth/invite/accept",
        &format!(r#"{{"token":"{invite}","username":"Bad Name","password":"hunter22"}}"#),
        &[],
    );
    assert_eq!(status, 400, "username charset");
    let (status, _) = post_json(
        f.addr,
        "/v1/auth/invite/accept",
        &format!(r#"{{"token":"{invite}","username":"pal","password":"short"}}"#),
        &[],
    );
    assert_eq!(status, 400, "password length");
    let (status, _) = post_json(
        f.addr,
        "/v1/auth/invite/accept",
        r#"{"token":"bogus","username":"pal","password":"hunter22"}"#,
        &[],
    );
    assert_eq!(status, 403, "unknown invite");

    // acceptance: user created with the invite's role, session cookie set
    let (status, resp) = post_json(
        f.addr,
        "/v1/auth/invite/accept",
        &format!(r#"{{"token":"{invite}","username":"pal","password":"hunter22"}}"#),
        &[],
    );
    assert_eq!(status, 200);
    let session = cookie_token(&resp);
    let v: serde_json::Value =
        serde_json::from_str(&resp.into_string().expect("json")).expect("parse");
    assert_eq!(
        (v["username"].as_str(), v["role"].as_str()),
        (Some("pal"), Some("friend"))
    );
    let resolved = db.session_user(&token_hash(&session), now).expect("q");
    assert_eq!(
        resolved.map(|(_, name, role)| (name, role)),
        Some(("pal".to_owned(), datboi_index::Role::Friend)),
        "session persisted under blake3(token)"
    );

    // single-use: the same invite mints nothing twice
    let (status, _) = post_json(
        f.addr,
        "/v1/auth/invite/accept",
        &format!(r#"{{"token":"{invite}","username":"other","password":"hunter22"}}"#),
        &[],
    );
    assert_eq!(status, 403, "invite consumed");

    // login: uniform 401 for wrong password and unknown user
    let (status, _) = post_json(
        f.addr,
        "/v1/auth/login",
        r#"{"username":"pal","password":"wrongwrong"}"#,
        &[],
    );
    assert_eq!(status, 401);
    let (status, _) = post_json(
        f.addr,
        "/v1/auth/login",
        r#"{"username":"nobody","password":"wrongwrong"}"#,
        &[],
    );
    assert_eq!(status, 401);
    let (status, resp) = post_json(
        f.addr,
        "/v1/auth/login",
        r#"{"username":"pal","password":"hunter22"}"#,
        &[],
    );
    assert_eq!(status, 200);
    let login_session = cookie_token(&resp);
    assert_ne!(login_session, session, "every login mints a fresh token");

    // logout deletes the presented session and clears the cookie
    let (status, resp) = post_json(
        f.addr,
        "/v1/auth/logout",
        "{}",
        &[("Cookie", &format!("datboi_session={login_session}"))],
    );
    assert_eq!(status, 200);
    let cleared = resp.header("set-cookie").expect("set-cookie");
    assert!(cleared.contains("Max-Age=0"), "{cleared}");
    assert_eq!(
        db.session_user(&token_hash(&login_session), now)
            .expect("q"),
        None,
        "logout revoked the session"
    );
    // ...but only that one: the acceptance session survives
    assert!(
        db.session_user(&token_hash(&session), now)
            .expect("q")
            .is_some()
    );
}

/// Compression (D78): negotiated for text-shaped responses, never for
/// the byte surfaces, and only above the framing-overhead floor.
/// Raw sockets on purpose: ureq's gzip feature transparently decodes
/// AND strips Content-Encoding, hiding exactly what this test pins.
#[test]
fn d78_compression_for_text_surfaces_only() {
    use std::io::{Read as _, Write as _};
    let f = fixture();

    let head = |path: &str, accept: Option<&str>| -> String {
        let mut sock = std::net::TcpStream::connect(f.addr).expect("connect");
        let extra = accept.map_or(String::new(), |v| format!("Accept-Encoding: {v}\r\n"));
        write!(
            sock,
            "GET {path} HTTP/1.1\r\nHost: test\r\n{extra}Connection: close\r\n\r\n"
        )
        .expect("send");
        let mut response = Vec::new();
        sock.read_to_end(&mut response).expect("read");
        let end = response
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .expect("header block");
        String::from_utf8_lossy(&response[..end]).to_ascii_lowercase()
    };

    // The SPA shell is html: gzip when asked...
    let shell = head("/", Some("gzip"));
    assert!(shell.contains("content-encoding: gzip"), "{shell}");
    assert!(
        shell.contains("vary:") && shell.contains("accept-encoding"),
        "caches must key on the negotiated encoding: {shell}"
    );

    // ...and raw when the client didn't ask.
    assert!(!head("/", None).contains("content-encoding"));

    // The traced-path favicon is 54 KB of SVG text: prime material.
    assert!(head("/icon.svg", Some("gzip")).contains("content-encoding: gzip"));

    // Tiny JSON stays raw — below MIN_BYTES the framing outweighs it.
    assert!(!head("/v1/views", Some("gzip")).contains("content-encoding"));

    // The verified byte path stays raw whatever the client asks for.
    let file = head("/view/test/Alpha/alpha.gba", Some("gzip"));
    assert!(!file.contains("content-encoding"), "{file}");
}

/// Browser hardening (D70): the security-header set on every response
/// class, and the Fetch-Metadata CSRF gate on state-changing methods.
#[test]
fn d70_security_headers_and_csrf() {
    // No hash-source: the theme-flash inline script died with the
    // theme toggle (D78) — the dist ships zero inline scripts.
    // 'wasm-unsafe-eval' admits the D84 emulator cores' wasm compile
    // and nothing else (no eval(), no inline script).
    const CSP: &str = "default-src 'self'; \
         script-src 'self' 'wasm-unsafe-eval'; \
         style-src 'self'; img-src 'self' data:; font-src 'self'; \
         connect-src 'self'; object-src 'none'; frame-ancestors 'none'; \
         base-uri 'none'; form-action 'self'";

    let f = fixture();

    // headers ride every response class: the SPA shell, a /v1 JSON
    // answer, and a 404 error alike
    for (path, want_status) in [("/", 200), ("/v1/views", 200), ("/v1/nope", 404)] {
        let (status, resp) = get(f.addr, path, &[]);
        assert_eq!(status, want_status, "{path}");
        assert_eq!(resp.header("content-security-policy"), Some(CSP), "{path}");
        assert_eq!(resp.header("x-content-type-options"), Some("nosniff"));
        assert_eq!(resp.header("referrer-policy"), Some("no-referrer"));
        assert_eq!(
            resp.header("cross-origin-opener-policy"),
            Some("same-origin")
        );
        // COOP+COEP → crossOriginIsolated (the emu lane's SAB headroom).
        assert_eq!(
            resp.header("cross-origin-embedder-policy"),
            Some("require-corp")
        );
        assert_eq!(
            resp.header("cross-origin-resource-policy"),
            Some("same-origin")
        );
        assert_eq!(resp.header("x-frame-options"), Some("DENY"));
        assert_eq!(
            resp.header("permissions-policy"),
            Some("camera=(), geolocation=(), microphone=(), payment=(), usb=()")
        );
        assert!(
            resp.header("strict-transport-security").is_none(),
            "no HSTS (plain-HTTP LAN)"
        );
    }

    // /v1 JSON is live per-identity state: it must never be cached
    let (_, resp) = get(f.addr, "/v1/views", &[]);
    assert_eq!(resp.header("cache-control"), Some("no-store"));

    // CSRF: a cross-site POST is rejected before the handler — even
    // over loopback (the DNS-rebinding case D70 exists for)
    let creds = r#"{"username":"pal","password":"wrongwrong"}"#;
    let (status, resp) = post_json(
        f.addr,
        "/v1/auth/login",
        creds,
        &[("Sec-Fetch-Site", "cross-site")],
    );
    assert_eq!(status, 403);
    let v: serde_json::Value =
        serde_json::from_str(&resp.into_string().expect("json")).expect("typed error shape");
    assert_eq!(v["error"], "cross-origin request rejected");

    // same-origin fetch (the SPA's own login call) reaches the handler:
    // normal invalid-credentials 401, not a 403
    let (status, _) = post_json(
        f.addr,
        "/v1/auth/login",
        creds,
        &[("Sec-Fetch-Site", "same-origin")],
    );
    assert_eq!(status, 401);

    // pre-Fetch-Metadata browser: Origin matching Host passes through
    let (status, _) = post_json(
        f.addr,
        "/v1/auth/login",
        creds,
        &[("Origin", &format!("http://{}", f.addr))],
    );
    assert_eq!(status, 401);

    // ...and a mismatched Origin rejects
    let (status, _) = post_json(
        f.addr,
        "/v1/auth/login",
        creds,
        &[("Origin", "http://evil.example")],
    );
    assert_eq!(status, 403);
}
