//! The M5 v1 read-model API (D30/D68 auth; scope ruling 2026-07-11):
//! systems/entries audit rollups, view metadata, storage stats — JSON
//! renders of the same queries the CLI's `audit`/`status` commands run.
//! Mutating pipeline actions (ingest, eviction, scrub, view eval) stay
//! CLI-only in M5; the UI deep-links CLI instructions instead.
//!
//! Everything here is owner-only except the view surface, which is the
//! friend surface (D68 grants). Owner-only misses answer 403; view-
//! scoped misses answer 404 exactly like nonexistent views so probing
//! leaks nothing (the auth.rs convention).

// Same rationale as http.rs: fallible steps short-circuit with the
// error Response itself.
#![allow(clippy::result_large_err)]

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use axum::Extension;
use axum::extract::{Path as UrlPath, RawQuery, State};
use axum::http::StatusCode;
use axum::response::Response;
use datboi_catalog::ViewDef;
use datboi_core::hash::Blake3;
use datboi_index::{Db, Residency, VerifyState};
use rusqlite::OptionalExtension as _;
use serde_json::{Value, json};

use crate::App;
use crate::auth::{self, Caller};
use crate::http::{enc_seg, json_response, run_blocking};
use crate::vfs;

/// Uniform API error shape: `{"error": "<message>"}`.
pub(crate) fn err(status: StatusCode, msg: &str) -> Response {
    json_response(status, &json!({"error": msg}))
}

fn internal(e: impl std::fmt::Display) -> Response {
    err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string())
}

/// Owner check (D68): loopback and owner-role callers pass; friends
/// get an explicit 403 — these resources aren't view-scoped, so there
/// is nothing to hide by answering 404.
pub(crate) fn require_owner(caller: &Caller) -> Result<(), Response> {
    if caller.is_owner() {
        Ok(())
    } else {
        Err(err(StatusCode::FORBIDDEN, "owner only"))
    }
}

fn lock_db(app: &App) -> std::sync::MutexGuard<'_, Db> {
    app.db
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

// ---- the 4-state entry vocabulary (web spec §7) ----

/// The UI's per-entry state, derived from the D39 rollup: `verified` =
/// every required claim grounded-verified; `claimed` = the rest covered
/// by verified-grade claims; `missing` = anything short of that
/// (probable/peer fold into missing for UI purposes — they are not
/// holdings); `nodump` = no satisfiable claims at all (forcenodump
/// semantics: excluded from completeness math client-side).
const STATE_CASE: &str = "CASE \
     WHEN ea.required IS NULL OR ea.required = 0 THEN 3 \
     WHEN ea.have_verified >= ea.required THEN 0 \
     WHEN ea.have_verified + ea.have_claimed >= ea.required THEN 1 \
     ELSE 2 END";

fn state_str(code: i64) -> &'static str {
    match code {
        0 => "verified",
        1 => "claimed",
        2 => "missing",
        _ => "nodump",
    }
}

fn state_code(name: &str) -> Option<i64> {
    match name {
        "verified" => Some(0),
        "claimed" => Some(1),
        "missing" => Some(2),
        "nodump" => Some(3),
        _ => None,
    }
}

// ---- query-string parsing (axum's Query needs serde derive; a
// two-key parser does not justify enabling it) ----

fn pct_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let hi = (bytes[i + 1] as char).to_digit(16);
                let lo = (bytes[i + 2] as char).to_digit(16);
                match (hi, lo) {
                    (Some(hi), Some(lo)) => {
                        out.push((hi * 16 + lo) as u8);
                        i += 3;
                    }
                    _ => {
                        out.push(b'%');
                        i += 1;
                    }
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn parse_query(query: &str) -> Vec<(String, String)> {
    query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            (pct_decode(k), pct_decode(v))
        })
        .collect()
}

/// Parsed + clamped pagination/filter params for the entries listing.
#[derive(Debug, PartialEq, Eq)]
struct Page {
    q: Option<String>,
    state: Option<i64>,
    offset: u64,
    limit: u64,
}

const LIMIT_DEFAULT: u64 = 200;
const LIMIT_MAX: u64 = 1000;

fn parse_page(query: Option<&str>) -> Result<Page, Response> {
    let mut page = Page {
        q: None,
        state: None,
        offset: 0,
        limit: LIMIT_DEFAULT,
    };
    for (key, value) in parse_query(query.unwrap_or("")) {
        match key.as_str() {
            "q" if !value.is_empty() => page.q = Some(value),
            "q" => {}
            "state" => {
                page.state = Some(state_code(&value).ok_or_else(|| {
                    err(
                        StatusCode::BAD_REQUEST,
                        "state must be one of verified|claimed|missing|nodump",
                    )
                })?);
            }
            "offset" => {
                page.offset = value
                    .parse()
                    .map_err(|_| err(StatusCode::BAD_REQUEST, "offset must be an integer"))?;
            }
            "limit" => {
                let limit: u64 = value
                    .parse()
                    .map_err(|_| err(StatusCode::BAD_REQUEST, "limit must be an integer"))?;
                page.limit = limit.clamp(1, LIMIT_MAX);
            }
            _ => {} // unknown params are ignored, not errors
        }
    }
    Ok(page)
}

// ---- GET /v1/systems ----

pub(crate) async fn systems(
    State(app): State<Arc<App>>,
    Extension(caller): Extension<Caller>,
) -> Response {
    run_blocking(move || {
        require_owner(&caller)?;
        Ok(json_response(StatusCode::OK, &systems_body(&app)?))
    })
    .await
}

/// A "system" is a `dat_source` row: `id` = `source_id` (a cache.db
/// surrogate — stable while the cache lives, re-minted by `datboi
/// recover`; clients treat it as a handle, not an identity — the
/// durable identity is provider/system). Counts derive from the same
/// entry_audit rollup `datboi audit` reads.
fn systems_body(app: &App) -> Result<Value, Response> {
    let db = lock_db(app);
    // Which stored ViewDefs reference each source.
    let mut views_by_source: HashMap<(String, String), Vec<String>> = HashMap::new();
    for name in datboi_catalog::list_views(&db).map_err(internal)? {
        if let Some(def) = datboi_catalog::get_view(&db, &name).map_err(internal)? {
            views_by_source
                .entry((def.provider, def.system))
                .or_default()
                .push(name);
        }
    }
    let conn = db.cache();
    let mut sources = conn
        .prepare(
            "SELECT s.source_id, s.provider, s.system,
                    r.revision_id, r.version, r.dat_date, r.imported_at
             FROM dat_source s
             LEFT JOIN dat_revision r ON r.revision_id = s.current_revision_id
             ORDER BY s.provider, s.system",
        )
        .map_err(internal)?;
    let mut counts = conn
        .prepare(&format!(
            "SELECT COALESCE(SUM(state = 0), 0), COALESCE(SUM(state = 1), 0),
                    COALESCE(SUM(state = 2), 0), COALESCE(SUM(state = 3), 0), COUNT(*)
             FROM (SELECT {STATE_CASE} AS state
                   FROM entry e
                   LEFT JOIN entry_audit ea ON ea.entry_id = e.entry_id
                   WHERE e.revision_id = ?1)"
        ))
        .map_err(internal)?;
    let rows = sources
        .query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<i64>>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, Option<i64>>(6)?,
            ))
        })
        .map_err(internal)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(internal)?;
    let mut out = Vec::new();
    for (source_id, provider, system, revision_id, version, dat_date, imported_at) in rows {
        let (verified, claimed, missing, nodump, total) = match revision_id {
            Some(rev) => counts
                .query_row([rev], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, i64>(4)?,
                    ))
                })
                .map_err(internal)?,
            None => (0, 0, 0, 0, 0),
        };
        let views = views_by_source
            .get(&(provider.clone(), system.clone()))
            .cloned()
            .unwrap_or_default();
        out.push(json!({
            "id": source_id,
            "provider": provider,
            "system": system,
            "source": format!("{provider}/{system}"),
            "revision": revision_id.map(|id| json!({
                "id": id,
                "version": version,
                "date": dat_date,
                "imported_at": imported_at,
            })),
            "counts": {
                "verified": verified,
                "claimed": claimed,
                "missing": missing,
                "nodump": nodump,
            },
            "total": total,
            "views": views,
        }));
    }
    Ok(json!({"systems": out}))
}

/// A source's current revision id; `Ok(None)` = source exists but has
/// no revision, `Err(404)` = no such source.
fn resolve_revision(conn: &rusqlite::Connection, source_id: i64) -> Result<Option<i64>, Response> {
    conn.query_row(
        "SELECT current_revision_id FROM dat_source WHERE source_id = ?1",
        [source_id],
        |row| row.get::<_, Option<i64>>(0),
    )
    .optional()
    .map_err(internal)?
    .ok_or_else(|| err(StatusCode::NOT_FOUND, "no such system"))
}

fn parse_system_id(id: &str) -> Result<i64, Response> {
    id.parse()
        .map_err(|_| err(StatusCode::NOT_FOUND, "no such system"))
}

// ---- GET /v1/systems/{id}/entries ----

/// Scalar subquery: an entry's total required size — NULL unless every
/// required claim declares one (missing/nodump rows show no size).
const SIZE_EXPR: &str = "(SELECT CASE WHEN COUNT(*) = COUNT(rc.size) THEN SUM(rc.size) END
       FROM rom_claim rc
       WHERE rc.entry_id = e.entry_id AND rc.status != 2 AND NOT rc.optional)";

/// Scalar subquery: `algo:hex` of the single required claim's best
/// hash — NULL when the entry wants zero or several ROMs (one hash for
/// a multi-rom set would be a lie; the detail endpoint has them all).
const WANTED_EXPR: &str = "(SELECT CASE WHEN COUNT(*) = 1 THEN MAX(CASE
         WHEN rc.sha256 IS NOT NULL THEN 'sha256:' || lower(hex(rc.sha256))
         WHEN rc.sha1 IS NOT NULL THEN 'sha1:' || lower(hex(rc.sha1))
         WHEN rc.md5 IS NOT NULL THEN 'md5:' || lower(hex(rc.md5))
         WHEN rc.crc32 IS NOT NULL THEN 'crc32:' || lower(hex(rc.crc32))
       END) END
       FROM rom_claim rc
       WHERE rc.entry_id = e.entry_id AND rc.status != 2 AND NOT rc.optional)";

pub(crate) async fn system_entries(
    State(app): State<Arc<App>>,
    Extension(caller): Extension<Caller>,
    UrlPath(id): UrlPath<String>,
    RawQuery(query): RawQuery,
) -> Response {
    run_blocking(move || {
        require_owner(&caller)?;
        let source_id = parse_system_id(&id)?;
        let page = parse_page(query.as_deref())?;
        Ok(json_response(
            StatusCode::OK,
            &entries_body(&app, source_id, &page)?,
        ))
    })
    .await
}

fn entries_body(app: &App, source_id: i64, page: &Page) -> Result<Value, Response> {
    let db = lock_db(app);
    let conn = db.cache();
    let Some(revision_id) = resolve_revision(conn, source_id)? else {
        // Source exists but nothing imported yet: an empty audit, not a
        // miss.
        return Ok(json!({
            "entries": [], "total": 0,
            "offset": page.offset, "limit": page.limit,
        }));
    };
    let filter = format!(
        "e.revision_id = ?1
           AND (?2 IS NULL OR instr(lower(e.name), lower(?2)) > 0)
           AND (?3 IS NULL OR ({STATE_CASE}) = ?3)"
    );
    let total: i64 = conn
        .query_row(
            &format!(
                "SELECT COUNT(*) FROM entry e
                 LEFT JOIN entry_audit ea ON ea.entry_id = e.entry_id
                 WHERE {filter}"
            ),
            rusqlite::params![revision_id, page.q, page.state],
            |row| row.get(0),
        )
        .map_err(internal)?;
    let mut stmt = conn
        .prepare(&format!(
            "SELECT e.name, {STATE_CASE}, {SIZE_EXPR}, {WANTED_EXPR}
             FROM entry e
             LEFT JOIN entry_audit ea ON ea.entry_id = e.entry_id
             WHERE {filter}
             ORDER BY e.name
             LIMIT ?4 OFFSET ?5"
        ))
        .map_err(internal)?;
    let entries = stmt
        .query_map(
            rusqlite::params![revision_id, page.q, page.state, page.limit, page.offset],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, Option<i64>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                ))
            },
        )
        .map_err(internal)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(internal)?
        .into_iter()
        .map(|(name, state, size, wanted)| {
            let (algo, hash) = split_wanted(wanted.as_deref());
            json!({
                "name": name,
                "state": state_str(state),
                "size": size,
                "wanted_hash": hash,
                "wanted_hash_algo": algo,
            })
        })
        .collect::<Vec<_>>();
    Ok(json!({
        "entries": entries, "total": total,
        "offset": page.offset, "limit": page.limit,
    }))
}

fn split_wanted(wanted: Option<&str>) -> (Option<String>, Option<String>) {
    match wanted.and_then(|w| w.split_once(':')) {
        Some((algo, hash)) => (Some(algo.to_owned()), Some(hash.to_owned())),
        None => (None, None),
    }
}

// ---- GET /v1/systems/{id}/entries/{name} ----

pub(crate) async fn system_entry(
    State(app): State<Arc<App>>,
    Extension(caller): Extension<Caller>,
    UrlPath((id, name)): UrlPath<(String, String)>,
) -> Response {
    run_blocking(move || {
        require_owner(&caller)?;
        let source_id = parse_system_id(&id)?;
        Ok(json_response(
            StatusCode::OK,
            &entry_body(&app, source_id, &name)?,
        ))
    })
    .await
}

/// Entry detail is keyed by NAME: `entry` carries
/// `UNIQUE(revision_id, name)`, so within a system's current revision
/// the name is a real key (claim names are not — MAME sets repeat them).
fn entry_body(app: &App, source_id: i64, name: &str) -> Result<Value, Response> {
    // Blob hashes whose pin lists we compute after dropping the lock
    // (vfs::view_tags takes it internally).
    let mut pin_targets: Vec<(usize, Blake3)> = Vec::new();
    let mut roms: Vec<Value> = Vec::new();
    let entry_json = {
        let db = lock_db(app);
        let conn = db.cache();
        let Some(revision_id) = resolve_revision(conn, source_id)? else {
            return Err(err(StatusCode::NOT_FOUND, "no such entry"));
        };
        let Some((entry_id, state, size, wanted)) = conn
            .query_row(
                &format!(
                    "SELECT e.entry_id, {STATE_CASE}, {SIZE_EXPR}, {WANTED_EXPR}
                     FROM entry e
                     LEFT JOIN entry_audit ea ON ea.entry_id = e.entry_id
                     WHERE e.revision_id = ?1 AND e.name = ?2"
                ),
                rusqlite::params![revision_id, name],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, Option<i64>>(2)?,
                        row.get::<_, Option<String>>(3)?,
                    ))
                },
            )
            .optional()
            .map_err(internal)?
        else {
            return Err(err(StatusCode::NOT_FOUND, "no such entry"));
        };
        let (rev_version, rev_date, rev_imported_at) = conn
            .query_row(
                "SELECT version, dat_date, imported_at FROM dat_revision
                 WHERE revision_id = ?1",
                [revision_id],
                |row| {
                    Ok((
                        row.get::<_, Option<String>>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, i64>(2)?,
                    ))
                },
            )
            .map_err(internal)?;

        // Per-claim detail: hashes, rollup state, resolved blob, routes.
        let mut claims = conn
            .prepare(
                "SELECT rc.name, rc.size, rc.status, rc.optional,
                        rc.crc32, rc.md5, rc.sha1, rc.sha256,
                        rc.identity_id, s.state
                 FROM rom_claim rc
                 LEFT JOIN identity_status s ON s.identity_id = rc.identity_id
                 WHERE rc.entry_id = ?1
                 ORDER BY rc.name, rc.claim_id",
            )
            .map_err(internal)?;
        #[allow(clippy::type_complexity)] // one projection row, named once
        let rows: Vec<(
            String,
            Option<i64>,
            i64,
            bool,
            Option<Vec<u8>>,
            Option<Vec<u8>>,
            Option<Vec<u8>>,
            Option<Vec<u8>>,
            Option<i64>,
            Option<i64>,
        )> = claims
            .query_map([entry_id], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                    row.get(7)?,
                    row.get(8)?,
                    row.get(9)?,
                ))
            })
            .map_err(internal)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(internal)?;
        for (
            claim_name,
            claim_size,
            status,
            optional,
            crc32,
            md5,
            sha1,
            sha256,
            identity_id,
            id_state,
        ) in rows
        {
            // identity_status codes (catalog rollup): 4 verified /
            // 3 claimed / 2 peer / 1 probable / 0 missing.
            let claim_state = if status == 2 {
                "nodump"
            } else {
                match id_state {
                    Some(4) => "verified",
                    Some(3) => "claimed",
                    Some(2) => "peer",
                    Some(1) => "probable",
                    _ => "missing",
                }
            };
            let mut hashes = serde_json::Map::new();
            for (algo, digest) in [
                ("crc32", &crc32),
                ("md5", &md5),
                ("sha1", &sha1),
                ("sha256", &sha256),
            ] {
                if let Some(digest) = digest {
                    hashes.insert(algo.to_owned(), Value::String(hex(digest)));
                }
            }
            let mut rom = json!({
                "name": claim_name,
                "size": claim_size,
                "state": claim_state,
                "optional": optional,
                "hashes": hashes,
            });
            // Resolve the identity to a local blob the way view eval
            // does: strong-basis links only, smallest hash wins.
            let blob = identity_id
                .map(|identity_id| {
                    conn.query_row(
                        "SELECT b.blob_id, b.hash, b.residency, b.verified_at
                         FROM identity_blob ib JOIN blob b ON b.blob_id = ib.blob_id
                         WHERE ib.identity_id = ?1 AND ib.basis >= 1
                         ORDER BY b.hash LIMIT 1",
                        [identity_id],
                        |row| {
                            Ok((
                                row.get::<_, i64>(0)?,
                                row.get::<_, [u8; 32]>(1)?,
                                row.get::<_, i64>(2)?,
                                row.get::<_, Option<i64>>(3)?,
                            ))
                        },
                    )
                    .optional()
                })
                .transpose()
                .map_err(internal)?
                .flatten();
            if let Some((blob_id, hash, residency, verified_at)) = blob {
                let hash = Blake3(hash);
                let residency = Residency::from_code(residency).map_err(internal)?;
                // verified_at is the last full-hash store verification
                // (ingest or scrub); the index records no method, so
                // none is claimed here.
                rom["blob"] = json!({
                    "hash": hash.to_hex(),
                    "residency": match residency {
                        Residency::Resident => "resident",
                        Residency::EvictedCovered => "evicted_covered",
                        Residency::Absent => "absent",
                    },
                    "verified_at": verified_at,
                });
                rom["routes"] = Value::Array(routes_json(&db, blob_id).map_err(internal)?);
                pin_targets.push((roms.len(), hash));
            }
            roms.push(rom);
        }
        let (algo, hash) = split_wanted(wanted.as_deref());
        json!({
            "name": name,
            "state": state_str(state),
            "size": size,
            "wanted_hash": hash,
            "wanted_hash_algo": algo,
            "revision": {
                "id": revision_id,
                "version": rev_version,
                "date": rev_date,
                "imported_at": rev_imported_at,
            },
        })
    };

    // Pins: which views' CURRENT snapshots reference each blob (D33 —
    // the tag is what pins; manifests come from the decode cache).
    let tags = vfs::view_tags(app).map_err(internal)?;
    for (rom_index, hash) in pin_targets {
        let pins: Vec<&str> = tags
            .iter()
            .filter(|(_, snap)| {
                vfs::snapshot_index(app, *snap).is_ok_and(|idx| idx.contains_hash(&hash))
            })
            .map(|(name, _)| name.as_str())
            .collect();
        roms[rom_index]["pins"] = json!(pins);
    }

    let mut body = entry_json;
    body["roms"] = Value::Array(roms);
    Ok(body)
}

/// Human-readable rebuild routes for one blob: non-poisoned recipes
/// rendered as `verb ← sources` with a sources-resident flag (the
/// design's source-availability dot). Source labels come from the
/// rescan cache when a path is known, else the input's short hash.
fn routes_json(db: &Db, blob_id: i64) -> Result<Vec<Value>, datboi_index::IndexError> {
    let mut routes = Vec::new();
    for recipe in db.recipes_for_output(blob_id)? {
        if recipe.verify == VerifyState::Failed {
            continue; // poisoned (D25 terminal): not a route
        }
        let inputs = db.recipe_inputs(recipe.recipe_id)?;
        let mut labels = Vec::new();
        let mut sources_resident = true;
        for input in &inputs {
            if input.residency != Residency::Resident {
                sources_resident = false;
            }
            let path: Option<String> = db
                .cache()
                .query_row(
                    "SELECT path FROM source_file WHERE blob_id = ?1 LIMIT 1",
                    [input.blob_id],
                    |row| row.get(0),
                )
                .optional()?;
            labels.push(match path {
                Some(path) => path.rsplit('/').next().unwrap_or(&path).to_owned(),
                None => format!("{}…", &input.hash.to_hex()[..8]),
            });
        }
        let verb = route_verb(&recipe.op_name);
        let route = if labels.is_empty() {
            verb.to_owned()
        } else {
            format!("{verb} ← {}", labels.join(" + "))
        };
        routes.push(json!({
            "route": route,
            "source_present": sources_resident,
            "verify": match recipe.verify {
                VerifyState::Pending => "pending",
                VerifyState::Verified => "verified",
                VerifyState::ReplayedLocal => "replayed_local",
                VerifyState::Failed => unreachable!("filtered above"),
            },
        }));
    }
    Ok(routes)
}

/// `deflate@1` → `deflate`; `<component-hex>#classify` → `classify`.
fn route_verb(op_name: &str) -> &str {
    op_name
        .split_once('#')
        .map(|(_, export)| export)
        .or_else(|| op_name.split_once('@').map(|(name, _)| name))
        .unwrap_or(op_name)
}

// ---- GET /v1/views ----

pub(crate) async fn views(
    State(app): State<Arc<App>>,
    Extension(caller): Extension<Caller>,
) -> Response {
    run_blocking(move || Ok(json_response(StatusCode::OK, &views_body(&app, &caller)?))).await
}

/// The union of tagged views (things being served) and stored ViewDefs
/// (things defined but maybe never evaluated), ACL-filtered: friends
/// see exactly their grants (D68).
fn views_body(app: &App, caller: &Caller) -> Result<Value, Response> {
    let mut items: BTreeMap<String, (Option<Blake3>, Option<ViewDef>)> = BTreeMap::new();
    {
        let db = lock_db(app);
        for (tag, hash) in db.list_tags().map_err(internal)? {
            if let Some(view) = tag.strip_prefix("view/") {
                items.entry(view.to_owned()).or_default().0 = Some(hash);
            }
        }
        for name in datboi_catalog::list_views(&db).map_err(internal)? {
            let def = datboi_catalog::get_view(&db, &name).map_err(internal)?;
            items.entry(name).or_default().1 = def;
        }
        items.retain(|name, _| auth::view_allowed(&db, caller, name));
    }
    let views = items
        .iter()
        .map(|(name, (snapshot, def))| view_json(app, name, *snapshot, def.as_ref()))
        .collect::<Vec<_>>();
    Ok(json!({"views": views}))
}

/// One view's listing entry. Row count / bytes / created_at come from
/// the decoded snapshot manifest (the immutable-object cache makes
/// this a hashmap hit after the first request); a missing or
/// undecodable snapshot just omits them — the listing must not die of
/// one damaged view.
fn view_json(app: &App, name: &str, snapshot: Option<Blake3>, def: Option<&ViewDef>) -> Value {
    let mut v = json!({
        "name": name,
        "snapshot": snapshot.map(|h| h.to_hex()),
        "definition": def.map(def_json),
    });
    if let Some(hash) = snapshot
        && let Ok(idx) = vfs::snapshot_index(app, hash)
    {
        let (rows, bytes) = idx.stats();
        v["rows"] = json!(rows);
        v["bytes"] = json!(bytes);
        v["created_at"] = json!(idx.created_at);
    }
    v
}

/// ViewDef summary: dat ref, 1G1R mode, profile, image params (D62).
fn def_json(def: &ViewDef) -> Value {
    json!({
        "provider": def.provider,
        "system": def.system,
        "template": def.template,
        "one_g_one_r": def.selection.as_ref().map(|policy| json!({
            "mode": if policy.strict { "strict" } else { "held_first" },
            "regions": policy.regions,
            "langs": policy.langs,
        })),
        "profile": def.profile,
        "image": def.image.as_ref().map(|image| json!({
            "cluster_size": image.cluster_size,
            "partition": image.partition,
            "label": image.label,
        })),
        "mame_mode": def.mame.map(datboi_catalog::MameMode::as_str),
    })
}

// ---- GET /v1/views/{name} ----

pub(crate) async fn view_detail(
    State(app): State<Arc<App>>,
    Extension(caller): Extension<Caller>,
    UrlPath(name): UrlPath<String>,
) -> Response {
    run_blocking(move || {
        Ok(json_response(
            StatusCode::OK,
            &view_detail_body(&app, &caller, &name)?,
        ))
    })
    .await
}

fn view_detail_body(app: &App, caller: &Caller, name: &str) -> Result<Value, Response> {
    let (snapshot, def, image_minted) = {
        let db = lock_db(app);
        // View-scoped resource: denial answers exactly like a miss
        // (auth.rs convention) so probing learns nothing.
        if !auth::view_allowed(&db, caller, name) {
            return Err(err(StatusCode::NOT_FOUND, "no such view"));
        }
        let snapshot = db.get_tag(&format!("view/{name}")).map_err(internal)?;
        let def = datboi_catalog::get_view(&db, name).map_err(internal)?;
        if snapshot.is_none() && def.is_none() {
            return Err(err(StatusCode::NOT_FOUND, "no such view"));
        }
        // D62: `image/<name>` tag = a minted FAT32 image for this view.
        let image_minted = match def.as_ref().and_then(|d| d.image.as_ref()) {
            Some(_) => {
                let tag = db.get_tag(&format!("image/{name}")).map_err(internal)?;
                match tag {
                    Some(hash) => {
                        let size = db
                            .blob_by_hash(&hash)
                            .map_err(internal)?
                            .and_then(|row| row.size);
                        Some(json!({"minted": true, "hash": hash.to_hex(), "bytes": size}))
                    }
                    None => Some(json!({"minted": false})),
                }
            }
            None => None,
        };
        (snapshot, def, image_minted)
    };
    let mut body = view_json(app, name, snapshot, def.as_ref());
    // Relative serve endpoints; DAV is loopback-only in M5 (D68 —
    // authenticated DAV is a recorded open question).
    body["endpoints"] = json!({
        "http": format!("/view/{}/", enc_seg(name)),
        "dav": format!("/dav/{}/", enc_seg(name)),
    });
    body["image"] = image_minted.unwrap_or(Value::Null);
    Ok(body)
}

// ---- GET /v1/views/{name}/files ----

pub(crate) async fn view_files(
    State(app): State<Arc<App>>,
    Extension(caller): Extension<Caller>,
    UrlPath(name): UrlPath<String>,
    RawQuery(query): RawQuery,
) -> Response {
    run_blocking(move || {
        // parse_page for the shared q/offset/limit vocabulary and
        // clamps; snapshot rows have no state (a snapshot only serves
        // verified content), so a `state` param is ignored here.
        let page = parse_page(query.as_deref())?;
        Ok(json_response(
            StatusCode::OK,
            &view_files_body(&app, &caller, &name, &page)?,
        ))
    })
    .await
}

/// Flat listing of a view's CURRENT snapshot manifest — the friend
/// browse surface (spec §4.3). Same decoded-manifest cache the byte
/// tree serves from, so a page here is a string scan, not a decode.
fn view_files_body(app: &App, caller: &Caller, name: &str, page: &Page) -> Result<Value, Response> {
    {
        // View-scoped resource: denial answers exactly like a miss
        // (auth.rs convention) so probing learns nothing.
        let db = lock_db(app);
        if !auth::view_allowed(&db, caller, name) {
            return Err(err(StatusCode::NOT_FOUND, "no such view"));
        }
    }
    let idx = vfs::view_index(app, name).map_err(|e| match e {
        vfs::LookupError::NoSuchView => err(StatusCode::NOT_FOUND, "no such view"),
        // A tagged snapshot that won't resolve is server-side damage.
        other => internal(other),
    })?;
    let needle = page.q.as_deref().map(str::to_lowercase);
    let mut files = Vec::new();
    let mut total: u64 = 0;
    for (path, meta) in idx.rows() {
        if let Some(needle) = &needle
            && !path.to_lowercase().contains(needle.as_str())
        {
            continue;
        }
        if total >= page.offset && (files.len() as u64) < page.limit {
            files.push(json!({
                "path": path,
                "size": meta.size,
                "hash": meta.hash.to_hex(),
            }));
        }
        total += 1;
    }
    Ok(json!({
        "files": files, "total": total,
        "offset": page.offset, "limit": page.limit,
        "snapshot": idx.snapshot.to_hex(),
    }))
}

// ---- GET /v1/storage ----

pub(crate) async fn storage(
    State(app): State<Arc<App>>,
    Extension(caller): Extension<Caller>,
) -> Response {
    run_blocking(move || {
        require_owner(&caller)?;
        Ok(json_response(StatusCode::OK, &storage_body(&app)?))
    })
    .await
}

/// Storage stats from the blob index (`datboi status` walks the store
/// directories for its numbers; per-request the index projection is
/// the same truth without an NFS metadata walk). No last-scrub field:
/// the index records per-blob `verified_at`, never a scrub run — a
/// run ledger is jobs-registry work (docs/open-questions.md, raised
/// 2026-07-11).
fn storage_body(app: &App) -> Result<Value, Response> {
    let db = lock_db(app);
    let conn = db.cache();
    // residency codes: 0 resident, 1 evicted-covered, 2 absent.
    let (blob_count, on_disk, represented) = conn
        .query_row(
            "SELECT COUNT(*),
                    COALESCE(SUM(CASE WHEN residency = 0 THEN size END), 0),
                    COALESCE(SUM(CASE WHEN residency IN (0, 1) THEN size END), 0)
             FROM blob WHERE namespace = 0",
            [],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            },
        )
        .map_err(internal)?;
    // Literal-only bytes: the exact `datboi status` query (resident
    // data with no non-failed rebuild route — the "can't shrink yet"
    // tax).
    let literal_only: i64 = conn
        .query_row(
            "SELECT COALESCE(SUM(b.size), 0) FROM blob b
             WHERE b.namespace = 0 AND b.residency = 0
               AND NOT EXISTS (
                 SELECT 1 FROM recipe_output ro
                 JOIN recipe r ON r.recipe_id = ro.recipe_id
                 WHERE ro.blob_id = b.blob_id AND r.verify != 2)",
            [],
            |row| row.get(0),
        )
        .map_err(internal)?;
    // D49 rule 3: components whose seek path produced bad bytes.
    let quarantined = db.list_seek_quarantined().map_err(internal)?;
    Ok(json!({
        "blob_count": blob_count,
        "on_disk_bytes": on_disk,
        "represented_bytes": represented,
        "literal_only_bytes": literal_only,
        "quarantine": {
            "count": quarantined.len(),
            "items": quarantined.iter().map(|(component, at, reason)| json!({
                "component": component.to_hex(),
                "quarantined_at": at,
                "reason": reason,
            })).collect::<Vec<_>>(),
        },
    }))
}

// ---- GET /v1/jobs ----

/// The daemon has no job registry: ingest/scrub/eviction run as CLI
/// processes today (docs/open-questions.md § raised 2026-07-11, "Jobs
/// tray backend"). An empty list is the truthful render until a real
/// one exists — nothing here fakes progress.
pub(crate) async fn jobs(Extension(caller): Extension<Caller>) -> Response {
    match require_owner(&caller) {
        Ok(()) => json_response(StatusCode::OK, &json!({"jobs": []})),
        Err(resp) => resp,
    }
}

// ---- helpers ----

pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::Via;
    use datboi_index::Role;

    #[test]
    fn query_parsing_decodes_and_splits() {
        assert_eq!(
            parse_query("q=Alpha%20%28USA%29&state=missing&x"),
            vec![
                ("q".into(), "Alpha (USA)".into()),
                ("state".into(), "missing".into()),
                ("x".into(), String::new()),
            ]
        );
        assert_eq!(pct_decode("a+b%2Fc"), "a b/c");
        assert_eq!(pct_decode("%"), "%", "truncated escape is literal");
        assert_eq!(pct_decode("%zz"), "%zz", "bad hex is literal");
    }

    #[test]
    fn page_params_clamp_and_reject() {
        let page = parse_page(None).expect("defaults");
        assert_eq!(
            page,
            Page {
                q: None,
                state: None,
                offset: 0,
                limit: LIMIT_DEFAULT
            }
        );
        let page = parse_page(Some("limit=999999&offset=3&q=zelda&state=nodump")).expect("parse");
        assert_eq!(page.limit, LIMIT_MAX, "limit clamps to the max");
        assert_eq!((page.offset, page.state), (3, Some(3)));
        assert_eq!(page.q.as_deref(), Some("zelda"));
        assert_eq!(parse_page(Some("limit=0")).expect("parse").limit, 1);
        assert!(parse_page(Some("limit=abc")).is_err());
        assert!(parse_page(Some("offset=-1")).is_err());
        assert!(parse_page(Some("state=bogus")).is_err());
        assert_eq!(
            parse_page(Some("q=")).expect("parse").q,
            None,
            "empty q is no filter"
        );
    }

    #[test]
    fn state_vocabulary_round_trips() {
        for (code, name) in [
            (0, "verified"),
            (1, "claimed"),
            (2, "missing"),
            (3, "nodump"),
        ] {
            assert_eq!(state_str(code), name);
            assert_eq!(state_code(name), Some(code));
        }
        assert_eq!(state_code("peer"), None, "peer is not a filterable state");
    }

    #[test]
    fn route_verbs_read_like_the_design() {
        assert_eq!(route_verb("deflate@1"), "deflate");
        assert_eq!(route_verb("assemble@1"), "assemble");
        assert_eq!(route_verb("abc123def#extract"), "extract");
        assert_eq!(route_verb("weird"), "weird");
    }

    fn test_app() -> (tempfile::TempDir, Arc<App>) {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_dir = dir.path().join("db");
        std::fs::create_dir_all(&db_dir).expect("db dir");
        let app = App::open(&dir.path().join("store"), &db_dir).expect("app");
        (dir, app)
    }

    fn friend(db: &Db, name: &str, grant: &[&str]) -> Caller {
        let user_id = db.create_user(name, "$x$", Role::Friend, 1).expect("user");
        for view in grant {
            db.grant_view(user_id, view).expect("grant");
        }
        Caller::User {
            user_id,
            username: name.to_owned(),
            role: Role::Friend,
            via: Via::Session,
        }
    }

    /// The friend/owner visibility matrix runs against the handler
    /// bodies directly: the HTTP gate makes every loopback peer the
    /// owner (D68), so integration tests physically cannot present a
    /// friend — this is where that path is exercised.
    #[test]
    fn friends_see_grants_owners_see_everything() {
        let (_dir, app) = test_app();
        let pal = {
            let db = lock_db(&app);
            db.set_tag("view/gba", &Blake3::compute(b"gba snap"), 1)
                .expect("tag");
            db.set_tag("view/psx", &Blake3::compute(b"psx snap"), 1)
                .expect("tag");
            friend(&db, "pal", &["gba"])
        };

        let names = |caller: &Caller| -> Vec<String> {
            let body = views_body(&app, caller).expect("views");
            body["views"]
                .as_array()
                .expect("array")
                .iter()
                .map(|v| v["name"].as_str().expect("name").to_owned())
                .collect()
        };
        assert_eq!(names(&Caller::Local), ["gba", "psx"]);
        assert_eq!(names(&pal), ["gba"]);

        // View detail: granted answers (even with the snapshot object
        // absent — stats just drop out); ungranted answers 404, not 403.
        let detail = view_detail_body(&app, &pal, "gba").expect("granted view");
        assert_eq!(detail["name"], "gba");
        assert_eq!(detail["endpoints"]["http"], "/view/gba/");
        assert!(
            detail.get("rows").is_none(),
            "undecodable snapshot: no stats"
        );
        let denied = view_detail_body(&app, &pal, "psx").expect_err("not granted");
        assert_eq!(denied.status(), StatusCode::NOT_FOUND);
        let missing = view_detail_body(&app, &Caller::Local, "nope").expect_err("no such view");
        assert_eq!(missing.status(), StatusCode::NOT_FOUND);
    }

    /// Friend gating + query semantics for the browse listing, against
    /// a REAL encoded snapshot (the decode path is the served path).
    #[test]
    fn view_files_pages_filters_and_gates() {
        use datboi_core::viewsnap::{ViewRow, ViewSnapshot};

        let (_dir, app) = test_app();
        let snap = ViewSnapshot {
            created_at: 7,
            view_name: "gba".into(),
            sources: vec![],
            rows: [
                "Games/Alpha (USA).gba",
                "Games/Beta (Japan).gba",
                "loose.txt",
            ]
            .into_iter()
            .map(|path| ViewRow {
                path: path.into(),
                hash: Blake3::compute(path.as_bytes()),
                size: 9,
                seek: 0,
            })
            .collect(),
        };
        let bytes = snap.encode().expect("encode");
        let (hash, _, _) = app
            .store
            .put_new(datboi_store_fs::Namespace::Meta, bytes.as_slice())
            .expect("put");
        let pal = {
            let db = lock_db(&app);
            db.set_tag("view/gba", &hash, 1).expect("tag");
            friend(&db, "pal", &["gba"])
        };

        let page = |q: Option<&str>, offset, limit| Page {
            q: q.map(str::to_owned),
            state: None,
            offset,
            limit,
        };
        let body = view_files_body(&app, &pal, "gba", &page(None, 0, 200)).expect("granted");
        assert_eq!(body["total"], 3);
        assert_eq!(body["snapshot"], hash.to_hex());
        let files = body["files"].as_array().expect("files");
        assert_eq!(files.len(), 3);
        assert_eq!(files[0]["path"], "Games/Alpha (USA).gba", "path-ordered");
        assert_eq!(files[0]["size"], 9);
        assert_eq!(
            files[0]["hash"],
            Blake3::compute(b"Games/Alpha (USA).gba").to_hex()
        );

        // q is case-insensitive substring on the FULL path; total is
        // the filtered count, the window slides under it.
        let body = view_files_body(&app, &pal, "gba", &page(Some("ALPHA"), 0, 200)).expect("q");
        assert_eq!(body["total"], 1);
        assert_eq!(body["files"][0]["path"], "Games/Alpha (USA).gba");
        let body = view_files_body(&app, &pal, "gba", &page(Some("games/"), 1, 1)).expect("page");
        assert_eq!(body["total"], 2);
        assert_eq!(body["files"].as_array().expect("files").len(), 1);
        assert_eq!(body["files"][0]["path"], "Games/Beta (Japan).gba");

        // Not granted / nonexistent: identical 404s (the auth.rs
        // convention — probing learns nothing).
        let denied = view_files_body(&app, &pal, "psx", &page(None, 0, 200)).expect_err("denied");
        assert_eq!(denied.status(), StatusCode::NOT_FOUND);
        let missing =
            view_files_body(&app, &Caller::Local, "nope", &page(None, 0, 200)).expect_err("miss");
        assert_eq!(missing.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn owner_only_surfaces_answer_403_to_friends() {
        let (_dir, app) = test_app();
        let pal = {
            let db = lock_db(&app);
            friend(&db, "pal", &[])
        };
        assert!(require_owner(&Caller::Local).is_ok());
        let resp = require_owner(&pal).expect_err("friend");
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        assert_eq!(
            require_owner(&Caller::Anonymous)
                .expect_err("anon")
                .status(),
            StatusCode::FORBIDDEN
        );

        // Empty-universe read models still answer sanely for owners.
        let systems = systems_body(&app).expect("systems");
        assert_eq!(systems["systems"].as_array().expect("array").len(), 0);
        let storage = storage_body(&app).expect("storage");
        assert_eq!(storage["blob_count"], 0);
        assert_eq!(storage["quarantine"]["count"], 0);
    }
}
