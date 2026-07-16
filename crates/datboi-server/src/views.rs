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
use datboi_api::{ErrorCode, JobStartResponse, ViewDefineRequest, ViewDefineResponse};
use datboi_catalog::{CatalogError, SelectionPolicy, ViewDef};
use tracing::{info, warn};

use crate::App;
use crate::api::{definition, err, internal, require_owner};
use crate::auth::{self, Caller};
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

// ---- POST /v1/views/{name}/eval ----

pub(crate) async fn eval(
    State(app): State<Arc<App>>,
    Extension(caller): Extension<Caller>,
    UrlPath(name): UrlPath<String>,
) -> Response {
    run_blocking(move || {
        require_owner(&caller)?;
        // Resolve the definition up front so an undefined view answers a
        // clean 404 instead of spawning a job that fails immediately.
        let def = {
            let db = app
                .db
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            datboi_catalog::get_view(&db, &name)
                .map_err(internal)?
                .ok_or_else(|| err(ErrorCode::NotFound, "no such view"))?
        };
        let id = app.jobs.create_eval(&name, auth::now_unix());
        // A daemon-lifetime thread, not a request-serving spawn_blocking
        // slot: a MAME-scale eval can run for minutes (ingest.rs rationale).
        let app = Arc::clone(&app);
        std::thread::spawn(move || run_eval_job(&app, id, def));
        Ok(json_response(StatusCode::OK, &JobStartResponse { job: id }))
    })
    .await
}

/// The eval job body: one `evaluate_view` under the write lock (it
/// relinks, refreshes rollups, and publishes the snapshot). The same
/// call the CLI's `view eval` runs — one code path (D96).
fn run_eval_job(app: &App, id: i64, def: ViewDef) {
    info!("eval job {id}: view {} ({}/{})", def.name, def.provider, def.system);
    let result = {
        let mut db = app
            .db
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        datboi_catalog::evaluate_view(&mut db, app.store, &def, auth::now_unix())
    };
    match result {
        Ok(report) => {
            app.jobs.push_note(
                id,
                format!(
                    "snapshot {} — {} row(s), {} claim(s) missing, {} path(s) disambiguated",
                    report.snapshot, report.rows, report.missing, report.disambiguated
                ),
            );
            info!(
                "eval job {id}: snapshot {} ({} rows)",
                report.snapshot, report.rows
            );
            app.jobs.finish(id, auth::now_unix());
        }
        Err(e) => {
            warn!("eval job {id}: FAILED — {e}");
            app.jobs.fail(id, &e.to_string(), auth::now_unix());
        }
    }
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
