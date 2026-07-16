//! View authoring over HTTP (D96): define now, evaluation and image
//! minting to follow. The friend-facing view READS live in api.rs; this
//! module is the owner-only authoring surface, the daemon's caller of
//! the same `datboi_catalog` functions the CLI's `view` subcommands run
//! — one code path per verb, so the two surfaces cannot diverge.

// Same rationale as http.rs: fallible steps short-circuit with the
// error Response itself.
#![allow(clippy::result_large_err)]

use std::sync::Arc;

use axum::Extension;
use axum::extract::{Path as UrlPath, State};
use axum::http::StatusCode;
use axum::response::Response;
use datboi_api::{ErrorCode, ViewDefineRequest, ViewDefineResponse};
use datboi_catalog::{CatalogError, SelectionPolicy, ViewDef};

use crate::App;
use crate::api::{definition, err, require_owner};
use crate::auth::Caller;
use crate::http::{ApiJson, json_response, run_blocking};

// ---- PUT /v1/views/{name} ----

pub(crate) async fn define(
    State(app): State<Arc<App>>,
    Extension(caller): Extension<Caller>,
    UrlPath(name): UrlPath<String>,
    ApiJson(req): ApiJson<ViewDefineRequest>,
) -> Response {
    run_blocking(move || {
        require_owner(&caller)?;
        let def = view_def_from_request(name.clone(), req);
        let db = app
            .db
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // define_view owns the validation (unknown profile, 1G1R⊕MAME) —
        // the same checks the CLI hits, mapped here to the typed 400.
        datboi_catalog::define_view(&db, &def).map_err(define_err)?;
        tracing::info!("view define: {name} over {}/{}", def.provider, def.system);
        Ok(json_response(
            StatusCode::OK,
            &ViewDefineResponse {
                name,
                definition: definition(&def),
            },
        ))
    })
    .await
}

/// Reverse of `api::definition`: the wire request → the catalog input.
fn view_def_from_request(name: String, req: ViewDefineRequest) -> ViewDef {
    ViewDef {
        name,
        provider: req.provider,
        system: req.system,
        template: req.template,
        selection: req.one_g_one_r.map(|o| SelectionPolicy {
            strict: matches!(o.mode, datboi_api::OneGOneRMode::Strict),
            regions: o.regions,
            langs: o.langs,
        }),
        profile: req.profile,
        image: req.image.map(|i| datboi_catalog::ImageParams {
            cluster_size: i.cluster_size,
            partition: i.partition,
            label: i.label.into_inner(),
        }),
        mame: req.mame_mode.map(|m| match m {
            datboi_api::MameMode::NonMerged => datboi_catalog::MameMode::NonMerged,
            datboi_api::MameMode::Split => datboi_catalog::MameMode::Split,
            datboi_api::MameMode::Merged => datboi_catalog::MameMode::Merged,
        }),
    }
}

/// A malformed definition is the caller's problem (400); index I/O is
/// ours (500). `define_view` validates before persisting anything.
fn define_err(e: CatalogError) -> Response {
    let code = match &e {
        CatalogError::UnknownProfile(_) | CatalogError::Mame(_) => ErrorCode::BadRequest,
        _ => ErrorCode::Internal,
    };
    err(code, &e.to_string())
}
