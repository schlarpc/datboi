//! Analyzer configuration over HTTP (D60/D96): list the shipped families
//! and set each one's enable state + opaque params. The daemon's caller
//! of the same `datboi_ingest::refine` functions the CLI's `analyzer`
//! subcommand runs, over the same `FAMILIES` list — one source, so the
//! two surfaces offer exactly the same families and semantics.

// Same rationale as http.rs: fallible steps short-circuit with the
// error Response itself.
#![allow(clippy::result_large_err)]

use std::sync::Arc;

use axum::Extension;
use axum::extract::{Path as UrlPath, State};
use axum::http::StatusCode;
use axum::response::Response;
use datboi_api::{AnalyzerConfigRequest, AnalyzerInfo, AnalyzersResponse, ErrorCode, Nullable};
use datboi_ingest::refine;
use tracing::info;

use crate::App;
use crate::api::{err, internal, require_owner};
use crate::auth::Caller;
use crate::http::{ApiJson, json_response, run_blocking};

// ---- GET /v1/analyzers ----

pub(crate) async fn list(
    State(app): State<Arc<App>>,
    Extension(caller): Extension<Caller>,
) -> Response {
    run_blocking(move || {
        require_owner(&caller)?;
        let db = app.readers.get();
        let mut analyzers = Vec::with_capacity(refine::FAMILIES.len());
        for &family in refine::FAMILIES {
            analyzers.push(read_info(&db, family)?);
        }
        Ok(json_response(
            StatusCode::OK,
            &AnalyzersResponse { analyzers },
        ))
    })
    .await
}

// ---- PUT /v1/analyzers/{family} ----

pub(crate) async fn config(
    State(app): State<Arc<App>>,
    Extension(caller): Extension<Caller>,
    UrlPath(family): UrlPath<String>,
    ApiJson(req): ApiJson<AnalyzerConfigRequest>,
) -> Response {
    run_blocking(move || {
        require_owner(&caller)?;
        // The family must be one we ship — an unknown name is the caller's
        // typo, not a silent no-op (mirrors the CLI's require_family).
        let family = refine::FAMILIES
            .iter()
            .find(|f| ***f == family)
            .copied()
            .ok_or_else(|| {
                err(
                    ErrorCode::BadRequest,
                    &format!(
                        "unknown analyzer family (available: {})",
                        refine::FAMILIES.join(", ")
                    ),
                )
            })?;
        let params = match req.params_hex.as_deref() {
            Some(hex) => Some(decode_hex(hex).ok_or_else(|| {
                err(
                    ErrorCode::BadRequest,
                    "params_hex must be an even-length hex string",
                )
            })?),
            None => None,
        };
        let db = app
            .db
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        refine::set_analyzer_enabled(&db, family, req.enabled).map_err(internal)?;
        refine::set_analyzer_params(&db, family, params.as_deref()).map_err(internal)?;
        info!(
            "analyzer config: {family} {} params {}",
            if req.enabled { "enabled" } else { "disabled" },
            params
                .as_ref()
                .map_or_else(|| "cleared".into(), |p| format!("set ({} bytes)", p.len()))
        );
        Ok(json_response(StatusCode::OK, &read_info(&db, family)?))
    })
    .await
}

/// One family's current config, hex-encoding the opaque params.
fn read_info(db: &datboi_index::Db, family: &str) -> Result<AnalyzerInfo, Response> {
    let enabled = refine::analyzer_enabled(db, family).map_err(internal)?;
    let params = refine::analyzer_params(db, family).map_err(internal)?;
    Ok(AnalyzerInfo {
        family: family.to_owned(),
        enabled,
        params_hex: Nullable(params.map(|p| encode_hex(&p))),
    })
}

fn encode_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
}

/// Even-length ASCII hex → bytes; `None` on any malformation.
fn decode_hex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}
