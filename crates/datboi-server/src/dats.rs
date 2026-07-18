//! Dat import over HTTP (docs/dats.md) — request-sized: bytes in,
//! report out, and the CLI path buffers the whole file exactly the same
//! way (`std::fs::read` in cmds.rs). Ingest has the opposite shape
//! (streamed staging + a background job — see ingest.rs). Under D96 the
//! serve+web surface is the complete one and every verb graduates here;
//! this was among the first, back when the split was still read-model
//! vs CLI-only.

// Same rationale as http.rs: fallible steps short-circuit with the
// error Response itself.
#![allow(clippy::result_large_err)]

use std::sync::Arc;

use axum::Extension;
use axum::body::{Body, Bytes};
use axum::extract::{Path as UrlPath, RawQuery, State};
use axum::http::{StatusCode, header};
use axum::response::Response;
use datboi_api::{
    ClonelistResponse, DatDiffResponse, DatFetchResponse, DatImportResponse, DatNameChange,
    ErrorCode,
};
use datboi_catalog::{
    CatalogError, ImportOptions, ImportReport, diff_source, export_dat, fetch_dat,
    import_clonelist, import_dat,
};
use datboi_index::Db;

use crate::App;
use crate::api::{err, parse_query, require_owner};
use crate::auth::{self, Caller};
use crate::http::{json_response, run_blocking};

/// Route-level body cap for a linked clonelist: retool's per-system JSON
/// is small, but the full multi-system file runs to a few MiB — 32 MiB
/// clears it with headroom, well under a real dat.
pub(crate) const CLONELIST_LIMIT: usize = 32 << 20;

/// Route-level body cap (the axum default, 2 MiB, is too small for
/// real dats). Imports buffer fully — `import_dat` parses one
/// contiguous slice, same as the CLI — so this bounds peak memory per
/// request. 512 MiB clears the largest dat in circulation (MAME's full
/// listxml, ~0.3 GiB) with headroom.
pub(crate) const BODY_LIMIT: usize = 512 << 20;

// ---- POST /v1/dats/import ----

pub(crate) async fn import(
    State(app): State<Arc<App>>,
    Extension(caller): Extension<Caller>,
    RawQuery(query): RawQuery,
    body: Bytes,
) -> Response {
    run_blocking(move || {
        require_owner(&caller)?;
        let mut provider = None;
        let mut system = None;
        for (key, value) in parse_query(query.as_deref().unwrap_or("")) {
            match key.as_str() {
                // Empty overrides mean "use the dat header", same as an
                // omitted CLI flag.
                "provider" if !value.is_empty() => provider = Some(value),
                "system" if !value.is_empty() => system = Some(value),
                _ => {} // unknown params are ignored, not errors (api.rs convention)
            }
        }
        if body.is_empty() {
            return Err(err(ErrorCode::BadRequest, "empty request body"));
        }
        let mut db = app
            .db
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let report = import_dat(
            app.store,
            &mut db,
            &body,
            &ImportOptions {
                provider: provider.as_deref(),
                system: system.as_deref(),
                imported_at: auth::now_unix(),
            },
        )
        .map_err(import_err)?;
        let body = import_response(&db, &report)?;
        // Operator log line (stderr is the log until the durable job
        // table — see ingest.rs run_job): a dat import rewrites what
        // the catalog wants, and the response body vanishes with the
        // request.
        tracing::info!(
            "dat import: {}/{} rev {} ({} entries)",
            body.provider,
            body.system,
            body.revision_id,
            body.entries
        );
        Ok(json_response(StatusCode::OK, &body))
    })
    .await
}

// ---- POST /v1/dats/fetch ----

/// Fetch a dat over HTTP and import it (D16/D96): the same
/// `catalog::fetch_dat` + `import_dat` the CLI `dat fetch` verb runs.
/// The network request runs WITHOUT the db lock (a 60 s timeout must
/// never block the pipeline writer); only the import takes it.
pub(crate) async fn fetch(
    State(app): State<Arc<App>>,
    Extension(caller): Extension<Caller>,
    crate::http::ApiJson(req): crate::http::ApiJson<datboi_api::DatFetchRequest>,
) -> Response {
    run_blocking(move || {
        require_owner(&caller)?;
        if req.source.trim().is_empty() {
            return Err(err(ErrorCode::BadRequest, "source is required"));
        }
        let fetched = fetch_dat(&req.source).map_err(import_err)?;
        // Empty overrides mean "use the dat header", same as the CLI.
        let provider = req.provider.as_deref().filter(|s| !s.is_empty());
        let system = req.system.as_deref().filter(|s| !s.is_empty());
        let mut db = app
            .db
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let report = import_dat(
            app.store,
            &mut db,
            &fetched.bytes,
            &ImportOptions {
                provider: provider.or(fetched.provider_default),
                system,
                imported_at: auth::now_unix(),
            },
        )
        .map_err(import_err)?;
        let import = import_response(&db, &report)?;
        tracing::info!(
            "dat fetch: {} -> {}/{} rev {} ({} entries)",
            fetched.url,
            import.provider,
            import.system,
            import.revision_id,
            import.entries
        );
        Ok(json_response(
            StatusCode::OK,
            &DatFetchResponse {
                url: fetched.url,
                import,
            },
        ))
    })
    .await
}

/// Build the import receipt: the report carries only `source_id`, but a
/// web caller never saw the dat header, so resolve the identity too.
fn import_response(db: &Db, report: &ImportReport) -> Result<DatImportResponse, Response> {
    let (provider, system) = db
        .cache()
        .query_row(
            "SELECT provider, system FROM dat_source WHERE source_id = ?1",
            [report.source_id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .map_err(|e| err(ErrorCode::Internal, &e.to_string()))?;
    Ok(DatImportResponse {
        source_id: report.source_id,
        revision_id: report.revision_id,
        dat_blob: report.dat_blob.to_hex(),
        provider,
        system,
        entries: report.entries,
        claims: report.claims,
        demoted_revisions: report.demoted_revisions.clone(),
    })
}

/// A dat that won't parse — or a fetch that failed on caller-supplied
/// input (bad source, unreachable URL, non-datfile zip) — is the
/// caller's problem (400); store/index failures are ours (500).
/// import_dat validates before storing, so a refused body leaves no blob
/// behind.
fn import_err(e: CatalogError) -> Response {
    let code = match &e {
        CatalogError::Parse(_) | CatalogError::Fetch(_) => ErrorCode::BadRequest,
        _ => ErrorCode::Internal,
    };
    err(code, &e.to_string())
}

// ---- GET /v1/dats/{provider}/{system}/diff ----

/// Diff a source's two newest revisions (previous → current, D38/D96) —
/// the same output as `datboi dat diff`. A pure read: an empty diff is a
/// 200 with empty lists, never an error.
pub(crate) async fn diff(
    State(app): State<Arc<App>>,
    Extension(caller): Extension<Caller>,
    UrlPath((provider, system)): UrlPath<(String, String)>,
) -> Response {
    run_blocking(move || {
        require_owner(&caller)?;
        let db = app.readers.get();
        let d = diff_source(&db, &provider, &system).map_err(source_err)?;
        let name_change = |from: String, to: String| DatNameChange { from, to };
        Ok(json_response(
            StatusCode::OK,
            &DatDiffResponse {
                provider: d.provider,
                system: d.system,
                revision_old: d.revision_old,
                revision_new: d.revision_new,
                entries_old: d.entries_old,
                entries_new: d.entries_new,
                added: d.added,
                removed: d.removed,
                renamed: d
                    .renamed
                    .into_iter()
                    .map(|(from, to)| name_change(from, to))
                    .collect(),
                rehashed: d
                    .rehashed
                    .into_iter()
                    .map(|r| name_change(r.name_old, r.name_new))
                    .collect(),
            },
        ))
    })
    .await
}

// ---- GET /v1/dats/{provider}/{system}/export ----

/// Export a source's current revision as a Logiqx dat (dir2dat, D29/D96)
/// — the same bytes `datboi export dat` writes. A raw XML download, not
/// JSON.
pub(crate) async fn export(
    State(app): State<Arc<App>>,
    Extension(caller): Extension<Caller>,
    UrlPath((provider, system)): UrlPath<(String, String)>,
) -> Response {
    run_blocking(move || {
        require_owner(&caller)?;
        let db = app.readers.get();
        let bytes = export_dat(&db, &provider, &system, None).map_err(source_err)?;
        let filename = format!("{provider}-{system}.dat");
        Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "application/xml")
            .header(header::CACHE_CONTROL, "no-store")
            .header(
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{}\"", filename.replace('"', "")),
            )
            .body(Body::from(bytes))
            .map_err(|e| err(ErrorCode::Internal, &e.to_string()))
    })
    .await
}

// ---- POST /v1/dats/{provider}/{system}/clonelist ----

/// Link a retool clonelist to a source (D57/D96): the raw clonelist JSON
/// is the body (request-sized, like dat import). Refines 1G1R clone
/// families in both held-first and strict view modes.
pub(crate) async fn clonelist(
    State(app): State<Arc<App>>,
    Extension(caller): Extension<Caller>,
    UrlPath((provider, system)): UrlPath<(String, String)>,
    body: Bytes,
) -> Response {
    run_blocking(move || {
        require_owner(&caller)?;
        if body.is_empty() {
            return Err(err(ErrorCode::BadRequest, "empty request body"));
        }
        let db = app
            .db
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let report =
            import_clonelist(&db, app.store, &provider, &system, &body).map_err(source_err)?;
        tracing::info!(
            "clonelist: {provider}/{system} -> {} ({} terms, {} skipped)",
            report.hash,
            report.terms,
            report.skipped
        );
        Ok(json_response(
            StatusCode::OK,
            &ClonelistResponse {
                hash: report.hash.to_hex(),
                terms: report.terms as u64,
                skipped: report.skipped as u64,
            },
        ))
    })
    .await
}

/// Errors for the provider/system dat verbs: an unknown or
/// revision-less source is a 404; "only one revision" and clonelist/parse
/// problems are the caller's 400; store/index failures are ours (500).
fn source_err(e: CatalogError) -> Response {
    let code = match &e {
        CatalogError::UnknownSource { .. } | CatalogError::NoCurrentRevision { .. } => {
            ErrorCode::NotFound
        }
        CatalogError::NoPreviousRevision { .. }
        | CatalogError::Clonelist(_)
        | CatalogError::Parse(_) => ErrorCode::BadRequest,
        _ => ErrorCode::Internal,
    };
    err(code, &e.to_string())
}
