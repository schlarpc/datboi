//! Dat import over HTTP (docs/60-dats.md) — the first mutating action
//! to graduate from the M5 CLI-only ruling, because it is
//! request-sized: bytes in, report out, and the CLI path buffers the
//! whole file exactly the same way (`std::fs::read` in cmds.rs).
//! Ingest graduated after it with the opposite shape (streamed
//! staging + a background job — see ingest.rs); eval, mint, evict,
//! and scrub still wait on their CLI processes.

// Same rationale as http.rs: fallible steps short-circuit with the
// error Response itself.
#![allow(clippy::result_large_err)]

use std::sync::Arc;

use axum::Extension;
use axum::body::Bytes;
use axum::extract::{RawQuery, State};
use axum::http::StatusCode;
use axum::response::Response;
use datboi_api::DatImportResponse;
use datboi_catalog::{CatalogError, ImportOptions, import_dat};

use crate::App;
use crate::api::{err, parse_query, require_owner};
use crate::auth::{self, Caller};
use crate::http::{json_response, run_blocking};

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
            return Err(err(StatusCode::BAD_REQUEST, "empty request body"));
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
        // The report carries only source_id; the caller never saw the
        // dat header, so answer the resolved identity too.
        let (provider, system) = db
            .cache()
            .query_row(
                "SELECT provider, system FROM dat_source WHERE source_id = ?1",
                [report.source_id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))?;
        // Operator log line (stderr is the log until the durable job
        // table — see ingest.rs run_job): a dat import rewrites what
        // the catalog wants, and the response body vanishes with the
        // request.
        eprintln!(
            "dat import: {provider}/{system} rev {} ({} entries)",
            report.revision_id, report.entries
        );
        Ok(json_response(
            StatusCode::OK,
            &DatImportResponse {
                source_id: report.source_id,
                revision_id: report.revision_id,
                dat_blob: report.dat_blob.to_hex(),
                provider,
                system,
                entries: report.entries,
                claims: report.claims,
                demoted_revisions: report.demoted_revisions,
            },
        ))
    })
    .await
}

/// A dat that won't parse is the caller's problem (400); store/index
/// failures are ours (500). import_dat validates before storing, so a
/// refused body leaves no blob behind.
fn import_err(e: CatalogError) -> Response {
    let status = match &e {
        CatalogError::Parse(_) => StatusCode::BAD_REQUEST,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };
    err(status, &e.to_string())
}
