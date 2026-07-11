//! End-to-end /v1 read-model + admin API against a live daemon over a
//! small REAL universe: one imported dat (three entries: have /
//! missing / nodump), one ingested blob, one evaluated view. Loopback
//! is implicitly owner (D68), so everything here runs as the owner;
//! the friend-visibility matrix is unit-tested in api.rs where a
//! friend Caller can actually be presented.

use std::net::SocketAddr;
use std::str::FromStr as _;

use datboi_catalog::{ImportOptions, ViewDef, define_view, evaluate_view, import_dat};
use datboi_core::alias::AliasHasher;
use datboi_core::hash::Blake3;
use datboi_index::{Db, Namespace as IxNs, Residency};
use datboi_server::{Config, Server};
use datboi_store_fs::{Namespace as StoreNs, Store};

const ALPHA: &[u8] = b"alpha rom content";

struct Fixture {
    addr: SocketAddr,
    system_id: i64,
    snapshot: String,
    db_dir: std::path::PathBuf,
    _root: tempfile::TempDir,
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn fixture() -> Fixture {
    let root = tempfile::tempdir().expect("tempdir");
    let store_root = root.path().join("store");
    let db_dir = root.path().join("db");
    std::fs::create_dir_all(&db_dir).expect("db dir");
    let store = Store::open(&store_root).expect("store");
    let mut db = Db::open(&db_dir).expect("db");

    // Ingest ALPHA the way the pipeline does: literal blob + alias
    // tuple + store-verification stamp.
    let (alpha_hash, aliases, _) = store.put_new(StoreNs::Data, ALPHA).expect("put");
    let blob_id = db
        .upsert_blob(
            &alpha_hash,
            Some(ALPHA.len() as u64),
            IxNs::Data,
            Residency::Resident,
        )
        .expect("blob");
    db.insert_aliases(blob_id, &aliases).expect("aliases");
    db.set_verified(blob_id, 999).expect("verified");

    // A three-state dat: Alpha is held, Beta is missing, Gamma was
    // never dumped.
    let mut hasher = AliasHasher::new();
    hasher.update(ALPHA);
    let alpha_tuple = hasher.finalize();
    let dat = format!(
        r#"<?xml version="1.0"?>
<!DOCTYPE datafile PUBLIC "-//Logiqx//DTD ROM Management Datafile//EN" "http://www.logiqx.com/Dats/datafile.dtd">
<datafile><header><name>gba</name><description>Test dat</description><version>r1</version><author>no-intro</author></header>
<game name="Alpha (USA)"><description>Alpha</description><rom name="Alpha (USA).gba" size="{alpha_size}" crc="{alpha_crc}" sha1="{alpha_sha1}"/></game>
<game name="Beta (Japan)"><description>Beta</description><rom name="Beta (Japan).gba" size="5" crc="12345678" sha1="000102030405060708090a0b0c0d0e0f10111213"/></game>
<game name="Gamma (USA)"><description>Gamma</description><rom name="Gamma (USA).gba" status="nodump"/></game>
</datafile>"#,
        alpha_size = ALPHA.len(),
        alpha_crc = hex(&alpha_tuple.crc32),
        alpha_sha1 = hex(&alpha_tuple.sha1),
    );
    let report = import_dat(
        &store,
        &mut db,
        dat.as_bytes(),
        &ImportOptions {
            provider: Some("no-intro"),
            system: Some("gba"),
            imported_at: 1_000,
        },
    )
    .expect("import");

    // One evaluated view over the source: Alpha's blob lands in it.
    let def = ViewDef {
        name: "gba".into(),
        provider: "no-intro".into(),
        system: "gba".into(),
        template: "Games/{name}".into(),
        selection: None,
        profile: None,
        image: None,
        mame: None,
    };
    define_view(&db, &def).expect("define");
    let eval = evaluate_view(&mut db, &store, &def, 1_780_000_000).expect("eval");
    assert_eq!(eval.rows, 1, "only Alpha is held");
    drop(db); // the daemon opens its own handles

    let server = Server::bind(&Config {
        store_root,
        db_dir: db_dir.clone(),
        listen: SocketAddr::from_str("127.0.0.1:0").expect("addr"),
        nfs_listen: None,
    })
    .expect("bind");
    let addr = server.local_addr().expect("addr");
    std::thread::spawn(move || server.serve());
    Fixture {
        addr,
        system_id: report.source_id,
        snapshot: eval.snapshot.to_hex(),
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
                std::thread::sleep(std::time::Duration::from_millis(40));
            }
        }
    }
    panic!("server never came up at {addr}");
}

fn request(addr: SocketAddr, method: &str, path: &str, body: &str) -> (u16, serde_json::Value) {
    let agent = ureq::AgentBuilder::new().redirects(0).build();
    let req = agent
        .request(method, &format!("http://{addr}{path}"))
        .set("Content-Type", "application/json");
    let result = if body.is_empty() {
        req.call()
    } else {
        req.send_string(body)
    };
    match result {
        Ok(resp) => (resp.status(), parse(resp)),
        Err(ureq::Error::Status(code, resp)) => (code, parse(resp)),
        Err(e) => panic!("{method} {path} transport error: {e}"),
    }
}

fn parse(resp: ureq::Response) -> serde_json::Value {
    let text = resp.into_string().expect("body");
    serde_json::from_str(&text).unwrap_or(serde_json::Value::String(text))
}

#[test]
fn read_models_end_to_end() {
    let f = fixture();

    // ---- /v1/systems ----
    let (status, v) = get(f.addr, "/v1/systems");
    assert_eq!(status, 200);
    let systems = v["systems"].as_array().expect("systems");
    assert_eq!(systems.len(), 1);
    let sys = &systems[0];
    assert_eq!(sys["id"], f.system_id);
    assert_eq!(sys["provider"], "no-intro");
    assert_eq!(sys["system"], "gba");
    assert_eq!(sys["source"], "no-intro/gba");
    assert_eq!(sys["counts"]["verified"], 1);
    assert_eq!(sys["counts"]["claimed"], 0);
    assert_eq!(sys["counts"]["missing"], 1);
    assert_eq!(sys["counts"]["nodump"], 1);
    assert_eq!(sys["total"], 3);
    assert_eq!(sys["views"][0], "gba");
    assert_eq!(sys["revision"]["version"], "r1");
    assert_eq!(sys["revision"]["imported_at"], 1_000);

    // ---- /v1/systems/{id}/entries: listing, filter, search, paging ----
    let base = format!("/v1/systems/{}/entries", f.system_id);
    let (status, v) = get(f.addr, &base);
    assert_eq!(status, 200);
    assert_eq!(v["total"], 3);
    assert_eq!(v["limit"], 200, "default limit");
    let entries = v["entries"].as_array().expect("entries");
    let names: Vec<&str> = entries
        .iter()
        .map(|e| e["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        ["Alpha (USA)", "Beta (Japan)", "Gamma (USA)"],
        "name-ordered"
    );
    assert_eq!(entries[0]["state"], "verified");
    assert_eq!(entries[0]["size"], ALPHA.len() as u64);
    assert_eq!(entries[0]["wanted_hash_algo"], "sha1");
    assert_eq!(
        entries[0]["wanted_hash"].as_str().expect("hex").len(),
        40,
        "full hex, UI truncates"
    );
    assert_eq!(entries[1]["state"], "missing");
    assert_eq!(entries[2]["state"], "nodump");
    assert_eq!(entries[2]["size"], serde_json::Value::Null);
    assert_eq!(entries[2]["wanted_hash"], serde_json::Value::Null);

    let (status, v) = get(f.addr, &format!("{base}?state=missing"));
    assert_eq!(status, 200);
    assert_eq!(v["total"], 1);
    assert_eq!(v["entries"][0]["name"], "Beta (Japan)");

    // case-insensitive substring search
    let (_, v) = get(f.addr, &format!("{base}?q=alpha"));
    assert_eq!(v["total"], 1);
    assert_eq!(v["entries"][0]["name"], "Alpha (USA)");
    let (_, v) = get(f.addr, &format!("{base}?q=%28usa%29"));
    assert_eq!(v["total"], 2, "percent-decoded, matches parentheticals");

    // pagination: window slides, total stays the filter's count
    let (_, v) = get(f.addr, &format!("{base}?limit=1&offset=1"));
    assert_eq!(v["total"], 3);
    assert_eq!(v["entries"].as_array().unwrap().len(), 1);
    assert_eq!(v["entries"][0]["name"], "Beta (Japan)");
    assert_eq!(
        (v["limit"].as_u64(), v["offset"].as_u64()),
        (Some(1), Some(1))
    );

    // parameter rejections + unknown system
    assert_eq!(get(f.addr, &format!("{base}?state=bogus")).0, 400);
    assert_eq!(get(f.addr, &format!("{base}?limit=abc")).0, 400);
    let (status, v) = get(f.addr, "/v1/systems/999/entries");
    assert_eq!(status, 404);
    assert_eq!(v["error"], "no such system");

    // ---- entry detail ----
    let (status, v) = get(f.addr, &format!("{base}/Alpha%20%28USA%29"));
    assert_eq!(status, 200);
    assert_eq!(v["name"], "Alpha (USA)");
    assert_eq!(v["state"], "verified");
    assert_eq!(v["size"], ALPHA.len() as u64);
    assert_eq!(v["revision"]["version"], "r1");
    let rom = &v["roms"][0];
    assert_eq!(rom["name"], "Alpha (USA).gba");
    assert_eq!(rom["state"], "verified");
    assert_eq!(rom["hashes"]["sha1"], v["wanted_hash"]);
    assert_eq!(rom["blob"]["hash"], Blake3::compute(ALPHA).to_hex());
    assert_eq!(rom["blob"]["residency"], "resident");
    assert_eq!(rom["blob"]["verified_at"], 999);
    assert_eq!(rom["routes"], serde_json::json!([]), "literal: no routes");
    assert_eq!(rom["pins"][0], "gba", "held by the evaluated view");

    let (_, v) = get(f.addr, &format!("{base}/Beta%20%28Japan%29"));
    assert_eq!(v["state"], "missing");
    let rom = &v["roms"][0];
    assert_eq!(rom["state"], "missing");
    assert!(rom.get("blob").is_none(), "no bytes, no blob key: {rom}");

    let (status, v) = get(f.addr, &format!("{base}/Nope"));
    assert_eq!(status, 404);
    assert_eq!(v["error"], "no such entry");

    // ---- /v1/views + detail ----
    let (status, v) = get(f.addr, "/v1/views");
    assert_eq!(status, 200);
    let view = &v["views"][0];
    assert_eq!(view["name"], "gba");
    assert_eq!(view["snapshot"], f.snapshot.as_str());
    assert_eq!(view["rows"], 1);
    assert_eq!(view["bytes"], ALPHA.len() as u64);
    assert_eq!(view["created_at"], 1_780_000_000u64);
    assert_eq!(view["definition"]["provider"], "no-intro");
    assert_eq!(view["definition"]["system"], "gba");
    assert_eq!(view["definition"]["one_g_one_r"], serde_json::Value::Null);
    assert_eq!(view["definition"]["image"], serde_json::Value::Null);

    let (status, v) = get(f.addr, "/v1/views/gba");
    assert_eq!(status, 200);
    assert_eq!(v["endpoints"]["http"], "/view/gba/");
    assert_eq!(v["endpoints"]["dav"], "/dav/gba/");
    assert_eq!(v["image"], serde_json::Value::Null, "no image profile");
    assert_eq!(v["snapshot"], f.snapshot.as_str());
    let (status, v) = get(f.addr, "/v1/views/nope");
    assert_eq!(status, 404);
    assert_eq!(v["error"], "no such view");

    // ---- /v1/storage ----
    let (status, v) = get(f.addr, "/v1/storage");
    assert_eq!(status, 200);
    // ALPHA + the dat blob are data/ blobs; the view snapshot is meta/.
    assert_eq!(v["blob_count"], 2);
    let on_disk = v["on_disk_bytes"].as_u64().expect("bytes");
    assert!(on_disk > ALPHA.len() as u64, "{v}");
    assert_eq!(v["represented_bytes"], on_disk, "nothing evicted yet");
    assert_eq!(v["literal_only_bytes"], on_disk, "no rebuild routes yet");
    assert_eq!(v["quarantine"]["count"], 0);

    // ---- /v1/jobs (truthful stub — no registry exists) ----
    let (status, v) = get(f.addr, "/v1/jobs");
    assert_eq!(status, 200);
    assert_eq!(v["jobs"], serde_json::json!([]));
}

#[test]
fn admin_crud_round_trip() {
    let f = fixture();

    // empty universe: no users, no invites
    let (status, v) = get(f.addr, "/v1/admin/users");
    assert_eq!(status, 200);
    assert_eq!(v["users"], serde_json::json!([]));
    assert_eq!(v["invites"], serde_json::json!([]));

    // mint an invite via the API (owner over loopback, D68)
    let (status, v) = request(
        f.addr,
        "POST",
        "/v1/admin/invites",
        r#"{"role":"friend","expires_days":3}"#,
    );
    assert_eq!(status, 200);
    let url_path = v["url_path"].as_str().expect("url_path");
    let token = url_path
        .strip_prefix("/invite#")
        .expect("token rides the fragment");
    assert_eq!(token.len(), 43, "32 bytes base64url");
    let expires_at = v["expires_at"].as_i64().expect("expires_at");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_secs() as i64;
    assert!(
        (expires_at - now - 3 * 24 * 60 * 60).abs() < 60,
        "3 days out"
    );

    // the pending invite shows up for the admin screen
    let (_, v) = get(f.addr, "/v1/admin/users");
    assert_eq!(v["invites"].as_array().expect("invites").len(), 1);
    assert_eq!(v["invites"][0]["role"], "friend");

    // rejections
    assert_eq!(
        request(f.addr, "POST", "/v1/admin/invites", r#"{"role":"god"}"#).0,
        400
    );
    assert_eq!(
        request(f.addr, "POST", "/v1/admin/invites", r#"{"expires_days":0}"#).0,
        400
    );

    // accept the invite exactly like the SPA would
    let (status, _) = request(
        f.addr,
        "POST",
        "/v1/auth/invite/accept",
        &format!(r#"{{"token":"{token}","username":"pal","password":"hunter22"}}"#),
    );
    assert_eq!(status, 200);
    let (_, v) = get(f.addr, "/v1/admin/users");
    assert_eq!(v["invites"], serde_json::json!([]), "invite consumed");
    let user = &v["users"][0];
    assert_eq!(user["username"], "pal");
    assert_eq!(user["role"], "friend");
    assert_eq!(user["sessions"], 1, "acceptance minted a session");
    assert_eq!(user["grants"], serde_json::json!([]));

    // grant the friend the view; the grant is what the ACL reads (D68)
    let (status, _) = request(
        f.addr,
        "POST",
        "/v1/admin/grants",
        r#"{"username":"pal","view":"gba"}"#,
    );
    assert_eq!(status, 200);
    let (_, v) = get(f.addr, "/v1/admin/users");
    assert_eq!(v["users"][0]["grants"], serde_json::json!(["gba"]));
    // ...and it is now live in the same view_grant table the HTTP ACL
    // consults (the friend-facing filter itself is unit-tested in
    // api.rs — loopback peers are always the owner, D68).
    let db = Db::open(&f.db_dir).expect("second handle");
    let pal = db.user_by_name("pal").expect("q").expect("exists");
    assert_eq!(db.grants_for_user(pal.user_id).expect("q"), ["gba"]);

    // grants on unknown users/views refuse
    let (status, v) = request(
        f.addr,
        "POST",
        "/v1/admin/grants",
        r#"{"username":"ghost","view":"gba"}"#,
    );
    assert_eq!((status, v["error"].as_str()), (404, Some("no such user")));
    let (status, v) = request(
        f.addr,
        "POST",
        "/v1/admin/grants",
        r#"{"username":"pal","view":"nope"}"#,
    );
    assert_eq!((status, v["error"].as_str()), (404, Some("no such view")));

    // revoke: once real, once already-gone
    let (status, _) = request(f.addr, "DELETE", "/v1/admin/grants/pal/gba", "");
    assert_eq!(status, 200);
    assert_eq!(
        request(f.addr, "DELETE", "/v1/admin/grants/pal/gba", "").0,
        404
    );
    assert_eq!(db.grants_for_user(pal.user_id).expect("q").len(), 0);

    // revoke every session
    let (status, v) = request(f.addr, "DELETE", "/v1/admin/sessions/pal", "");
    assert_eq!(status, 200);
    assert_eq!(v["revoked"], 1);
    let (_, v) = get(f.addr, "/v1/admin/users");
    assert_eq!(v["users"][0]["sessions"], 0);

    // mint + revoke a pending invite by its stored hash
    let (_, v) = request(f.addr, "POST", "/v1/admin/invites", "{}");
    assert!(
        v["url_path"]
            .as_str()
            .expect("minted")
            .starts_with("/invite#")
    );
    let (_, v) = get(f.addr, "/v1/admin/users");
    let hash_hex = v["invites"][0]["token_hash"].as_str().expect("hash");
    let (status, _) = request(
        f.addr,
        "DELETE",
        &format!("/v1/admin/invites/{hash_hex}"),
        "",
    );
    assert_eq!(status, 200);
    let (status, _) = request(
        f.addr,
        "DELETE",
        &format!("/v1/admin/invites/{hash_hex}"),
        "",
    );
    assert_eq!(status, 404, "already revoked");
    assert_eq!(
        request(f.addr, "DELETE", "/v1/admin/invites/nothex", "").0,
        400
    );
    let (_, v) = get(f.addr, "/v1/admin/users");
    assert_eq!(v["invites"], serde_json::json!([]));
}
