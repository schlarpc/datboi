//! The D73 review/apply surface: orphans list, keep-marks, and the one
//! human-triggered destructive action in the system.
//!
//! Apply discipline: every requested deletion re-verifies
//! unreferenced + aged + unkept AT DELETE TIME under the D72 singleton
//! guard — the mark alone never justifies a drop, and refusals are
//! `skipped` counts, never errors (the reviewer sees exactly what
//! declined and why the next list looks different). Store unlink
//! precedes row removal (bytes-as-truth: a crash in between is the
//! direction recovery reconciles).

#![allow(clippy::result_large_err)]

use std::sync::Arc;

use axum::Extension;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Response;
use datboi_api::{
    GcApplyRequest, GcApplyResponse, GcKeepRequest, OkResponse, OrphanItem, OrphansResponse,
};
use datboi_core::hash::Blake3;
use datboi_exec::policy;
use datboi_index::GuardHolder;
use datboi_store_fs::Namespace as StoreNs;

use crate::App;
use crate::api::{err, require_owner};
use crate::auth::{Caller, now_unix};
use crate::http::{json_response, run_blocking};
use crate::maintain::claim_guard;

fn internal(e: impl std::fmt::Display) -> Response {
    err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string())
}

// ---- GET /v1/gc/orphans ----

pub(crate) async fn orphans(
    State(app): State<Arc<App>>,
    Extension(caller): Extension<Caller>,
) -> Response {
    run_blocking(move || {
        require_owner(&caller)?;
        let db = app
            .db
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let grace = policy::grace_secs(&db).map_err(internal)?;
        let keeps = policy::keep_set(&db).map_err(internal)?;
        let candidates = db
            .list_orphan_candidates(now_unix(), grace)
            .map_err(internal)?;
        let orphans: Vec<OrphanItem> = candidates
            .into_iter()
            .map(|c| OrphanItem {
                kept: keeps.contains(&c.hash),
                hash: c.hash.to_hex(),
                size: c.size.unwrap_or(0),
                marked_at: c.marked_at,
                sources: c.sources,
            })
            .collect();
        let reclaimable_bytes = orphans.iter().filter(|o| !o.kept).map(|o| o.size).sum();
        Ok(json_response(
            StatusCode::OK,
            &OrphansResponse {
                orphans,
                reclaimable_bytes,
                grace_secs: grace,
            },
        ))
    })
    .await
}

// ---- POST /v1/gc/keep ----

pub(crate) async fn keep(
    State(app): State<Arc<App>>,
    Extension(caller): Extension<Caller>,
    crate::http::ApiJson(req): crate::http::ApiJson<GcKeepRequest>,
) -> Response {
    run_blocking(move || {
        require_owner(&caller)?;
        let hash: Blake3 = req
            .hash
            .parse()
            .map_err(|_| err(StatusCode::BAD_REQUEST, "not a blake3 hex hash"))?;
        let keep = req.keep;
        let db = app
            .db
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        policy::set_keep(&db, &hash, keep).map_err(internal)?;
        Ok(json_response(StatusCode::OK, &OkResponse { ok: true }))
    })
    .await
}

// ---- POST /v1/gc/orphans/apply ----

pub(crate) async fn apply(
    State(app): State<Arc<App>>,
    Extension(caller): Extension<Caller>,
    crate::http::ApiJson(req): crate::http::ApiJson<GcApplyRequest>,
) -> Response {
    run_blocking(move || {
        require_owner(&caller)?;
        let mut holder = [0u8; 16];
        getrandom::getrandom(&mut holder)
            .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, &format!("entropy: {e}")))?;
        let holder = GuardHolder(holder);

        let mut db = app
            .db
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let grace = policy::grace_secs(&db).map_err(internal)?;
        let keeps = policy::keep_set(&db).map_err(internal)?;
        let roots = app.exec.orphan_extra_roots(&db).map_err(internal)?;
        let now = now_unix();
        let mut wanted = db.list_orphan_candidates(now, grace).map_err(internal)?;
        if let Some(hashes) = &req.hashes {
            let requested: std::collections::HashSet<&str> =
                hashes.iter().map(String::as_str).collect();
            wanted.retain(|c| requested.contains(c.hash.to_hex().as_str()));
        }

        if !claim_guard(&db, &holder) {
            return Err(err(
                StatusCode::SERVICE_UNAVAILABLE,
                "gc guard busy (an eviction or apply is running); retry shortly",
            ));
        }
        // From here every exit releases the guard.
        let mut response = GcApplyResponse {
            deleted: 0,
            bytes_reclaimed: 0,
            skipped: 0,
        };
        let mut failure: Option<Response> = None;
        for candidate in wanted {
            if keeps.contains(&candidate.hash) {
                response.skipped += 1;
                continue;
            }
            // Delete-time re-verification (D73): the review-era mark
            // proves nothing about NOW.
            match db.orphan_still_deletable(candidate.blob_id, &roots, now, grace) {
                Ok(true) => {}
                Ok(false) => {
                    response.skipped += 1;
                    continue;
                }
                Err(e) => {
                    failure = Some(internal(e));
                    break;
                }
            }
            // Bytes first, rows second (recovery's direction).
            if let Err(e) = app.store.remove_blob(StoreNs::Data, &candidate.hash) {
                failure = Some(internal(e));
                break;
            }
            if let Err(e) = db.delete_orphan_rows(candidate.blob_id) {
                failure = Some(internal(e));
                break;
            }
            response.deleted += 1;
            response.bytes_reclaimed += candidate.size.unwrap_or(0);
        }
        db.release_gc_guard(&holder).unwrap_or(()); // TTL is the backstop
        if let Some(resp) = failure {
            return Err(resp);
        }

        // The record in the tray: a finished gc job, like every other
        // byte-level event.
        let job = app.jobs.create_gc("gc — orphan apply", response.deleted, now_unix());
        app.jobs.refine_progress(job, response.deleted, response.deleted);
        app.jobs.push_note(
            job,
            format!(
                "{} deleted, {} byte(s) reclaimed, {} skipped by delete-time re-verification",
                response.deleted, response.bytes_reclaimed, response.skipped
            ),
        );
        app.jobs.finish(job, now_unix());
        eprintln!(
            "gc apply: {} deleted, {} byte(s), {} skipped",
            response.deleted, response.bytes_reclaimed, response.skipped
        );
        Ok(json_response(StatusCode::OK, &response))
    })
    .await
}
