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
    ErrorCode, GcApplyRequest, GcApplyResponse, GcConfig, GcConfigRequest, GcKeepRequest,
    OkResponse, OrphanItem, OrphansResponse,
};
use datboi_core::hash::Blake3;
use datboi_exec::policy::{self, Watermark};
use datboi_index::{Db, GuardHolder};
use datboi_store_fs::Namespace as StoreNs;

use crate::App;
use crate::api::{err, require_owner};
use crate::auth::{Caller, now_unix};
use crate::http::{ApiJson, json_response, run_blocking};
use crate::maintain::claim_guard;

fn internal(e: impl std::fmt::Display) -> Response {
    err(ErrorCode::Internal, &e.to_string())
}

// ---- GET /v1/gc/orphans ----

pub(crate) async fn orphans(
    State(app): State<Arc<App>>,
    Extension(caller): Extension<Caller>,
) -> Response {
    run_blocking(move || {
        require_owner(&caller)?;
        // Pure read (D93): the orphan LIST reads grace + keep-marks +
        // candidates and mutates nothing, so it belongs on the read-only
        // pool, not the pipeline writer it used to borrow.
        let db = app.readers.get();
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
            .map_err(|_| err(ErrorCode::BadRequest, "not a blake3 hex hash"))?;
        let keep = req.keep;
        // Stays on the PIPELINE writer, not the quick-write pool (D93):
        // `apply` reads the keep-set ONCE and then loops deleting, and
        // its delete-time re-verification checks unreferenced+aged, NOT
        // keep-marks — so a keep must not interleave with an in-flight
        // apply. The process mutex is the named argument that serializes
        // keep against apply; the guard only serializes apply against
        // other gc actors.
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
            .map_err(|e| err(ErrorCode::Internal, &format!("entropy: {e}")))?;
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
                ErrorCode::Busy,
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
        // Packed orphans (D91) have no loose file to unlink — remove_blob
        // would no-op and leave the bytes stranded in the pack while the
        // row vanished (index/store divergence). Defer them, grouped by
        // pack, to one tombstone-and-repack each after the loose sweep.
        let mut packed: std::collections::HashMap<Blake3, Vec<(i64, Blake3, u64)>> =
            std::collections::HashMap::new();
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
            if let Some(pack) = app.store.pack_of(&candidate.hash) {
                packed.entry(pack).or_default().push((
                    candidate.blob_id,
                    candidate.hash,
                    candidate.size.unwrap_or(0),
                ));
                continue;
            }
            // Loose (or already-gone): bytes first, rows second.
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
        // Tombstone-and-repack the affected packs (D91): each is rewritten
        // once without its orphaned members, then their obao sidecars and
        // rows go. Same bytes-first-rows-second direction as the loose path.
        if failure.is_none() {
            for (pack, orphans) in packed {
                let drop: std::collections::HashSet<Blake3> =
                    orphans.iter().map(|(_, hash, _)| *hash).collect();
                if let Err(e) = app.store.repack(&pack, &drop) {
                    failure = Some(internal(e));
                    break;
                }
                for (blob_id, hash, size) in &orphans {
                    // Clear any obao sidecar / double-resident loose copy.
                    let _ = app.store.remove_blob(StoreNs::Data, hash);
                    if let Err(e) = db.delete_orphan_rows(*blob_id) {
                        failure = Some(internal(e));
                        break;
                    }
                    response.deleted += 1;
                    response.bytes_reclaimed += size;
                }
                if failure.is_some() {
                    break;
                }
            }
        }
        db.release_gc_guard(&holder).unwrap_or(()); // TTL is the backstop
        if let Some(resp) = failure {
            return Err(resp);
        }

        // The record in the tray: a finished gc job, like every other
        // byte-level event.
        let job = app
            .jobs
            .create_gc("gc — orphan apply", response.deleted, now_unix());
        app.jobs
            .refine_progress(job, response.deleted, response.deleted);
        app.jobs.push_note(
            job,
            format!(
                "{} deleted, {} byte(s) reclaimed, {} skipped by delete-time re-verification",
                response.deleted, response.bytes_reclaimed, response.skipped
            ),
        );
        app.jobs.finish(job, now_unix());
        tracing::info!(
            "gc apply: {} deleted, {} byte(s), {} skipped",
            response.deleted,
            response.bytes_reclaimed,
            response.skipped
        );
        Ok(json_response(StatusCode::OK, &response))
    })
    .await
}

// ---- GET /v1/gc/config ----

pub(crate) async fn config_get(
    State(app): State<Arc<App>>,
    Extension(caller): Extension<Caller>,
) -> Response {
    run_blocking(move || {
        require_owner(&caller)?;
        let db = app.readers.get();
        Ok(json_response(StatusCode::OK, &read_config(&db)?))
    })
    .await
}

// ---- PUT /v1/gc/config ----

pub(crate) async fn config_set(
    State(app): State<Arc<App>>,
    Extension(caller): Extension<Caller>,
    ApiJson(req): ApiJson<GcConfigRequest>,
) -> Response {
    run_blocking(move || {
        require_owner(&caller)?;
        // Validate every provided field BEFORE any write, so a bad value
        // leaves the whole policy untouched (all-or-nothing). Watermarks
        // parse through the shared policy parser (D96).
        let high = parse_watermark(req.high_water.as_deref(), "high_water")?;
        let low = parse_watermark(req.low_water.as_deref(), "low_water")?;
        if req.grace_secs.is_some_and(|g| g < 0) {
            return Err(err(ErrorCode::BadRequest, "grace_secs must be non-negative"));
        }
        let db = app
            .db
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(wm) = high {
            policy::set_high_water(&db, wm).map_err(internal)?;
        }
        if let Some(wm) = low {
            policy::set_low_water(&db, wm).map_err(internal)?;
        }
        if let Some(grace) = req.grace_secs {
            policy::set_grace_secs(&db, grace).map_err(internal)?;
        }
        Ok(json_response(StatusCode::OK, &read_config(&db)?))
    })
    .await
}

/// Parse an optional watermark string, mapping a malformed value to the
/// typed 400 (the `field` names which one for the message).
fn parse_watermark(value: Option<&str>, field: &str) -> Result<Option<Watermark>, Response> {
    match value {
        Some(text) => Watermark::parse_str(text).map(Some).ok_or_else(|| {
            err(
                ErrorCode::BadRequest,
                &format!("{field}: expected \"off\", \"NN%\", or absolute bytes"),
            )
        }),
        None => Ok(None),
    }
}

/// The current policy in canonical wire form (watermarks as strings).
fn read_config(db: &Db) -> Result<GcConfig, Response> {
    Ok(GcConfig {
        high_water: policy::high_water(db).map_err(internal)?.to_string(),
        low_water: policy::low_water(db).map_err(internal)?.to_string(),
        grace_secs: policy::grace_secs(db).map_err(internal)?,
    })
}
