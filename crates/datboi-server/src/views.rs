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
use datboi_catalog::{CatalogError, ImageParams, SelectionPolicy, ViewDef};
use datboi_core::hash::Blake3;
use datboi_core::viewsnap::ViewSnapshot;
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

// ---- POST /v1/views/{name}/image ----

pub(crate) async fn mint(
    State(app): State<Arc<App>>,
    Extension(caller): Extension<Caller>,
    UrlPath(name): UrlPath<String>,
) -> Response {
    run_blocking(move || {
        require_owner(&caller)?;
        // Image params ride the definition (D62); the current snapshot is
        // the mint input, so a never-evaluated view is a precondition
        // failure, not a 404. Resolve both before spawning.
        let (params, snap_hash) = {
            let db = app
                .db
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let def = datboi_catalog::get_view(&db, &name)
                .map_err(internal)?
                .ok_or_else(|| err(ErrorCode::NotFound, "no such view"))?;
            let snapshot = db
                .get_tag(&format!("view/{name}"))
                .map_err(internal)?
                .ok_or_else(|| {
                    err(
                        ErrorCode::BadRequest,
                        "view has no snapshot yet — evaluate it first",
                    )
                })?;
            (def.image.unwrap_or_default(), snapshot)
        };
        let snap = load_snapshot(&app, snap_hash)?;
        let id = app.jobs.create_mint(&name, auth::now_unix());
        let app = Arc::clone(&app);
        std::thread::spawn(move || run_mint_job(&app, id, name, snap_hash, snap, params));
        Ok(json_response(StatusCode::OK, &JobStartResponse { job: id }))
    })
    .await
}

/// Read a snapshot object from the meta namespace and decode it — the
/// raw `ViewSnapshot` mint needs (the vfs cache holds a `ViewIndex`
/// instead, and mint is rare enough not to warrant its own cache).
fn load_snapshot(app: &App, hash: Blake3) -> Result<ViewSnapshot, Response> {
    use std::io::Read as _;
    let mut file = app
        .store
        .get(datboi_store_fs::Namespace::Meta, &hash)
        .map_err(internal)?
        .ok_or_else(|| err(ErrorCode::Internal, "snapshot object missing from store"))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).map_err(internal)?;
    ViewSnapshot::decode(&bytes).map_err(internal)
}

/// The mint job body: materialize the snapshot's absent inputs, then mint
/// the FAT32 image (D62) — the same `missing_inputs` → `materialize` →
/// `mint_image` sequence the CLI's `view image` runs. Obao is always
/// stored (the download surface serves ranges from it); no file export
/// (the image downloads via `GET /v1/views/{name}/image`).
fn run_mint_job(app: &App, id: i64, name: String, snap_hash: Blake3, snap: ViewSnapshot, params: ImageParams) {
    info!("mint job {id}: view {name} (snapshot {snap_hash})");
    let now = auth::now_unix();
    let result: Result<(datboi_catalog::ImageReport, usize), String> = (|| {
        let mut db = app
            .db
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let missing = datboi_catalog::missing_inputs(&db, &snap).map_err(|e| e.to_string())?;
        let materialized = missing.len();
        for hash in &missing {
            app.exec
                .materialize(&db, hash)
                .map_err(|e| format!("materializing image input {hash}: {e}"))?;
        }
        let report =
            datboi_catalog::mint_image(&mut db, app.store, &name, &snap_hash, &snap, &params, true, now)
                .map_err(|e| e.to_string())?;
        Ok((report, materialized))
    })();
    match result {
        Ok((report, materialized)) => {
            let obao = if report.obao_stored { ", obao stored" } else { "" };
            let extra = if materialized > 0 {
                format!(" (materialized {materialized} input(s) first)")
            } else {
                String::new()
            };
            app.jobs.push_note(
                id,
                format!(
                    "image {} — {} B, {} row(s), skeleton {} B{obao}{extra}",
                    report.image, report.size, report.rows, report.skeleton_bytes
                ),
            );
            info!("mint job {id}: image {} ({} B)", report.image, report.size);
            app.jobs.finish(id, auth::now_unix());
        }
        Err(e) => {
            warn!("mint job {id}: FAILED — {e}");
            app.jobs.fail(id, &e, auth::now_unix());
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
