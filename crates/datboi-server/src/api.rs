//! The v1 API (D30/D68 auth). Under D96 the HTTP+web surface is the
//! COMPLETE one and the CLI is a convenience over the same daemon —
//! both call one shared library function per verb, never two impls.
//! Read models (systems/entries audit rollups, view metadata, storage
//! stats) are JSON renders of the same `datboi-catalog` queries the
//! CLI's `audit`/`status` run. Mutating and expensive verbs live here
//! too — dat import (dats.rs), ingest (ingest.rs, staged uploads + the
//! jobs.rs registry), and the view/maintenance verbs graduating under
//! D96 — each registering long-running work in the jobs.rs ledger.
//! The only permanent CLI-first exceptions (D96) are operator
//! bootstrap: `recover` and initial identity/token minting.
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
use datboi_api::{
    BlobDetail, BlobDigests, BlobInfo, BlobRow, BlobsPage, ClaimRef, ClaimState, ClassBytes,
    Counts, Definition, Endpoints, EntriesPage, EntryDetail, EntryRow, EntryState, ErrorCode,
    EvictBlocked, EvictPlan, EvictRequest,
    FileRow, HashRef, ImageStatus, JobsResponse, Nullable, ProvenanceRow, ProvenanceViaRow,
    Quarantine, QuarantineItem, ResidencyState, Revision, RomClaim, RomHashes, RootRef,
    RootRelation, RouteEdge, RouteInfo, RouteVerify, SourceBytes, StorageBreakdown,
    StorageResponse, System, SystemsResponse, ViewDetail, ViewFilesPage, ViewProfile,
    ViewProfilesResponse, ViewSummary, ViewsResponse,
};
use datboi_catalog::ViewDef;
use datboi_core::hash::Blake3;
use datboi_index::{AliasAlgo, Db, GuardHolder, Namespace, Residency, VerifyState};
use rusqlite::OptionalExtension as _;

use crate::App;
use crate::auth::{self, Caller};
use crate::http::{enc_seg, json_response, run_blocking};
use crate::vfs;

/// Uniform API error shape (datboi-api, D69/D77): the machine-readable
/// code picks the HTTP status — a handler cannot pair them wrong — and
/// `msg` rides along as diagnostic detail for CLI/log consumers.
pub(crate) fn err(code: datboi_api::ErrorCode, msg: &str) -> Response {
    json_response(
        StatusCode::from_u16(code.http_status()).expect("contract statuses are valid"),
        &datboi_api::ApiError {
            error: msg.to_owned(),
            code,
        },
    )
}

pub(crate) fn internal(e: impl std::fmt::Display) -> Response {
    err(datboi_api::ErrorCode::Internal, &e.to_string())
}

/// Owner check (D68): loopback and owner-role callers pass; friends
/// get an explicit 403 — these resources aren't view-scoped, so there
/// is nothing to hide by answering 404.
pub(crate) fn require_owner(caller: &Caller) -> Result<(), Response> {
    if caller.is_owner() {
        Ok(())
    } else {
        Err(err(datboi_api::ErrorCode::OwnerOnly, "owner only"))
    }
}

fn lock_db(app: &App) -> std::sync::MutexGuard<'_, Db> {
    app.db
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// A read-only pool connection (D93): the shape for handlers that
/// never write. The sqlite-level read-only flag is the fence — a
/// write through this guard errors loudly.
fn read_db(app: &App) -> std::sync::MutexGuard<'_, Db> {
    app.readers.get()
}

// ---- the 4-state entry vocabulary (web spec §7) ----
//
// The threshold rule, its SQL projection, and the wire codes all live in
// ONE place — `datboi_catalog::state` (D96: one code path per concept).
// This module only bridges catalog's `RollupState` to the `datboi-api`
// wire enum. `STATE_CASE` is catalog's SQL fragment; the `{STATE_CASE}`
// format sites below splice it into query strings unchanged.

use datboi_catalog::RollupState;

const STATE_CASE: &str = datboi_catalog::STATE_CASE_SQL;

fn entry_state(code: i64) -> EntryState {
    match RollupState::from_code(code) {
        RollupState::Verified => EntryState::Verified,
        RollupState::Claimed => EntryState::Claimed,
        RollupState::Missing => EntryState::Missing,
        RollupState::Nodump => EntryState::Nodump,
    }
}

fn state_code(name: &str) -> Option<i64> {
    RollupState::from_name(name).map(RollupState::code)
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

pub(crate) fn parse_query(query: &str) -> Vec<(String, String)> {
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
                        ErrorCode::BadRequest,
                        "state must be one of verified|claimed|missing|nodump",
                    )
                })?);
            }
            "offset" => {
                page.offset = value
                    .parse()
                    .map_err(|_| err(ErrorCode::BadRequest, "offset must be an integer"))?;
            }
            "limit" => {
                let limit: u64 = value
                    .parse()
                    .map_err(|_| err(ErrorCode::BadRequest, "limit must be an integer"))?;
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
fn systems_body(app: &App) -> Result<SystemsResponse, Response> {
    let db = read_db(app);
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
        out.push(System {
            id: source_id,
            source: format!("{provider}/{system}"),
            provider,
            system,
            revision: revision_id
                .map(|id| Revision {
                    id,
                    version: version.into(),
                    date: dat_date.into(),
                    imported_at: imported_at.into(),
                })
                .into(),
            counts: Counts {
                verified,
                claimed,
                missing,
                nodump,
            },
            total,
            views,
        });
    }
    Ok(SystemsResponse { systems: out })
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
    .ok_or_else(|| err(ErrorCode::NotFound, "no such system"))
}

fn parse_system_id(id: &str) -> Result<i64, Response> {
    id.parse()
        .map_err(|_| err(ErrorCode::NotFound, "no such system"))
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

fn entries_body(app: &App, source_id: i64, page: &Page) -> Result<EntriesPage, Response> {
    let db = read_db(app);
    let conn = db.cache();
    let Some(revision_id) = resolve_revision(conn, source_id)? else {
        // Source exists but nothing imported yet: an empty audit, not a
        // miss.
        return Ok(EntriesPage {
            entries: Vec::new(),
            total: 0,
            offset: page.offset,
            limit: page.limit,
        });
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
            EntryRow {
                name,
                state: entry_state(state),
                size: size.into(),
                wanted_hash: hash.into(),
                wanted_hash_algo: algo.into(),
            }
        })
        .collect::<Vec<_>>();
    Ok(EntriesPage {
        entries,
        total,
        offset: page.offset,
        limit: page.limit,
    })
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
fn entry_body(app: &App, source_id: i64, name: &str) -> Result<EntryDetail, Response> {
    // Blob hashes whose pin lists we compute after dropping the lock
    // (vfs::view_tags takes it internally).
    let mut pin_targets: Vec<(usize, Blake3)> = Vec::new();
    let mut roms: Vec<RomClaim> = Vec::new();
    let mut detail = {
        let db = read_db(app);
        let conn = db.cache();
        let Some(revision_id) = resolve_revision(conn, source_id)? else {
            return Err(err(ErrorCode::NotFound, "no such entry"));
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
            return Err(err(ErrorCode::NotFound, "no such entry"));
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
                ClaimState::Nodump
            } else {
                match id_state {
                    Some(4) => ClaimState::Verified,
                    Some(3) => ClaimState::Claimed,
                    Some(2) => ClaimState::Peer,
                    Some(1) => ClaimState::Probable,
                    _ => ClaimState::Missing,
                }
            };
            let mut rom = RomClaim {
                name: claim_name,
                size: claim_size.into(),
                state: claim_state,
                optional,
                hashes: RomHashes {
                    crc32: crc32.as_deref().map(hex),
                    md5: md5.as_deref().map(hex),
                    sha1: sha1.as_deref().map(hex),
                    sha256: sha256.as_deref().map(hex),
                },
                blob: None,
                routes: None,
                pins: None,
            };
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
                rom.blob = Some(BlobInfo {
                    hash: hash.to_hex(),
                    residency: match residency {
                        Residency::Resident => ResidencyState::Resident,
                        Residency::EvictedCovered => ResidencyState::EvictedCovered,
                        Residency::Absent => ResidencyState::Absent,
                    },
                    verified_at: verified_at.into(),
                });
                rom.routes = Some(routes_info(&db, blob_id).map_err(internal)?);
                pin_targets.push((roms.len(), hash));
            }
            roms.push(rom);
        }
        let (algo, hash) = split_wanted(wanted.as_deref());
        EntryDetail {
            name: name.to_owned(),
            state: entry_state(state),
            size: size.into(),
            wanted_hash: hash.into(),
            wanted_hash_algo: algo.into(),
            revision: Revision {
                id: revision_id,
                version: rev_version.into(),
                date: rev_date.into(),
                imported_at: rev_imported_at.into(),
            },
            roms: Vec::new(), // filled below, after the pin scan
        }
    };

    // Pins: which views' CURRENT snapshots reference each blob (D33 —
    // the tag is what pins; manifests come from the decode cache).
    let tags = vfs::view_tags(app).map_err(internal)?;
    for (rom_index, hash) in pin_targets {
        let pins: Vec<String> = tags
            .iter()
            .filter(|(_, snap)| {
                vfs::snapshot_index(app, *snap).is_ok_and(|idx| idx.contains_hash(&hash))
            })
            .map(|(name, _)| name.clone())
            .collect();
        roms[rom_index].pins = Some(pins);
    }

    detail.roms = roms;
    Ok(detail)
}

/// Human-readable rebuild routes for one blob: non-poisoned recipes
/// rendered as `verb ← sources` with a sources-resident flag (the
/// design's source-availability dot). Source labels come from the
/// rescan cache when a path is known, else the input's short hash.
fn routes_info(db: &Db, blob_id: i64) -> Result<Vec<RouteInfo>, datboi_index::IndexError> {
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
        routes.push(RouteInfo {
            route,
            source_present: sources_resident,
            verify: match recipe.verify {
                VerifyState::Pending => RouteVerify::Pending,
                VerifyState::Verified => RouteVerify::Verified,
                VerifyState::ReplayedLocal => RouteVerify::ReplayedLocal,
                VerifyState::Failed => unreachable!("filtered above"),
            },
        });
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

// ---- GET /v1/view-profiles ----

/// The built-in constraint profiles, rendered from
/// `datboi_catalog::PROFILES` — the same static data the CLI's `view
/// profiles` prints (D96: one source, two surfaces). Owner-only, since
/// it exists to feed the authoring UI.
pub(crate) async fn view_profiles(Extension(caller): Extension<Caller>) -> Response {
    run_blocking(move || {
        require_owner(&caller)?;
        let profiles = datboi_catalog::PROFILES
            .iter()
            .map(|p| ViewProfile {
                name: p.name.to_owned(),
                max_name_len: p.max_name_len as u64,
                max_file_size: p.max_file_size.into(),
                max_dir_entries: p.max_dir_entries.map(|n| n as u64).into(),
            })
            .collect();
        Ok(json_response(
            StatusCode::OK,
            &ViewProfilesResponse { profiles },
        ))
    })
    .await
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
fn views_body(app: &App, caller: &Caller) -> Result<ViewsResponse, Response> {
    let mut items: BTreeMap<String, (Option<Blake3>, Option<ViewDef>)> = BTreeMap::new();
    {
        let db = read_db(app);
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
        .map(|(name, (snapshot, def))| view_summary(app, name, *snapshot, def.as_ref()))
        .collect::<Vec<_>>();
    Ok(ViewsResponse { views })
}

/// One view's listing entry. Row count / bytes / created_at come from
/// the decoded snapshot manifest (the immutable-object cache makes
/// this a hashmap hit after the first request); a missing or
/// undecodable snapshot just omits them — the listing must not die of
/// one damaged view.
fn view_summary(
    app: &App,
    name: &str,
    snapshot: Option<Blake3>,
    def: Option<&ViewDef>,
) -> ViewSummary {
    let mut v = ViewSummary {
        name: name.to_owned(),
        snapshot: snapshot.map(|h| h.to_hex()).into(),
        definition: def.map(definition).into(),
        rows: None,
        bytes: None,
        created_at: None,
    };
    if let Some(hash) = snapshot
        && let Ok(idx) = vfs::snapshot_index(app, hash)
    {
        let (rows, bytes) = idx.stats();
        v.rows = Some(rows as u64);
        v.bytes = Some(bytes);
        v.created_at = Some(idx.created_at);
    }
    v
}

/// ViewDef summary: dat ref, 1G1R mode, profile, image params (D62).
pub(crate) fn definition(def: &ViewDef) -> Definition {
    Definition {
        provider: def.provider.clone(),
        system: def.system.clone(),
        template: def.template.clone(),
        one_g_one_r: def
            .selection
            .as_ref()
            .map(|policy| datboi_api::OneGOneR {
                mode: if policy.strict {
                    datboi_api::OneGOneRMode::Strict
                } else {
                    datboi_api::OneGOneRMode::HeldFirst
                },
                regions: policy.regions.clone(),
                langs: policy.langs.clone(),
            })
            .into(),
        profile: def.profile.clone().into(),
        image: def
            .image
            .as_ref()
            .map(|image| datboi_api::ImageParams {
                cluster_size: image.cluster_size,
                partition: image.partition,
                label: image.label.clone().into(),
            })
            .into(),
        mame_mode: def
            .mame
            .map(|mode| match mode {
                datboi_catalog::MameMode::NonMerged => datboi_api::MameMode::NonMerged,
                datboi_catalog::MameMode::Split => datboi_api::MameMode::Split,
                datboi_catalog::MameMode::Merged => datboi_api::MameMode::Merged,
            })
            .into(),
    }
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

fn view_detail_body(app: &App, caller: &Caller, name: &str) -> Result<ViewDetail, Response> {
    let (snapshot, def, image_minted) = {
        let db = read_db(app);
        // View-scoped resource: denial answers exactly like a miss
        // (auth.rs convention) so probing learns nothing.
        if !auth::view_allowed(&db, caller, name) {
            return Err(err(ErrorCode::NotFound, "no such view"));
        }
        let snapshot = db.get_tag(&format!("view/{name}")).map_err(internal)?;
        let def = datboi_catalog::get_view(&db, name).map_err(internal)?;
        if snapshot.is_none() && def.is_none() {
            return Err(err(ErrorCode::NotFound, "no such view"));
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
                        Some(ImageStatus {
                            minted: true,
                            hash: Some(hash.to_hex()),
                            // Some(null) = minted but sizeless: the
                            // key renders as null, not absent.
                            bytes: Some(size.into()),
                        })
                    }
                    None => Some(ImageStatus {
                        minted: false,
                        hash: None,
                        bytes: None,
                    }),
                }
            }
            None => None,
        };
        (snapshot, def, image_minted)
    };
    Ok(ViewDetail {
        summary: view_summary(app, name, snapshot, def.as_ref()),
        // Relative serve endpoints; DAV is loopback-only in M5 (D68 —
        // authenticated DAV is a recorded open question).
        endpoints: Endpoints {
            http: format!("/view/{}/", enc_seg(name)),
            dav: format!("/dav/{}/", enc_seg(name)),
        },
        image: image_minted.into(),
    })
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
fn view_files_body(
    app: &App,
    caller: &Caller,
    name: &str,
    page: &Page,
) -> Result<ViewFilesPage, Response> {
    {
        // View-scoped resource: denial answers exactly like a miss
        // (auth.rs convention) so probing learns nothing.
        let db = read_db(app);
        if !auth::view_allowed(&db, caller, name) {
            return Err(err(ErrorCode::NotFound, "no such view"));
        }
    }
    let idx = vfs::view_index(app, name).map_err(|e| match e {
        vfs::LookupError::NoSuchView => err(ErrorCode::NotFound, "no such view"),
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
            files.push(FileRow {
                path: path.to_owned(),
                size: meta.size,
                hash: meta.hash.to_hex(),
            });
        }
        total += 1;
    }
    Ok(ViewFilesPage {
        files,
        total,
        offset: page.offset,
        limit: page.limit,
        snapshot: idx.snapshot.to_hex(),
    })
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
/// the same truth without an NFS metadata walk). The last-scrub readout
/// rides the D74 job ledger, not a per-blob column: `verified_at` still
/// records per-blob freshness, while `last_scrub` names the newest
/// finished Scrub RUN (CLI or the daemon's `POST /v1/scrub`).
fn storage_body(app: &App) -> Result<StorageResponse, Response> {
    let db = read_db(app);
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
    // The scrub-run readout (D74): newest finished scrub row, whichever
    // side ran it — the CLI stamps terminal rows, and the daemon's
    // `POST /v1/scrub` finishes a Scrub job into the same ledger (D96).
    let last_scrub = db
        .latest_finished_job_of_kind(datboi_index::JobKind::Scrub)
        .map_err(internal)?
        .and_then(|row| {
            Some(datboi_api::LastScrub {
                finished_at: row.finished_at?,
                name: row.name,
            })
        });
    Ok(StorageResponse {
        blob_count,
        on_disk_bytes: on_disk,
        represented_bytes: represented,
        literal_only_bytes: literal_only,
        last_scrub: last_scrub.into(),
        quarantine: Quarantine {
            count: quarantined.len() as u64,
            items: quarantined
                .into_iter()
                .map(|(component, quarantined_at, reason)| QuarantineItem {
                    component: component.to_hex(),
                    quarantined_at,
                    reason,
                })
                .collect(),
        },
    })
}

// ---- GET /v1/storage/breakdown ----

pub(crate) async fn storage_breakdown(
    State(app): State<Arc<App>>,
    Extension(caller): Extension<Caller>,
) -> Response {
    run_blocking(move || {
        require_owner(&caller)?;
        Ok(json_response(StatusCode::OK, &breakdown_body(&app)?))
    })
    .await
}

/// `data`/`meta` wire label for a namespace code (D20).
fn ns_label(code: i64) -> Result<&'static str, Response> {
    Ok(match Namespace::from_code(code).map_err(internal)? {
        Namespace::Data => "data",
        Namespace::Meta => "meta",
    })
}

fn residency_state(code: i64) -> Result<ResidencyState, Response> {
    Ok(match Residency::from_code(code).map_err(internal)? {
        Residency::Resident => ResidencyState::Resident,
        Residency::EvictedCovered => ResidencyState::EvictedCovered,
        Residency::Absent => ResidencyState::Absent,
    })
}

/// The recipe DAG as an undirected edge list, poisoned recipes
/// excluded (D25). Shared prefix of every D79 attribution query.
const DAG_EDGES_CTE: &str = "edges(a, b) AS (
       SELECT ri.blob_id, ro.blob_id
       FROM recipe_input ri
       JOIN recipe_output ro ON ro.recipe_id = ri.recipe_id
       JOIN recipe r ON r.recipe_id = ri.recipe_id
       WHERE r.verify <> 2)";

/// Every (claimed root, connected blob) pair: seed each claimed blob
/// as its own root, then flood the recipe DAG UNDIRECTEDLY — a chunk,
/// a container, and a preflate stream all serve the rom they connect
/// to (D79). The UNION dedupes pairs, so the recursion converges.
const REACH_CTE: &str = "reach(root, id) AS (
       SELECT DISTINCT ib.blob_id, ib.blob_id
       FROM identity_blob ib
       JOIN rom_claim rc ON rc.identity_id = ib.identity_id
       UNION
       SELECT rch.root, CASE WHEN e.a = rch.id THEN e.b ELSE e.a END
       FROM reach rch JOIN edges e ON e.a = rch.id OR e.b = rch.id)";

/// The by_source attribution (D79): a blob belongs to every source
/// whose claimed content it is recipe-connected to — the two roms AND
/// their containers, chunks, and streams, not just the claimed blobs
/// themselves. Deduped per (source, blob); a blob serving several
/// sources counts in EACH, so the column does not sum to the store.
/// Links of any evidence grade attribute: this is explanation, not
/// holdings math (the D39 rollup owns that).
fn by_source_sql() -> String {
    format!(
        "WITH RECURSIVE {DAG_EDGES_CTE}, {REACH_CTE}
         SELECT source, COUNT(*), COALESCE(SUM(size), 0)
         FROM (SELECT DISTINCT ds.provider || '/' || ds.system AS source,
                      b.blob_id, b.size
               FROM reach rch
               JOIN blob b ON b.blob_id = rch.id AND b.namespace = 0
               JOIN identity_blob ib ON ib.blob_id = rch.root
               JOIN rom_claim rc ON rc.identity_id = ib.identity_id
               JOIN entry e ON e.entry_id = rc.entry_id
               JOIN dat_revision dr ON dr.revision_id = e.revision_id
               JOIN dat_source ds ON ds.source_id = dr.source_id
                                 AND ds.current_revision_id = dr.revision_id)
         GROUP BY source
         ORDER BY 3 DESC, source"
    )
}

fn breakdown_body(app: &App) -> Result<StorageBreakdown, Response> {
    let db = read_db(app);
    let conn = db.cache();
    // by_class: every (namespace, residency) cell that exists. NULL
    // sizes sum as 0; the sizeless count keeps the 0 honest.
    let mut stmt = conn
        .prepare(
            "SELECT namespace, residency, COUNT(*),
                    COALESCE(SUM(size), 0), COALESCE(SUM(size IS NULL), 0)
             FROM blob
             GROUP BY namespace, residency
             ORDER BY namespace, residency",
        )
        .map_err(internal)?;
    let cells: Vec<(i64, i64, i64, i64, i64)> = stmt
        .query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
            ))
        })
        .map_err(internal)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(internal)?;
    let mut by_class = Vec::new();
    for (ns, residency, blobs, bytes, sizeless) in cells {
        by_class.push(ClassBytes {
            namespace: ns_label(ns)?.to_owned(),
            residency: residency_state(residency)?,
            blobs,
            bytes,
            sizeless,
        });
    }

    let mut stmt = conn.prepare(&by_source_sql()).map_err(internal)?;
    let mut by_source = stmt
        .query_map([], |row| {
            Ok(SourceBytes {
                source: row.get(0)?,
                blobs: row.get(1)?,
                bytes: row.get(2)?,
            })
        })
        .map_err(internal)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(internal)?;
    // Truly UNATTACHED blobs (D79): connected to nothing claimed, not
    // even through the recipe DAG — dat files, never-matched uploads.
    // This bucket is actionable (junk you could delete), where the old
    // no-direct-claim "(unattributed)" swept every chunk and container
    // into an alarming mystery pile.
    let (blobs, bytes): (i64, i64) = conn
        .query_row(
            &format!(
                "WITH RECURSIVE {DAG_EDGES_CTE}, {REACH_CTE}
                 SELECT COUNT(*), COALESCE(SUM(b.size), 0) FROM blob b
                 WHERE b.namespace = 0
                   AND b.blob_id NOT IN (SELECT id FROM reach)"
            ),
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map_err(internal)?;
    if blobs > 0 {
        by_source.push(SourceBytes {
            source: "(unattached)".to_owned(),
            blobs,
            bytes,
        });
    }

    // Sizeless rows sort below every sized one (SQLite: NULL is
    // smallest, so DESC puts them last) — they only pad a short list.
    let largest = blob_rows(
        conn,
        "WHERE b.namespace = 0 ORDER BY b.size DESC, b.hash LIMIT 50",
        [],
    )?;
    Ok(StorageBreakdown {
        by_class,
        by_source,
        largest,
    })
}

// ---- GET /v1/blobs ----

/// The listing projection: one blob row plus its graph degree. Degree
/// counts exclude poisoned recipes (D25), like every route surface.
const BLOB_ROW_SQL: &str = "SELECT b.hash, b.size, b.namespace, b.residency, b.verified_at,
       (SELECT COUNT(*) FROM source_file sf WHERE sf.blob_id = b.blob_id),
       (SELECT COUNT(DISTINCT ro.recipe_id)
          FROM recipe_output ro JOIN recipe r ON r.recipe_id = ro.recipe_id
          WHERE ro.blob_id = b.blob_id AND r.verify != 2),
       (SELECT COUNT(DISTINCT ri.recipe_id)
          FROM recipe_input ri JOIN recipe r ON r.recipe_id = ri.recipe_id
          WHERE ri.blob_id = b.blob_id AND r.verify != 2)
     FROM blob b";

fn blob_rows(
    conn: &rusqlite::Connection,
    suffix: &str,
    params: impl rusqlite::Params,
) -> Result<Vec<BlobRow>, Response> {
    let mut stmt = conn
        .prepare(&format!("{BLOB_ROW_SQL} {suffix}"))
        .map_err(internal)?;
    #[allow(clippy::type_complexity)] // one projection row, named once
    let rows: Vec<([u8; 32], Option<i64>, i64, i64, Option<i64>, i64, i64, i64)> = stmt
        .query_map(params, |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
                row.get(6)?,
                row.get(7)?,
            ))
        })
        .map_err(internal)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(internal)?;
    let count = |n: i64| u64::try_from(n).expect("COUNT(*) is non-negative");
    rows.into_iter()
        .map(
            |(hash, size, ns, residency, verified_at, sources, routes_in, routes_out)| {
                Ok(BlobRow {
                    hash: Blake3(hash).to_hex(),
                    size: size.into(),
                    namespace: ns_label(ns)?.to_owned(),
                    residency: residency_state(residency)?,
                    verified_at: verified_at.into(),
                    sources: count(sources),
                    routes_in: count(routes_in),
                    routes_out: count(routes_out),
                })
            },
        )
        .collect()
}

/// Parsed + clamped params for the blob listing (the parse_page shape,
/// with the blob vocabulary instead of entry states).
#[derive(Debug, PartialEq, Eq)]
struct BlobsQuery {
    /// Lowercased hex prefix.
    q: Option<String>,
    ns: Option<i64>,
    residency: Option<i64>,
    offset: u64,
    limit: u64,
}

fn parse_blobs_query(query: Option<&str>) -> Result<BlobsQuery, Response> {
    let mut page = BlobsQuery {
        q: None,
        ns: None,
        residency: None,
        offset: 0,
        limit: LIMIT_DEFAULT,
    };
    for (key, value) in parse_query(query.unwrap_or("")) {
        match key.as_str() {
            "q" if !value.is_empty() => page.q = Some(value.to_lowercase()),
            "q" => {}
            "ns" => {
                page.ns = Some(match value.as_str() {
                    "data" => Namespace::Data.code(),
                    "meta" => Namespace::Meta.code(),
                    _ => return Err(err(ErrorCode::BadRequest, "ns must be data|meta")),
                });
            }
            "residency" => {
                page.residency = Some(match value.as_str() {
                    "resident" => Residency::Resident.code(),
                    "evicted_covered" => Residency::EvictedCovered.code(),
                    "absent" => Residency::Absent.code(),
                    _ => {
                        return Err(err(
                            ErrorCode::BadRequest,
                            "residency must be one of resident|evicted_covered|absent",
                        ));
                    }
                });
            }
            "offset" => {
                page.offset = value
                    .parse()
                    .map_err(|_| err(ErrorCode::BadRequest, "offset must be an integer"))?;
            }
            "limit" => {
                let limit: u64 = value
                    .parse()
                    .map_err(|_| err(ErrorCode::BadRequest, "limit must be an integer"))?;
                page.limit = limit.clamp(1, LIMIT_MAX);
            }
            _ => {} // unknown params are ignored, not errors
        }
    }
    Ok(page)
}

pub(crate) async fn blobs(
    State(app): State<Arc<App>>,
    Extension(caller): Extension<Caller>,
    RawQuery(query): RawQuery,
) -> Response {
    run_blocking(move || {
        require_owner(&caller)?;
        let page = parse_blobs_query(query.as_deref())?;
        Ok(json_response(StatusCode::OK, &blobs_body(&app, &page)?))
    })
    .await
}

fn blobs_body(app: &App, page: &BlobsQuery) -> Result<BlobsPage, Response> {
    let db = read_db(app);
    let conn = db.cache();
    // Prefix match by substr equality, not LIKE — `%`/`_` in q must
    // not wildcard. Non-hex input just matches nothing.
    let filter = "WHERE (?1 IS NULL OR substr(lower(hex(b.hash)), 1, length(?1)) = ?1)
           AND (?2 IS NULL OR b.namespace = ?2)
           AND (?3 IS NULL OR b.residency = ?3)";
    let total: i64 = conn
        .query_row(
            &format!("SELECT COUNT(*) FROM blob b {filter}"),
            rusqlite::params![page.q, page.ns, page.residency],
            |row| row.get(0),
        )
        .map_err(internal)?;
    let blobs = blob_rows(
        conn,
        &format!("{filter} ORDER BY b.hash LIMIT ?4 OFFSET ?5"),
        rusqlite::params![page.q, page.ns, page.residency, page.limit, page.offset],
    )?;
    Ok(BlobsPage {
        blobs,
        total,
        offset: page.offset,
        limit: page.limit,
    })
}

// ---- GET /v1/blobs/{hash} ----

/// GET /v1/blobs/{hash}/bytes — raw blob bytes, the D84 BIOS-from-CAS
/// fetch half (the Play screen asks for each accepted system-file hash
/// until one answers; a friend's 403 falls back to HLE). Serving goes
/// through the same verified windows as /view files; immutable caching
/// is correct because the URL IS the content hash.
pub(crate) async fn blob_bytes(
    State(app): State<Arc<App>>,
    Extension(caller): Extension<Caller>,
    UrlPath(hash): UrlPath<String>,
    method: axum::http::Method,
    headers: axum::http::HeaderMap,
) -> Response {
    if let Err(resp) = require_owner(&caller) {
        return resp;
    }
    let Ok(hash) = hash.to_lowercase().parse::<Blake3>() else {
        return err(ErrorCode::BadRequest, "not a blake3 hex hash");
    };
    let size = {
        let db = lock_db(&app);
        db.cache()
            .query_row(
                "SELECT size FROM blob WHERE hash = ?1",
                rusqlite::params![hash.0.as_slice()],
                |row| row.get::<_, Option<i64>>(0),
            )
            .optional()
            .ok()
            .flatten()
            .flatten()
    };
    let Some(size) = size else {
        return err(ErrorCode::NotFound, "no such blob (or no bytes to serve)");
    };
    let row = crate::vfs::RowMeta {
        hash,
        size: size as u64,
        seek: 0,
    };
    crate::http::file_response(&app, row, &method, &headers, true, None, None)
}

pub(crate) async fn blob_detail(
    State(app): State<Arc<App>>,
    Extension(caller): Extension<Caller>,
    UrlPath(hash): UrlPath<String>,
) -> Response {
    run_blocking(move || {
        require_owner(&caller)?;
        // Case-insensitive like the listing's q; a non-hash is a 400,
        // an unknown hash a 404.
        let hash: Blake3 = hash
            .to_lowercase()
            .parse()
            .map_err(|_| err(ErrorCode::BadRequest, "not a blake3 hex hash"))?;
        Ok(json_response(
            StatusCode::OK,
            &blob_detail_body(&app, &hash)?,
        ))
    })
    .await
}

/// POST /v1/blobs/{hash}/verify (D80): verify one blob right now —
/// re-hash the resident bytes on a background thread, stamp
/// `verified_at` on match, fail the job with evidence on mismatch.
/// Resident literals only: a rebuildable blob verifies by replay,
/// which stays CLI.
pub(crate) async fn blob_verify(
    State(app): State<Arc<App>>,
    Extension(caller): Extension<Caller>,
    UrlPath(hash): UrlPath<String>,
) -> Response {
    run_blocking(move || {
        require_owner(&caller)?;
        let hash: Blake3 = hash
            .to_lowercase()
            .parse()
            .map_err(|_| err(ErrorCode::BadRequest, "not a blake3 hex hash"))?;
        let (blob_id, ns) = {
            let db = lock_db(&app);
            let row = db
                .blob_by_hash(&hash)
                .map_err(internal)?
                .ok_or_else(|| err(ErrorCode::NotFound, "no such blob"))?;
            if row.residency != Residency::Resident {
                return Err(err(
                    ErrorCode::BadRequest,
                    "blob is not on disk — rebuildable blobs verify by replay \
                     (POST /v1/blobs/{hash}/materialize first, which re-verifies as it rebuilds)",
                ));
            }
            (row.blob_id, row.namespace)
        };
        let now = crate::auth::now_unix();
        let job = app
            .jobs
            .create_scrub(&format!("verify — {}", &hash.to_hex()[..8]), 1, now);
        let app = Arc::clone(&app);
        std::thread::spawn(move || verify_one(&app, job, blob_id, ns, &hash));
        Ok(json_response(
            StatusCode::ACCEPTED,
            &datboi_api::VerifyStartResponse { job },
        ))
    })
    .await
}

/// The verify-one worker. Rides the scrub primitive
/// (`verify_with_aliases`) so a pass also back-fills the alias tuple,
/// exactly like a CLI scrub read would.
fn verify_one(app: &App, job: i64, blob_id: i64, ns: Namespace, hash: &Blake3) {
    use datboi_store_fs::VerifyOutcome;
    let store_ns = match ns {
        Namespace::Data => datboi_store_fs::Namespace::Data,
        Namespace::Meta => datboi_store_fs::Namespace::Meta,
    };
    let now = crate::auth::now_unix();
    match app.store.verify_with_aliases(store_ns, hash) {
        Ok((VerifyOutcome::Valid, aliases)) => {
            {
                let db = lock_db(app);
                if let Some(aliases) = &aliases
                    && let Err(e) = db.insert_aliases(blob_id, aliases)
                {
                    tracing::warn!("verify {}: alias back-fill failed: {e}", hash.to_hex());
                }
                if let Err(e) = db.set_verified(blob_id, now) {
                    app.jobs
                        .fail(job, &format!("verified, but the stamp failed: {e}"), now);
                    return;
                }
            }
            app.jobs.refine_progress(job, 1, 1);
            app.jobs.finish(job, crate::auth::now_unix());
        }
        Ok((VerifyOutcome::Corrupt { actual }, _)) => {
            app.jobs.fail(
                job,
                &format!(
                    "CORRUPT: bytes on disk hash to {}, not {}",
                    actual.to_hex(),
                    hash.to_hex()
                ),
                crate::auth::now_unix(),
            );
        }
        Ok((VerifyOutcome::Missing, _)) => {
            // Index said resident, store had no bytes — self-heal by
            // demoting the row (D81) so the lie can't repeat.
            {
                let db = lock_db(app);
                if let Err(e) = db.set_residency(blob_id, Residency::Absent) {
                    tracing::warn!("verify {}: demote failed: {e}", hash.to_hex());
                }
            }
            app.jobs.fail(
                job,
                "no bytes on disk — index said resident and has been demoted to absent (D81)",
                crate::auth::now_unix(),
            );
        }
        Err(e) => {
            app.jobs.fail(job, &e.to_string(), crate::auth::now_unix());
        }
    }
}

// ---- POST /v1/blobs/{hash}/materialize ----

/// Rematerialize an evicted/claimed blob by replaying its cheapest
/// rebuild route (D25/D27) — the same `exec.materialize` the CLI's
/// `materialize` command runs. Synchronous: one blob's replay, bounded
/// work; an already-resident blob is a no-op success.
pub(crate) async fn blob_materialize(
    State(app): State<Arc<App>>,
    Extension(caller): Extension<Caller>,
    UrlPath(hash): UrlPath<String>,
) -> Response {
    run_blocking(move || {
        require_owner(&caller)?;
        let hash: Blake3 = hash
            .to_lowercase()
            .parse()
            .map_err(|_| err(ErrorCode::BadRequest, "not a blake3 hex hash"))?;
        let db = lock_db(&app);
        let row = db
            .blob_by_hash(&hash)
            .map_err(internal)?
            .ok_or_else(|| err(ErrorCode::NotFound, "no such blob"))?;
        if row.residency == Residency::Resident {
            return Ok(json_response(StatusCode::OK, &datboi_api::OkResponse { ok: true }));
        }
        app.exec.materialize(&db, &hash).map_err(|e| {
            err(ErrorCode::Internal, &format!("materialize failed: {e}"))
        })?;
        tracing::info!("materialize: {} replayed", hash.to_hex());
        Ok(json_response(StatusCode::OK, &datboi_api::OkResponse { ok: true }))
    })
    .await
}

// ---- POST /v1/scrub ----

/// Trigger a corpus scrub (D96): the same walk `datboi scrub` runs,
/// descended to `Executor::scrub`. Long-running, so it starts a Scrub
/// job on a background thread with a PRIVATE db connection (a
/// minutes-long corpus walk must never hold the pipeline write mutex —
/// the D71 refiner posture) and answers the job id.
pub(crate) async fn scrub(
    State(app): State<Arc<App>>,
    Extension(caller): Extension<Caller>,
    crate::http::ApiJson(req): crate::http::ApiJson<datboi_api::ScrubRequest>,
) -> Response {
    run_blocking(move || {
        require_owner(&caller)?;
        let sample_pct = req.sample_pct.unwrap_or(100);
        if sample_pct > 100 {
            return Err(err(ErrorCode::BadRequest, "sample_pct must be 0..=100"));
        }
        let rehabilitate = req.rehabilitate.unwrap_or(false);
        let now = auth::now_unix();
        let job = app
            .jobs
            .create_scrub(&format!("scrub — {sample_pct}% sample"), 0, now);
        let app = Arc::clone(&app);
        std::thread::spawn(move || run_scrub(&app, job, sample_pct, rehabilitate));
        Ok(json_response(
            StatusCode::ACCEPTED,
            &datboi_api::JobStartResponse { job },
        ))
    })
    .await
}

/// The scrub worker. A byte disproof is a report finding, not an error
/// (D81), so a run that finds corruption still FINISHES (never fails) —
/// the note carries the counts and a bounded list, and a WARN logs the
/// bad bytes for the operator. Only an environmental error fails the job.
fn run_scrub(app: &App, job: i64, sample_pct: u8, rehabilitate: bool) {
    // How many corrupt/missing hashes to spell out in the note before
    // deferring to the logs — a rotten pack could name thousands.
    const LIST_CAP: usize = 20;
    let db = match Db::open(&app.db_dir) {
        Ok(db) => db,
        Err(e) => {
            app.jobs
                .fail(job, &format!("scrub: open db: {e}"), auth::now_unix());
            return;
        }
    };
    let report = match app.exec.scrub(&db, sample_pct, rehabilitate, auth::now_unix()) {
        Ok(report) => report,
        Err(e) => {
            app.jobs.fail(job, &e.to_string(), auth::now_unix());
            return;
        }
    };
    let mut note = format!(
        "checked {} blob(s), {} row(s) refreshed; {} corrupt, {} missing",
        report.checked,
        report.refreshed,
        report.corrupt.len(),
        report.missing.len()
    );
    if rehabilitate {
        note.push_str(&format!(
            "; {} recipe(s) rehabilitated, {} still poisoned",
            report.rehabilitated.len(),
            report.still_failed.len()
        ));
    }
    app.jobs.push_note(job, note);
    if !report.is_clean() {
        let bad: Vec<&String> = report
            .corrupt
            .iter()
            .chain(report.missing.iter())
            .take(LIST_CAP)
            .collect();
        let more = (report.corrupt.len() + report.missing.len()).saturating_sub(LIST_CAP);
        app.jobs.push_note(
            job,
            format!(
                "problem bytes: {}{}",
                bad.iter().map(|h| h.as_str()).collect::<Vec<_>>().join(", "),
                if more > 0 {
                    format!(" (+{more} more — see the daemon log)")
                } else {
                    String::new()
                }
            ),
        );
        tracing::warn!(
            "scrub job {job}: {} corrupt, {} missing — {:?} {:?}",
            report.corrupt.len(),
            report.missing.len(),
            report.corrupt,
            report.missing
        );
    }
    app.jobs.refine_progress(job, report.checked, report.checked);
    app.jobs.finish(job, auth::now_unix());
    tracing::info!(
        "scrub job {job}: {} checked, {} refreshed",
        report.checked,
        report.refreshed
    );
}

// ---- POST /v1/evict ----

/// Reclaim resident bytes by evicting recipe-covered literals (D72/D96).
/// `dry_run` answers the plan synchronously (a read — the D27 preview
/// surface); a real run claims the D72 singleton guard up front (a busy
/// guard is a clean 503, never a failed job) and drops on a background
/// Gc job with a private connection — the CLI `evict` verb, over serve.
pub(crate) async fn evict(
    State(app): State<Arc<App>>,
    Extension(caller): Extension<Caller>,
    crate::http::ApiJson(req): crate::http::ApiJson<EvictRequest>,
) -> Response {
    run_blocking(move || {
        require_owner(&caller)?;
        let license = req.license.unwrap_or(false);
        if req.dry_run.unwrap_or(false) {
            return evict_plan(&app);
        }
        // Claim the D72 guard before answering: this endpoint racing the
        // daemon's watermark eviction or a gc apply is exactly the
        // jointly-stranded-pair hazard the guard exists for, and a busy
        // guard should surface as a retryable 503, not as a job that
        // starts and immediately dies.
        let mut bytes = [0u8; 16];
        getrandom::getrandom(&mut bytes)
            .map_err(|e| err(ErrorCode::Internal, &format!("entropy: {e}")))?;
        let holder = GuardHolder(bytes);
        if !crate::maintain::claim_guard(&lock_db(&app), &holder) {
            return Err(err(
                ErrorCode::Busy,
                "gc guard busy (an eviction or apply is running); retry shortly",
            ));
        }
        let job = app
            .jobs
            .create_gc("evict — on demand", 0, auth::now_unix());
        let app = Arc::clone(&app);
        std::thread::spawn(move || run_evict(&app, job, req.target_bytes, license, holder));
        Ok(json_response(
            StatusCode::ACCEPTED,
            &datboi_api::JobStartResponse { job },
        ))
    })
    .await
}

/// The dry-run plan (read-only, no guard): what would drop at the
/// current target, and every held-back candidate with its D25/D27
/// reasons — the same numbers `datboi evict --dry-run` prints.
fn evict_plan(app: &App) -> Result<Response, Response> {
    let db = read_db(app);
    let mut evictable: u64 = 0;
    let mut reclaimable_bytes: u64 = 0;
    let mut blocked: Vec<EvictBlocked> = Vec::new();
    for candidate in db.list_eviction_candidates().map_err(internal)? {
        if db.is_evictable(candidate.blob_id).map_err(internal)? {
            evictable += 1;
            reclaimable_bytes += candidate.size.unwrap_or(0);
        } else {
            blocked.push(EvictBlocked {
                hash: candidate.hash.to_hex(),
                reasons: app
                    .exec
                    .explain_eviction(&db, &candidate.hash)
                    .map_err(internal)?,
            });
        }
    }
    Ok(json_response(
        StatusCode::OK,
        &EvictPlan {
            evictable,
            reclaimable_bytes,
            blocked,
        },
    ))
}

/// The eviction worker: a private connection (the corpus-scale drop must
/// not hold the pipeline mutex), then release the guard on the shared
/// connection (holder-keyed, so the connection is immaterial) and fold
/// the report into the job. The guard's TTL is the backstop if this
/// thread dies mid-drop.
fn run_evict(app: &App, job: i64, target_bytes: u64, license: bool, holder: GuardHolder) {
    let db = match Db::open(&app.db_dir) {
        Ok(db) => db,
        Err(e) => {
            let _ = lock_db(app).release_gc_guard(&holder);
            app.jobs
                .fail(job, &format!("evict: open db: {e}"), auth::now_unix());
            return;
        }
    };
    let result = app.exec.evict_covered(&db, target_bytes, license);
    let _ = lock_db(app).release_gc_guard(&holder);
    match result {
        Ok(report) => {
            let evicted = report.evicted as u64;
            app.jobs.refine_progress(job, evicted, evicted);
            app.jobs.push_note(
                job,
                format!(
                    "{} blob(s) evicted, {} byte(s) reclaimed, {} licensing replay(s), {} blocked",
                    report.evicted,
                    report.bytes_reclaimed,
                    report.replays,
                    report.blocked.len()
                ),
            );
            app.jobs.finish(job, auth::now_unix());
            tracing::info!(
                "evict job {job}: {} evicted, {} byte(s)",
                report.evicted,
                report.bytes_reclaimed
            );
        }
        Err(e) => {
            tracing::warn!("evict job {job}: FAILED — {e}");
            app.jobs.fail(job, &e.to_string(), auth::now_unix());
        }
    }
}

// ---- POST /v1/sweep ----

/// Run one analyzer sweep round on demand (D71/D96): the manual
/// equivalent of the ambient refiner's per-family drain. The name is
/// validated up front (unknown → 400 before any job); the drain runs on
/// a background Refine job over a private connection (a preflate split
/// is minutes-long and must not hold the pipeline mutex). Leases make a
/// manual sweep and the ambient refiner claiming the same family safe.
pub(crate) async fn sweep(
    State(app): State<Arc<App>>,
    Extension(caller): Extension<Caller>,
    crate::http::ApiJson(req): crate::http::ApiJson<datboi_api::SweepRequest>,
) -> Response {
    run_blocking(move || {
        require_owner(&caller)?;
        // Validate here so an unknown name is a clean 400; the worker
        // builds its own analyzer (trait objects stay thread-local).
        let Some(probe) = datboi_ingest::analyzers::analyzer_for(&req.analyzer) else {
            return Err(err(
                ErrorCode::BadRequest,
                &format!(
                    "unknown analyzer {:?} (available: {})",
                    req.analyzer,
                    datboi_ingest::analyzers::SWEEP_ANALYZERS.join(", ")
                ),
            ));
        };
        let family = probe.family().to_owned();
        drop(probe);
        let limit = usize::try_from(req.limit.unwrap_or(10_000)).unwrap_or(usize::MAX);
        let job = app.jobs.create_refine(&family, 0, auth::now_unix());
        let app = Arc::clone(&app);
        let name = req.analyzer;
        std::thread::spawn(move || run_sweep_job(&app, job, &name, limit));
        Ok(json_response(
            StatusCode::ACCEPTED,
            &datboi_api::JobStartResponse { job },
        ))
    })
    .await
}

/// The sweep worker: a private connection, then one `run_sweep` round
/// over the logical CAS (D92 — the executor serves absent-but-grounded
/// items), folding the outcome counts into the Refine job.
fn run_sweep_job(app: &App, job: i64, analyzer_name: &str, limit: usize) {
    let mut db = match Db::open(&app.db_dir) {
        Ok(db) => db,
        Err(e) => {
            app.jobs
                .fail(job, &format!("sweep: open db: {e}"), auth::now_unix());
            return;
        }
    };
    // Validated in the handler; a None here would be a logic error.
    let Some(mut analyzer) = datboi_ingest::analyzers::analyzer_for(analyzer_name) else {
        app.jobs
            .fail(job, "sweep: analyzer vanished between validate and run", auth::now_unix());
        return;
    };
    let bytes = datboi_ingest::refine::Logical::new(app.store, &app.exec);
    let report =
        match datboi_ingest::refine::run_sweep(&mut db, app.store, &bytes, analyzer.as_mut(), limit)
        {
            Ok(report) => report,
            Err(e) => {
                app.jobs.fail(job, &e.to_string(), auth::now_unix());
                return;
            }
        };
    if report.disabled {
        // A disabled family (D60) is a policy state, not a failure — the
        // note says so and the job finishes cleanly.
        app.jobs.push_note(
            job,
            format!(
                "analyzer family {:?} is disabled (D60): enable it via PUT /v1/analyzers/{}",
                analyzer.family(),
                analyzer.family()
            ),
        );
        app.jobs.finish(job, auth::now_unix());
        return;
    }
    let remaining = db.sweep_queue_len(&analyzer.id()).unwrap_or(0);
    for (hash, error) in &report.errors {
        app.jobs.refine_error(job, &hash.to_hex(), error);
    }
    let analyzed = report.analyzed as u64;
    app.jobs.refine_progress(job, analyzed, analyzed + remaining);
    app.jobs.push_note(
        job,
        format!(
            "{} enqueued, {} analyzed ({} positive, {} negative), {} error(s), {} still queued",
            report.enqueued,
            report.analyzed,
            report.positive,
            report.negative,
            report.errors.len(),
            remaining
        ),
    );
    app.jobs.finish(job, auth::now_unix());
    tracing::info!(
        "sweep job {job}: {} analyzed ({} positive), {remaining} queued",
        report.analyzed,
        report.positive
    );
}

// ---- POST /v1/snapshot ----

/// Mint a state snapshot on demand (D75/D96): the same `statesnap::mint`
/// `datboi snapshot` runs — the manual trigger beside the daemon's
/// dirty-triggered auto-cadence (D75). Synchronous under the pipeline
/// writer (the mint is the CLI's own bounded write; a snapshot is not
/// yet a byte-level job kind — open-questions) and owner-only.
pub(crate) async fn snapshot(
    State(app): State<Arc<App>>,
    Extension(caller): Extension<Caller>,
) -> Response {
    run_blocking(move || {
        require_owner(&caller)?;
        // The daemon signs with the instance identity — the same key the
        // ambient auto-cadence rider (maintain.rs) and `datboi snapshot`
        // use; one definition, one signature.
        let identity =
            datboi_catalog::statesnap::load_or_create_identity(&app.db_dir).map_err(internal)?;
        let db = lock_db(&app);
        let report = datboi_catalog::statesnap::mint(app.store, &db, &identity, auth::now_unix())
            .map_err(internal)?;
        tracing::info!("snapshot: {} (seq {})", report.hash, report.sequence);
        Ok(json_response(
            StatusCode::OK,
            &datboi_api::SnapshotResponse {
                hash: report.hash.to_hex(),
                sequence: report.sequence,
                sources: report.sources as u64,
                alias_rows: report.alias_rows,
                analysis_rows: report.analysis_rows,
                new_batch_blobs: report.new_batch_blobs,
            },
        ))
    })
    .await
}

/// The claims a blob satisfies: identity links of any evidence grade
/// (explanation, not the D39 holdings rollup) → rom_claim → entry,
/// restricted to each source's CURRENT revision. DISTINCT because an
/// entry with several claims resolving to one identity is a single
/// answer to "what is this?".
const CLAIMS_SQL: &str =
    "SELECT DISTINCT e.name AS entry, ds.provider || '/' || ds.system AS source
     FROM identity_blob ib
     JOIN rom_claim rc ON rc.identity_id = ib.identity_id
     JOIN entry e ON e.entry_id = rc.entry_id
     JOIN dat_revision dr ON dr.revision_id = e.revision_id
     JOIN dat_source ds ON ds.source_id = dr.source_id
                       AND ds.current_revision_id = dr.revision_id
     WHERE ib.blob_id = ?1";

/// The compiled magic database (nixpkgs' file(1) magic.mgc), embedded
/// at build time so the binary stays self-contained (D66; wired by
/// build.rs like the web dist).
const MAGIC_DB: &[u8] = include_bytes!(env!("DATBOI_MAGIC_DB"));

std::thread_local! {
    /// One libmagic cookie per blocking-pool thread: the cookie wraps
    /// a raw pointer (!Send), and loading the database costs a few ms
    /// — paid once per thread, never per request. `None` if libmagic
    /// refuses the embedded db; the sniff just goes quiet.
    static MAGIC: Option<magic::cookie::Cookie<magic::cookie::Load>> =
        match magic::cookie::Cookie::open(magic::cookie::Flags::empty()) {
            Err(e) => {
                tracing::warn!("libmagic open failed — sniff disabled: {e}");
                None
            }
            Ok(open) => match open.load_buffers(&[MAGIC_DB]) {
                Ok(cookie) => Some(cookie),
                Err(e) => {
                    tracing::warn!(
                        "libmagic rejected the embedded magic.mgc — sniff disabled: {e}"
                    );
                    None
                }
            },
        };
}

/// Magic-byte display sniff (D79 headline fallback): libmagic over the
/// first 64 KiB of the resident bytes. A hint, never identity — D18
/// keeps blobs untyped. Note the trade-off, eyes open: libmagic is a
/// native C parser of wild bytes, which D58 normally banishes to wasm;
/// it is admitted here because the surface is owner-only display over
/// bytes the OWNER ingested, on a bounded head — not peer input.
/// Dats keep their own label first: libmagic would answer "XML
/// document", and we know better.
fn sniff_blob(app: &App, hash: &Blake3) -> Option<String> {
    use std::io::Read as _;
    let file = app
        .store
        .get(datboi_store_fs::Namespace::Data, hash)
        .ok()??;
    let mut head = Vec::with_capacity(64 * 1024);
    file.take(64 * 1024).read_to_end(&mut head).ok()?;
    if datboi_formats::detect(&head).is_some() {
        return Some("dat file".to_owned());
    }
    MAGIC.with(|cookie| {
        let desc = cookie.as_ref()?.buffer(&head).ok()?;
        // "data" is libmagic's shrug — an empty answer, not a label.
        if desc == "data" { None } else { Some(desc) }
    })
}

fn blob_detail_body(app: &App, hash: &Blake3) -> Result<BlobDetail, Response> {
    let mut detail = {
        let db = read_db(app);
        let conn = db.cache();
        let Some((blob_id, size, ns, residency, verified_at)) = conn
            .query_row(
                "SELECT blob_id, size, namespace, residency, verified_at
                 FROM blob WHERE hash = ?1",
                rusqlite::params![hash.0.as_slice()],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, Option<i64>>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, Option<i64>>(4)?,
                    ))
                },
            )
            .optional()
            .map_err(internal)?
        else {
            return Err(err(ErrorCode::NotFound, "no such blob"));
        };

        // Alias digests (D22). ChdSha1 attests decompressed content,
        // not these bytes (D44) — excluded. First row per algo wins
        // (deterministic by the ORDER BY; multi-digest rows would mean
        // a same-blob hash collision).
        let mut aliases = RomHashes::default();
        let mut stmt = conn
            .prepare("SELECT algo, digest FROM alias WHERE blob_id = ?1 ORDER BY algo, digest")
            .map_err(internal)?;
        let alias_rows: Vec<(i64, Vec<u8>)> = stmt
            .query_map([blob_id], |row| Ok((row.get(0)?, row.get(1)?)))
            .map_err(internal)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(internal)?;
        for (algo, digest) in alias_rows {
            let slot = match AliasAlgo::from_code(algo).map_err(internal)? {
                AliasAlgo::Crc32 => &mut aliases.crc32,
                AliasAlgo::Md5 => &mut aliases.md5,
                AliasAlgo::Sha1 => &mut aliases.sha1,
                AliasAlgo::Sha256 => &mut aliases.sha256,
                AliasAlgo::ChdSha1 => continue,
            };
            slot.get_or_insert_with(|| hex(&digest));
        }

        // Provenance: every rescan-cache path that hashed to this blob.
        let mut stmt = conn
            .prepare("SELECT path, scanned_at FROM source_file WHERE blob_id = ?1 ORDER BY path")
            .map_err(internal)?;
        let provenance = stmt
            .query_map([blob_id], |row| {
                Ok(ProvenanceRow {
                    path: row.get(0)?,
                    ingested_at: Nullable(row.get(1)?),
                })
            })
            .map_err(internal)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(internal)?;

        // One DAG hop each way: recipes producing this blob, recipes
        // consuming it. Poisoned recipes drop out inside route_edge.
        let mut routes_in = Vec::new();
        for recipe in db.recipes_for_output(blob_id).map_err(internal)? {
            routes_in.extend(route_edge(&db, &recipe)?);
        }
        let mut stmt = conn
            .prepare(
                "SELECT DISTINCT recipe_id FROM recipe_input WHERE blob_id = ?1 ORDER BY recipe_id",
            )
            .map_err(internal)?;
        let consuming: Vec<i64> = stmt
            .query_map([blob_id], |row| row.get(0))
            .map_err(internal)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(internal)?;
        let mut routes_out = Vec::new();
        for recipe_id in consuming {
            let recipe = db.recipe_by_id(recipe_id).map_err(internal)?;
            routes_out.extend(route_edge(&db, &recipe)?);
        }

        let claims_total: i64 = conn
            .query_row(
                &format!("SELECT COUNT(*) FROM ({CLAIMS_SQL})"),
                [blob_id],
                |row| row.get(0),
            )
            .map_err(internal)?;
        let mut stmt = conn
            .prepare(&format!("{CLAIMS_SQL} ORDER BY source, entry LIMIT 100"))
            .map_err(internal)?;
        let claims = stmt
            .query_map([blob_id], |row| {
                Ok(ClaimRef {
                    entry: row.get(0)?,
                    source: row.get(1)?,
                })
            })
            .map_err(internal)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(internal)?;

        // D79: meaning from the edges. One UNDIRECTED walk collects
        // this blob's recipe-connected component (capped — a runaway
        // component truncates instead of hanging the request); two
        // DIRECTED walks classify each claimed root found in it.
        let walk = |sql: &str| -> Result<Vec<i64>, Response> {
            let mut stmt = conn.prepare(sql).map_err(internal)?;
            let ids = stmt
                .query_map([blob_id], |row| row.get::<_, i64>(0))
                .map_err(internal)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(internal)?;
            Ok(ids)
        };
        let comp = walk(&format!(
            "WITH RECURSIVE {DAG_EDGES_CTE},
             comp(id) AS (
               VALUES(?1)
               UNION
               SELECT CASE WHEN e.a = c.id THEN e.b ELSE e.a END
               FROM comp c JOIN edges e ON e.a = c.id OR e.b = c.id)
             SELECT id FROM comp LIMIT 10000"
        ))?;
        let down: std::collections::HashSet<i64> = walk(&format!(
            "WITH RECURSIVE {DAG_EDGES_CTE},
             down(id) AS (
               VALUES(?1)
               UNION
               SELECT e.b FROM down d JOIN edges e ON e.a = d.id)
             SELECT id FROM down LIMIT 10000"
        ))?
        .into_iter()
        .collect();
        let up: std::collections::HashSet<i64> = walk(&format!(
            "WITH RECURSIVE {DAG_EDGES_CTE},
             up(id) AS (
               VALUES(?1)
               UNION
               SELECT e.a FROM up u JOIN edges e ON e.b = u.id)
             SELECT id FROM up LIMIT 10000"
        ))?
        .into_iter()
        .collect();

        let in_list = |ids: &[i64]| {
            ids.iter()
                .map(|id| id.to_string())
                .collect::<Vec<_>>()
                .join(",")
        };
        // Claimed roots in the component (self excluded — a direct
        // claim already renders as `claims`).
        let others: Vec<i64> = comp.iter().copied().filter(|id| *id != blob_id).collect();
        let mut roots = Vec::new();
        if !others.is_empty() {
            let root_ids = {
                let mut stmt = conn
                    .prepare(&format!(
                        "SELECT DISTINCT ib.blob_id
                         FROM identity_blob ib
                         JOIN rom_claim rc ON rc.identity_id = ib.identity_id
                         WHERE ib.blob_id IN ({})
                         ORDER BY ib.blob_id LIMIT 20",
                        in_list(&others)
                    ))
                    .map_err(internal)?;
                stmt.query_map([], |row| row.get::<_, i64>(0))
                    .map_err(internal)?
                    .collect::<Result<Vec<i64>, _>>()
                    .map_err(internal)?
            };
            for root_id in root_ids {
                // Direction classifies the relation: the root sits
                // downstream of an ingredient, upstream of a product.
                let relation = if down.contains(&root_id) {
                    RootRelation::Makes
                } else if up.contains(&root_id) {
                    RootRelation::DerivedFrom
                } else {
                    RootRelation::Related
                };
                let row = conn
                    .query_row(
                        &format!(
                            "SELECT b.hash, x.entry, x.source
                             FROM blob b,
                                  ({CLAIMS_SQL} ORDER BY source, entry LIMIT 1) x
                             WHERE b.blob_id = ?1"
                        ),
                        [root_id],
                        |row| {
                            Ok((
                                row.get::<_, [u8; 32]>(0)?,
                                row.get::<_, String>(1)?,
                                row.get::<_, String>(2)?,
                            ))
                        },
                    )
                    .optional()
                    .map_err(internal)?;
                if let Some((root_hash, entry, source)) = row {
                    roots.push(RootRef {
                        hash: Blake3(root_hash).to_hex(),
                        entry,
                        source,
                        relation,
                    });
                }
            }
        }

        // Viral provenance display (D79): source paths carried by
        // CONNECTED blobs — a chunk's bytes arrived as somebody's
        // zip, and that path lives on the container literal.
        let provenance_via = if others.is_empty() {
            Vec::new()
        } else {
            let mut stmt = conn
                .prepare(&format!(
                    "SELECT sf.path, sf.scanned_at, b.hash
                     FROM source_file sf
                     JOIN blob b ON b.blob_id = sf.blob_id
                     WHERE sf.blob_id IN ({})
                     ORDER BY sf.path LIMIT 10",
                    in_list(&others)
                ))
                .map_err(internal)?;
            stmt.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<i64>>(1)?,
                    row.get::<_, [u8; 32]>(2)?,
                ))
            })
            .map_err(internal)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(internal)?
            .into_iter()
            .map(|(path, at, via)| ProvenanceViaRow {
                path,
                ingested_at: at.into(),
                via: Blake3(via).to_hex(),
            })
            .collect()
        };

        // Magic-byte sniff of resident bytes: a display hint for the
        // headline fallback, never identity (D18).
        let sniff = if Residency::from_code(residency).map_err(internal)? == Residency::Resident {
            sniff_blob(app, hash)
        } else {
            None
        };

        BlobDetail {
            hash: hash.to_hex(),
            size: size.into(),
            namespace: ns_label(ns)?.to_owned(),
            residency: residency_state(residency)?,
            verified_at: verified_at.into(),
            digests: BlobDigests {
                blake3: hash.to_hex(),
                aliases,
            },
            provenance,
            provenance_via,
            routes_in,
            routes_out,
            claims,
            claims_total: u64::try_from(claims_total).expect("COUNT(*) is non-negative"),
            roots,
            sniff: sniff.into(),
            pins: Vec::new(), // filled below, after the lock drops
        }
    };

    // Pins: which views' CURRENT snapshots reference the blob (D33 —
    // the tag is what pins; the entry-detail path computes per-claim
    // pins the same way). vfs::view_tags takes the db lock internally.
    let tags = vfs::view_tags(app).map_err(internal)?;
    detail.pins = tags
        .iter()
        .filter(|(_, snap)| {
            vfs::snapshot_index(app, *snap).is_ok_and(|idx| idx.contains_hash(hash))
        })
        .map(|(name, _)| name.clone())
        .collect();
    Ok(detail)
}

/// One recipe as a navigable DAG edge; `None` for poisoned recipes
/// (D25 terminal — not a route anywhere in the API). `name` carries
/// the output member name / input role when the index recorded one.
fn route_edge(
    db: &Db,
    recipe: &datboi_index::recipes::RecipeRow,
) -> Result<Option<RouteEdge>, Response> {
    if recipe.verify == VerifyState::Failed {
        return Ok(None);
    }
    let conn = db.cache();
    let mut stmt = conn
        .prepare(
            "SELECT b.hash, b.size, ri.role
             FROM recipe_input ri JOIN blob b ON b.blob_id = ri.blob_id
             WHERE ri.recipe_id = ?1 ORDER BY ri.position",
        )
        .map_err(internal)?;
    let inputs = hash_refs(&mut stmt, recipe.recipe_id)?;
    let mut stmt = conn
        .prepare(
            "SELECT b.hash, b.size, ro.name
             FROM recipe_output ro JOIN blob b ON b.blob_id = ro.blob_id
             WHERE ro.recipe_id = ?1 ORDER BY ro.ordinal",
        )
        .map_err(internal)?;
    let outputs = hash_refs(&mut stmt, recipe.recipe_id)?;
    Ok(Some(RouteEdge {
        op: recipe.op_name.clone(),
        verify: match recipe.verify {
            VerifyState::Pending => RouteVerify::Pending,
            VerifyState::Verified => RouteVerify::Verified,
            VerifyState::ReplayedLocal => RouteVerify::ReplayedLocal,
            VerifyState::Failed => unreachable!("filtered above"),
        },
        inputs,
        outputs,
    }))
}

fn hash_refs(stmt: &mut rusqlite::Statement<'_>, recipe_id: i64) -> Result<Vec<HashRef>, Response> {
    let rows: Vec<([u8; 32], Option<i64>, Option<String>)> = stmt
        .query_map([recipe_id], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })
        .map_err(internal)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(internal)?;
    Ok(rows
        .into_iter()
        .map(|(hash, size, name)| HashRef {
            hash: Blake3(hash).to_hex(),
            size: Nullable(size),
            name: Nullable(name),
        })
        .collect())
}

// ---- GET /v1/jobs (+ /{id}) ----

/// The in-memory job registry (jobs.rs): running jobs plus a bounded
/// finished tail. Scrub/eviction still run as CLI processes; a
/// durable job table stays the recorded open question ("Jobs tray
/// backend").
pub(crate) async fn jobs(
    State(app): State<Arc<App>>,
    Extension(caller): Extension<Caller>,
) -> Response {
    match require_owner(&caller) {
        Ok(()) => json_response(
            StatusCode::OK,
            &JobsResponse {
                jobs: app.jobs.list(),
            },
        ),
        Err(resp) => resp,
    }
}

pub(crate) async fn job_detail(
    State(app): State<Arc<App>>,
    Extension(caller): Extension<Caller>,
    UrlPath(id): UrlPath<String>,
) -> Response {
    match require_owner(&caller) {
        Ok(()) => {
            let Ok(id) = id.parse::<i64>() else {
                return err(ErrorCode::BadRequest, "job id must be an integer");
            };
            match app.jobs.detail(id) {
                Some(detail) => json_response(StatusCode::OK, &detail),
                None => err(ErrorCode::NotFound, "no such job"),
            }
        }
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
    fn blob_query_params_clamp_and_reject() {
        let page = parse_blobs_query(None).expect("defaults");
        assert_eq!(
            page,
            BlobsQuery {
                q: None,
                ns: None,
                residency: None,
                offset: 0,
                limit: LIMIT_DEFAULT
            }
        );
        let page = parse_blobs_query(Some(
            "q=ABCD&ns=meta&residency=evicted_covered&limit=999999",
        ))
        .expect("parse");
        assert_eq!(page.q.as_deref(), Some("abcd"), "prefix lowercases");
        assert_eq!((page.ns, page.residency), (Some(1), Some(1)));
        assert_eq!(page.limit, LIMIT_MAX, "limit clamps to the max");
        assert!(parse_blobs_query(Some("ns=bogus")).is_err());
        assert!(parse_blobs_query(Some("residency=bogus")).is_err());
        assert!(parse_blobs_query(Some("limit=abc")).is_err());
        assert_eq!(
            parse_blobs_query(Some("q=")).expect("parse").q,
            None,
            "empty q is no filter"
        );
    }

    #[test]
    fn state_vocabulary_round_trips() {
        for (code, name, state) in [
            (0, "verified", EntryState::Verified),
            (1, "claimed", EntryState::Claimed),
            (2, "missing", EntryState::Missing),
            (3, "nodump", EntryState::Nodump),
        ] {
            assert_eq!(entry_state(code), state);
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
        let app = App::open(&crate::Config {
            store_root: dir.path().join("store"),
            db_dir,
            listen: "127.0.0.1:0".parse().expect("addr"),
            nfs_listen: None,
            detectors_dir: None,
            refine: false,
            p2p: false,
        })
        .expect("app");
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
            body.views.into_iter().map(|v| v.name).collect()
        };
        assert_eq!(names(&Caller::Local), ["gba", "psx"]);
        assert_eq!(names(&pal), ["gba"]);

        // View detail: granted answers (even with the snapshot object
        // absent — stats just drop out); ungranted answers 404, not 403.
        let detail = view_detail_body(&app, &pal, "gba").expect("granted view");
        assert_eq!(detail.summary.name, "gba");
        assert_eq!(detail.endpoints.http, "/view/gba/");
        assert!(
            detail.summary.rows.is_none(),
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
        assert_eq!(body.total, 3);
        assert_eq!(body.snapshot, hash.to_hex());
        assert_eq!(body.files.len(), 3);
        assert_eq!(body.files[0].path, "Games/Alpha (USA).gba", "path-ordered");
        assert_eq!(body.files[0].size, 9);
        assert_eq!(
            body.files[0].hash,
            Blake3::compute(b"Games/Alpha (USA).gba").to_hex()
        );

        // q is case-insensitive substring on the FULL path; total is
        // the filtered count, the window slides under it.
        let body = view_files_body(&app, &pal, "gba", &page(Some("ALPHA"), 0, 200)).expect("q");
        assert_eq!(body.total, 1);
        assert_eq!(body.files[0].path, "Games/Alpha (USA).gba");
        let body = view_files_body(&app, &pal, "gba", &page(Some("games/"), 1, 1)).expect("page");
        assert_eq!(body.total, 2);
        assert_eq!(body.files.len(), 1);
        assert_eq!(body.files[0].path, "Games/Beta (Japan).gba");

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
        assert_eq!(systems.systems.len(), 0);
        let storage = storage_body(&app).expect("storage");
        assert_eq!(storage.blob_count, 0);
        assert_eq!(storage.quarantine.count, 0);
        let breakdown = breakdown_body(&app).expect("breakdown");
        assert!(breakdown.by_class.is_empty());
        assert!(breakdown.by_source.is_empty(), "no lying zero rows");
        assert!(breakdown.largest.is_empty());
        let page = blobs_body(&app, &parse_blobs_query(None).expect("defaults")).expect("blobs");
        assert_eq!((page.total, page.blobs.len()), (0, 0));
        let miss = blob_detail_body(&app, &Blake3::compute(b"nothing")).expect_err("miss");
        assert_eq!(miss.status(), StatusCode::NOT_FOUND);
    }
}
