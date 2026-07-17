//! End-to-end /v1 read-model + admin API against a live daemon over a
//! small REAL universe: one imported dat (three entries: have /
//! missing / nodump), one ingested blob, one evaluated view. Loopback
//! is implicitly owner (D68), so everything here runs as the owner;
//! the friend-visibility matrix is unit-tested in api.rs where a
//! friend Caller can actually be presented.

use std::io::Read as _;
use std::net::SocketAddr;
use std::str::FromStr as _;

use datboi_catalog::{
    ImageParams, ImportOptions, ViewDef, define_view, evaluate_view, import_dat, mint_image,
};
use datboi_core::alias::AliasHasher;
use datboi_core::hash::Blake3;
use datboi_core::viewsnap::ViewSnapshot;
use datboi_index::{Db, Namespace as IxNs, Residency};
use datboi_server::{Config, Server};
use datboi_store_fs::{Namespace as StoreNs, Store};

const ALPHA: &[u8] = b"alpha rom content";

struct Fixture {
    addr: SocketAddr,
    system_id: i64,
    snapshot: String,
    /// (blake3 hex, byte size) of the minted `gba-sd` image, when the
    /// fixture was built `with_image`.
    image: Option<(String, u64)>,
    db_dir: std::path::PathBuf,
    _root: tempfile::TempDir,
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn fixture() -> Fixture {
    fixture_ext(false)
}

fn fixture_ext(with_image: bool) -> Fixture {
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
    // Rescan-cache provenance — what the blob inspector reports.
    db.upsert_source_file("roms/alpha.gba", 0, ALPHA.len() as u64, Some(blob_id), 998)
        .expect("source file");

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

    // Optionally a second, image-bearing view (D62): defined with
    // image params, evaluated, minted — the way `datboi view image`
    // does it. 512-byte clusters keep the FAT32 floor (65,525
    // clusters) at ~35 MB instead of the default's gigabytes.
    let image = with_image.then(|| {
        let params = ImageParams {
            cluster_size: 512,
            ..ImageParams::default()
        };
        let def = ViewDef {
            name: "gba-sd".into(),
            provider: "no-intro".into(),
            system: "gba".into(),
            template: "Games/{name}".into(),
            selection: None,
            profile: None,
            image: Some(params.clone()),
            mame: None,
        };
        define_view(&db, &def).expect("define");
        let eval = evaluate_view(&mut db, &store, &def, 1_780_000_000).expect("eval");
        let mut snap_bytes = Vec::new();
        store
            .get(StoreNs::Meta, &eval.snapshot)
            .expect("get snapshot")
            .expect("snapshot blob")
            .read_to_end(&mut snap_bytes)
            .expect("read snapshot");
        let snap = ViewSnapshot::decode(&snap_bytes).expect("decode snapshot");
        let report = mint_image(
            &mut db,
            &store,
            "gba-sd",
            &eval.snapshot,
            &snap,
            &params,
            true,
            1_780_000_100,
        )
        .expect("mint");
        (report.image.to_hex(), report.size)
    });
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
        system_id: report.source_id,
        snapshot: eval.snapshot.to_hex(),
        image,
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

/// POST raw bytes (the dat-import upload shape).
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

/// GET returning the raw response (binary bodies, header assertions).
fn get_raw(addr: SocketAddr, path: &str, range: Option<&str>) -> ureq::Response {
    let agent = ureq::AgentBuilder::new().redirects(0).build();
    let mut req = agent.get(&format!("http://{addr}{path}"));
    if let Some(range) = range {
        req = req.set("Range", range);
    }
    match req.call() {
        Ok(resp) | Err(ureq::Error::Status(_, resp)) => resp,
        Err(e) => panic!("GET {path} transport error: {e}"),
    }
}

fn body_bytes(resp: ureq::Response) -> Vec<u8> {
    let mut bytes = Vec::new();
    resp.into_reader()
        .read_to_end(&mut bytes)
        .expect("read body");
    bytes
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

/// The storage introspection surface (owner debug): breakdown
/// aggregates, the blob listing, and the inspector card, over the
/// fixture's tiny real universe (data/: ALPHA + the dat blob; meta/:
/// the view snapshot + no recipes). The DAG-walking half — members and
/// containers — lives in tests/ingest.rs where zips exist.
#[test]
fn storage_breakdown_and_blob_inspector() {
    let f = fixture();
    let alpha_hex = Blake3::compute(ALPHA).to_hex();

    // ---- /v1/storage/breakdown: by_class ----
    let (status, v) = get(f.addr, "/v1/storage/breakdown");
    assert_eq!(status, 200);
    let by_class = v["by_class"].as_array().expect("by_class");
    let data_cell = by_class
        .iter()
        .find(|c| c["namespace"] == "data" && c["residency"] == "resident")
        .unwrap_or_else(|| panic!("no data/resident cell: {v}"));
    assert_eq!(data_cell["blobs"], 2, "ALPHA + the dat blob");
    assert!(data_cell["bytes"].as_i64().expect("bytes") > ALPHA.len() as i64);
    assert_eq!(data_cell["sizeless"], 0);
    assert!(
        by_class
            .iter()
            .any(|c| c["namespace"] == "meta" && c["residency"] == "resident"),
        "the snapshot blob is meta/resident: {v}"
    );

    // by_source (D79: attribution is viral through the recipe DAG):
    // ALPHA attributes to no-intro/gba through its identity link; the
    // dat blob connects to nothing claimed and is truly (unattached).
    let by_source = v["by_source"].as_array().expect("by_source");
    assert_eq!(by_source.len(), 2, "{v}");
    let source_row = |source: &str| {
        by_source
            .iter()
            .find(|s| s["source"] == source)
            .unwrap_or_else(|| panic!("no {source} row: {v}"))
    };
    let gba = source_row("no-intro/gba");
    assert_eq!(gba["blobs"], 1);
    assert_eq!(gba["bytes"], ALPHA.len() as u64);
    let unattached = source_row("(unattached)");
    assert_eq!(unattached["blobs"], 1, "the dat blob");
    assert!(unattached["bytes"].as_i64().expect("bytes") > 0);

    // largest: data blobs only, size DESC — the dat outweighs ALPHA.
    let largest = v["largest"].as_array().expect("largest");
    assert_eq!(largest.len(), 2);
    let alpha_row = &largest[1];
    assert_eq!(alpha_row["hash"], alpha_hex.as_str());
    assert_eq!(alpha_row["size"], ALPHA.len() as u64);
    assert_eq!(alpha_row["sources"], 1, "the rescan-cache row");
    assert_eq!(alpha_row["routes_in"], 0, "a literal: no recipes");
    assert_eq!(alpha_row["routes_out"], 0);

    // ---- /v1/blobs: paging + filters ----
    let (status, v) = get(f.addr, "/v1/blobs");
    assert_eq!(status, 200);
    assert_eq!(v["total"], 3, "2 data + the snapshot: {v}");
    assert_eq!(v["limit"], 200, "default limit");
    let (_, v) = get(f.addr, "/v1/blobs?limit=1&offset=1");
    assert_eq!(v["total"], 3);
    assert_eq!(v["blobs"].as_array().expect("blobs").len(), 1);

    // q: case-insensitive hex-prefix match
    let prefix = alpha_hex[..8].to_uppercase();
    let (_, v) = get(f.addr, &format!("/v1/blobs?q={prefix}"));
    assert_eq!(v["total"], 1, "{v}");
    assert_eq!(v["blobs"][0]["hash"], alpha_hex.as_str());
    assert_eq!(v["blobs"][0]["namespace"], "data");
    assert_eq!(v["blobs"][0]["residency"], "resident");
    assert_eq!(v["blobs"][0]["verified_at"], 999);

    // ns/residency filters + rejections
    let (_, v) = get(f.addr, "/v1/blobs?ns=meta");
    assert_eq!(v["total"], 1, "the view snapshot: {v}");
    let (_, v) = get(f.addr, "/v1/blobs?residency=evicted_covered");
    assert_eq!(v["total"], 0);
    assert_eq!(get(f.addr, "/v1/blobs?ns=bogus").0, 400);
    assert_eq!(get(f.addr, "/v1/blobs?residency=bogus").0, 400);
    assert_eq!(get(f.addr, "/v1/blobs?limit=abc").0, 400);

    // ---- /v1/blobs/{hash}: the literal's inspector card ----
    let (status, v) = get(f.addr, &format!("/v1/blobs/{alpha_hex}"));
    assert_eq!(status, 200);
    assert_eq!(v["hash"], alpha_hex.as_str());
    assert_eq!(v["size"], ALPHA.len() as u64);
    assert_eq!(v["namespace"], "data");
    assert_eq!(v["residency"], "resident");
    assert_eq!(v["verified_at"], 999);
    // digests: the blake3 key plus the recorded alias tuple
    let mut hasher = AliasHasher::new();
    hasher.update(ALPHA);
    let tuple = hasher.finalize();
    assert_eq!(v["digests"]["blake3"], alpha_hex.as_str());
    assert_eq!(v["digests"]["crc32"], hex(&tuple.crc32));
    assert_eq!(v["digests"]["md5"], hex(&tuple.md5));
    assert_eq!(v["digests"]["sha1"], hex(&tuple.sha1));
    assert_eq!(v["digests"]["sha256"], hex(&tuple.sha256));
    // provenance from the rescan cache
    assert_eq!(v["provenance"][0]["path"], "roms/alpha.gba");
    assert_eq!(v["provenance"][0]["ingested_at"], 998);
    // a loose literal has no DAG neighborhood
    assert_eq!(v["routes_in"], serde_json::json!([]));
    assert_eq!(v["routes_out"], serde_json::json!([]));
    // claims + pins
    assert_eq!(v["claims_total"], 1);
    assert_eq!(v["claims"][0]["entry"], "Alpha (USA)");
    assert_eq!(v["claims"][0]["source"], "no-intro/gba");
    assert_eq!(v["pins"], serde_json::json!(["gba"]));

    // uppercase hex answers the same card
    let upper = alpha_hex.to_uppercase();
    assert_eq!(get(f.addr, &format!("/v1/blobs/{upper}")).0, 200);

    // misses: unknown hash 404, non-hex 400
    let zeros = "0".repeat(64);
    let (status, v) = get(f.addr, &format!("/v1/blobs/{zeros}"));
    assert_eq!(status, 404);
    assert_eq!(v["error"], "no such blob");
    assert_eq!(get(f.addr, "/v1/blobs/nothex").0, 400);
}

/// The M5 friend-surface additions, owner path: the flat files listing
/// (`/v1/views/{name}/files`) and the minted-image download
/// (`/v1/views/{name}/image`). Friend gating for both is unit-tested
/// in api.rs/http.rs terms (loopback peers are always the owner, D68);
/// this exercises the full HTTP semantics.
#[test]
fn view_files_and_image_end_to_end() {
    let f = fixture_ext(true);
    let (image_hex, image_size) = f.image.clone().expect("minted");

    // ---- files listing: rows, q, paging, misses ----
    let (status, v) = get(f.addr, "/v1/views/gba/files");
    assert_eq!(status, 200);
    assert_eq!(v["total"], 1);
    assert_eq!(v["snapshot"], f.snapshot.as_str());
    let row = &v["files"][0];
    assert_eq!(row["path"], "Games/Alpha (USA).gba");
    assert_eq!(row["size"], ALPHA.len() as u64);
    assert_eq!(row["hash"], Blake3::compute(ALPHA).to_hex());

    // q is case-insensitive substring over the full path
    let (_, v) = get(f.addr, "/v1/views/gba/files?q=games%2Falpha");
    assert_eq!(v["total"], 1);
    let (_, v) = get(f.addr, "/v1/views/gba/files?q=zelda");
    assert_eq!(v["total"], 0);
    assert_eq!(v["files"], serde_json::json!([]));

    // the window slides under the filtered total
    let (_, v) = get(f.addr, "/v1/views/gba/files?offset=1&limit=1");
    assert_eq!(v["total"], 1);
    assert_eq!(v["files"], serde_json::json!([]));
    assert_eq!(
        (v["offset"].as_u64(), v["limit"].as_u64()),
        (Some(1), Some(1))
    );

    assert_eq!(get(f.addr, "/v1/views/gba/files?limit=abc").0, 400);
    let (status, v) = get(f.addr, "/v1/views/nope/files");
    assert_eq!(status, 404);
    assert_eq!(v["error"], "no such view");

    // ---- view detail reports the mint ----
    let (_, v) = get(f.addr, "/v1/views/gba-sd");
    assert_eq!(v["image"]["minted"], true);
    assert_eq!(v["image"]["hash"], image_hex.as_str());
    assert_eq!(v["image"]["bytes"], image_size);

    // ---- image download: full body hashes to the minted image ----
    let resp = get_raw(f.addr, "/v1/views/gba-sd/image", None);
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.header("content-disposition"),
        Some("attachment; filename=\"gba-sd.img\"")
    );
    assert_eq!(resp.header("accept-ranges"), Some("bytes"));
    assert_eq!(
        resp.header("etag"),
        Some(format!("\"{image_hex}\"").as_str())
    );
    let whole = body_bytes(resp);
    assert_eq!(whole.len() as u64, image_size);
    assert_eq!(Blake3::compute(&whole).to_hex(), image_hex);

    // ---- Range: same verified path, same bytes ----
    let resp = get_raw(f.addr, "/v1/views/gba-sd/image", Some("bytes=512-1023"));
    assert_eq!(resp.status(), 206);
    assert_eq!(
        resp.header("content-range"),
        Some(format!("bytes 512-1023/{image_size}").as_str())
    );
    assert_eq!(body_bytes(resp), whole[512..1024]);

    // ---- misses: no mint on `gba`, unknown view — identical 404s ----
    assert_eq!(get_raw(f.addr, "/v1/views/gba/image", None).status(), 404);
    assert_eq!(get_raw(f.addr, "/v1/views/nope/image", None).status(), 404);
}

/// POST /v1/dats/import — the web upload path, same operation as
/// `datboi dat import` (dats.rs). Loopback is implicitly owner (D68);
/// the friend 403 is covered by require_owner's unit tests.
#[test]
fn dat_import_end_to_end() {
    let f = fixture();

    // A fresh source: provider/system resolve from the dat header.
    let psx_dat = r#"<?xml version="1.0"?>
<!DOCTYPE datafile PUBLIC "-//Logiqx//DTD ROM Management Datafile//EN" "http://www.logiqx.com/Dats/datafile.dtd">
<datafile><header><name>psx</name><description>Test dat</description><version>r7</version><author>redump</author></header>
<game name="Delta (Europe)"><description>Delta</description><rom name="Delta (Europe).bin" size="4" crc="deadbeef" sha1="0102030405060708090a0b0c0d0e0f1011121314"/></game>
</datafile>"#;
    let (status, v) = post_bytes(f.addr, "/v1/dats/import", psx_dat.as_bytes());
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["provider"], "redump");
    assert_eq!(v["system"], "psx");
    assert_eq!(v["entries"], 1);
    assert_eq!(v["claims"], 1);
    assert_eq!(v["demoted_revisions"], serde_json::json!([]));
    assert_eq!(
        v["dat_blob"].as_str().expect("hex").len(),
        64,
        "blake3 hex of the stored dat blob"
    );
    let psx_id = v["source_id"].as_i64().expect("source_id");
    assert_ne!(psx_id, f.system_id, "a new dat source");

    // ...and it is live in the read model, rollups included.
    let (_, v) = get(f.addr, "/v1/systems");
    let systems = v["systems"].as_array().expect("systems");
    assert_eq!(systems.len(), 2);
    let psx = systems
        .iter()
        .find(|s| s["id"] == psx_id)
        .expect("imported system listed");
    assert_eq!(psx["source"], "redump/psx");
    assert_eq!(psx["counts"]["missing"], 1);
    assert_eq!(psx["revision"]["version"], "r7");

    // Query overrides beat the header (percent-decoded like the rest
    // of the query surface).
    let (status, v) = post_bytes(
        f.addr,
        "/v1/dats/import?provider=my%20mirror&system=psx-usa",
        psx_dat.as_bytes(),
    );
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["provider"], "my mirror");
    assert_eq!(v["system"], "psx-usa");
    assert_ne!(
        v["source_id"].as_i64(),
        Some(psx_id),
        "override = new source"
    );

    // A newer revision of the fixture's source lands on the SAME
    // source and becomes current.
    let gba_r2 = r#"<?xml version="1.0"?>
<!DOCTYPE datafile PUBLIC "-//Logiqx//DTD ROM Management Datafile//EN" "http://www.logiqx.com/Dats/datafile.dtd">
<datafile><header><name>gba</name><description>Test dat</description><version>r2</version><author>no-intro</author></header>
<game name="Alpha (USA)"><description>Alpha</description><rom name="Alpha (USA).gba" size="5" crc="12345678" sha1="000102030405060708090a0b0c0d0e0f10111213"/></game>
</datafile>"#;
    let (status, v) = post_bytes(f.addr, "/v1/dats/import", gba_r2.as_bytes());
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["source_id"], f.system_id);
    let (_, v) = get(f.addr, "/v1/systems");
    let gba = v["systems"]
        .as_array()
        .expect("systems")
        .iter()
        .find(|s| s["id"] == f.system_id)
        .expect("fixture system")
        .clone();
    assert_eq!(gba["revision"]["version"], "r2", "import flipped current");

    // Rejections: not a dat (400, the caller's bytes), empty body.
    let (status, v) = post_bytes(f.addr, "/v1/dats/import", b"definitely not a dat");
    assert_eq!(status, 400, "{v}");
    let (status, v) = post_bytes(f.addr, "/v1/dats/import", b"");
    assert_eq!(
        (status, v["error"].as_str()),
        (400, Some("empty request body"))
    );
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

/// D96: the scrub verb reaches the HTTP surface. A full-sample scrub
/// over the fixture's one resident blob starts a Scrub job, runs the
/// descended `Executor::scrub` on a private connection, and finishes
/// clean — the note carrying the checked/refreshed counts.
#[test]
fn scrub_over_http() {
    use std::time::{Duration, Instant};
    let f = fixture();

    // Bad sample is a typed 400, before any job is created.
    let (status, _) = request(f.addr, "POST", "/v1/scrub", r#"{"sample_pct":150}"#);
    assert_eq!(status, 400, "sample_pct out of range");

    // Defaults ({} → 100% sample, no rehab): a 202 with a job id.
    let (status, v) = request(f.addr, "POST", "/v1/scrub", "{}");
    assert_eq!(status, 202, "{v}");
    let job = v["job"].as_i64().expect("job id");

    // Poll to completion (one small blob is near-instant).
    let deadline = Instant::now() + Duration::from_secs(10);
    let done = loop {
        let (status, v) = get(f.addr, &format!("/v1/jobs/{job}"));
        assert_eq!(status, 200, "{v}");
        if v["state"] != "running" {
            break v;
        }
        assert!(Instant::now() < deadline, "scrub never finished: {v}");
        std::thread::sleep(Duration::from_millis(50));
    };
    assert_eq!(done["state"], "done", "{done}");
    assert_eq!(done["kind"], "scrub", "{done}");
    let notes = done["report"]["notes"].as_array().expect("notes");
    let summary = notes[0].as_str().expect("summary note");
    assert!(summary.contains("0 corrupt, 0 missing"), "{summary}");
    assert!(summary.contains("refreshed"), "{summary}");
}

/// D101: the p2p surface on a daemon WITHOUT `--p2p` — status honestly
/// says disabled (null endpoint id), and sync is a clean typed 503
/// before any job is created (outbound rides the seedbox endpoint;
/// no seedbox, no lane). The enabled path is proven end-to-end in
/// datboi-p2p's own sync tests over real QUIC.
#[test]
fn p2p_surface_without_p2p() {
    let f = fixture();

    let (status, v) = get(f.addr, "/v1/p2p");
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["enabled"], serde_json::Value::Bool(false), "{v}");
    assert!(v["endpoint_id"].is_null(), "{v}");

    let (status, v) = request(
        f.addr,
        "POST",
        "/v1/p2p/sync",
        r#"{"peer":"pretend-peer-id"}"#,
    );
    assert_eq!(status, 503, "{v}");
    assert_eq!(v["code"], "busy", "{v}");
    assert!(
        v["error"].as_str().expect("detail").contains("--p2p"),
        "{v}"
    );

    // No stillborn job rows from the refusal.
    let (status, v) = get(f.addr, "/v1/jobs");
    assert_eq!(status, 200);
    assert!(
        v["jobs"]
            .as_array()
            .expect("jobs")
            .iter()
            .all(|j| j["kind"] != "sync"),
        "{v}"
    );
}

/// D96: the snapshot verb reaches the HTTP surface. `POST /v1/snapshot`
/// mints on demand (the manual trigger beside D75's auto-cadence) and
/// answers the mint report — a real sequence and the object hash.
#[test]
fn snapshot_over_http() {
    let f = fixture();
    let (status, v) = request(f.addr, "POST", "/v1/snapshot", "");
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["hash"].as_str().expect("hash").len(), 64, "{v}");
    assert!(v["sequence"].as_i64().expect("sequence") >= 1, "{v}");
    // The fixture imported one dat source.
    assert_eq!(v["sources"], 1, "{v}");
}

/// D96: the evict verb reaches the HTTP surface. The fixture's one blob
/// is a plain literal with no rebuild route, so nothing is evictable —
/// the dry-run plan is empty, and a real run finishes clean (0 dropped)
/// under the D72 guard. Both the preview and the guarded job path.
#[test]
fn evict_over_http() {
    use std::time::{Duration, Instant};
    let f = fixture();

    // Dry-run: the D27 preview surface, a synchronous read.
    let (status, plan) = request(f.addr, "POST", "/v1/evict", r#"{"target_bytes":0,"dry_run":true}"#);
    assert_eq!(status, 200, "{plan}");
    assert_eq!(plan["evictable"], 0, "no rebuildable literals: {plan}");
    assert!(plan["blocked"].is_array(), "{plan}");
    assert!(plan["reclaimable_bytes"].is_u64(), "{plan}");

    // Real run: claims the guard, starts a Gc job, finishes with nothing
    // to drop (the literal has no route, so it is never a candidate).
    let (status, v) = request(f.addr, "POST", "/v1/evict", r#"{"target_bytes":0}"#);
    assert_eq!(status, 202, "{v}");
    let job = v["job"].as_i64().expect("job id");
    let deadline = Instant::now() + Duration::from_secs(10);
    let done = loop {
        let (status, v) = get(f.addr, &format!("/v1/jobs/{job}"));
        assert_eq!(status, 200, "{v}");
        if v["state"] != "running" {
            break v;
        }
        assert!(Instant::now() < deadline, "evict never finished: {v}");
        std::thread::sleep(Duration::from_millis(50));
    };
    assert_eq!(done["state"], "done", "{done}");
    assert_eq!(done["kind"], "gc", "{done}");
    let note = done["report"]["notes"][0].as_str().expect("note");
    assert!(note.starts_with("0 blob(s) evicted"), "{note}");
}

/// D96: the sweep verb reaches the HTTP surface. An unknown analyzer is
/// a clean 400; a `noop` sweep over the fixture's one blob runs as a
/// background Refine job and finishes with its outcome counts — the same
/// vocabulary the CLI `sweep` verb accepts, from the shared factory.
#[test]
fn sweep_over_http() {
    use std::time::{Duration, Instant};
    let f = fixture();

    let (status, _) = request(f.addr, "POST", "/v1/sweep", r#"{"analyzer":"bogus"}"#);
    assert_eq!(status, 400, "unknown analyzer");

    let (status, v) = request(f.addr, "POST", "/v1/sweep", r#"{"analyzer":"noop","limit":100}"#);
    assert_eq!(status, 202, "{v}");
    let job = v["job"].as_i64().expect("job id");
    let deadline = Instant::now() + Duration::from_secs(10);
    let done = loop {
        let (status, v) = get(f.addr, &format!("/v1/jobs/{job}"));
        assert_eq!(status, 200, "{v}");
        if v["state"] != "running" {
            break v;
        }
        assert!(Instant::now() < deadline, "sweep never finished: {v}");
        std::thread::sleep(Duration::from_millis(50));
    };
    assert_eq!(done["state"], "done", "{done}");
    assert_eq!(done["kind"], "refine", "{done}");
    let note = done["report"]["notes"][0].as_str().expect("note");
    assert!(note.contains("analyzed") && note.contains("queued"), "{note}");
}

/// D96: the dat fetch verb reaches the HTTP surface. A bad source is a
/// clean 400; a real fetch pulls a bare dat from a one-shot upstream and
/// runs it through the normal import path (D15), answering the resolved
/// URL plus the import receipt.
#[test]
fn dat_fetch_over_http() {
    use std::io::Write as _;
    let f = fixture();

    // Bad source: rejected before any network I/O.
    let (status, _) = request(f.addr, "POST", "/v1/dats/fetch", r#"{"source":"not-a-url"}"#);
    assert_eq!(status, 400, "bad source");

    // One-shot upstream serving a bare Logiqx dat.
    let dat = r#"<?xml version="1.0"?>
<datafile><header><name>fetched</name></header>
<game name="Zeta"><description>Zeta</description><rom name="Zeta.gba" size="3" crc="00000000" sha1="0000000000000000000000000000000000000000"/></game>
</datafile>"#;
    let body = dat.as_bytes().to_vec();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut buf = [0u8; 4096];
        let _ = std::io::Read::read(&mut stream, &mut buf);
        let head = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/xml\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        stream.write_all(head.as_bytes()).unwrap();
        stream.write_all(&body).unwrap();
    });
    let src = format!("http://127.0.0.1:{port}/datfile/");
    let req_body = format!(r#"{{"source":"{src}","provider":"fetchprov","system":"fetchsys"}}"#);
    let (status, v) = request(f.addr, "POST", "/v1/dats/fetch", &req_body);
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["url"], src, "{v}");
    assert_eq!(v["import"]["provider"], "fetchprov", "{v}");
    assert_eq!(v["import"]["system"], "fetchsys", "{v}");
    assert!(v["import"]["entries"].as_u64().expect("entries") >= 1, "{v}");
}

/// D96: the dat diff / export / clonelist verbs reach the HTTP surface.
/// Export streams the current revision as XML; diff needs two revisions
/// (the fixture has one → 400) and 404s an unknown source; clonelist
/// links a retool JSON body.
#[test]
fn dat_diff_export_clonelist_over_http() {
    let f = fixture();

    // Export: the current revision as an XML download.
    let resp = get_raw(f.addr, "/v1/dats/no-intro/gba/export", None);
    assert_eq!(resp.status(), 200);
    assert!(
        resp.header("content-type").unwrap_or("").contains("xml"),
        "content type"
    );
    let xml = String::from_utf8(body_bytes(resp)).expect("utf8");
    assert!(xml.contains("Alpha"), "{xml}");

    // Export of an unknown source is a 404 (JSON error).
    assert_eq!(get(f.addr, "/v1/dats/no-intro/nope/export").0, 404);

    // Diff: only one revision materialized → 400 with the helpful text.
    assert_eq!(get(f.addr, "/v1/dats/no-intro/gba/diff").0, 400);
    // Unknown source → 404.
    assert_eq!(get(f.addr, "/v1/dats/no-intro/nope/diff").0, 404);

    // Clonelist: link a minimal retool clonelist (one non-regex term).
    let cl = br#"{"variants":[{"group":"Alpha","titles":[{"searchTerm":"Alpha (USA)"}]}]}"#;
    let (status, v) = post_bytes(f.addr, "/v1/dats/no-intro/gba/clonelist", cl);
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["terms"], 1, "{v}");
    assert_eq!(v["hash"].as_str().expect("hash").len(), 64, "{v}");
}
