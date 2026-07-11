//! ROM ingest over HTTP: staged streaming uploads + a background job
//! running the same pipeline as `datboi ingest`.
//!
//! Ingest was the M5 ruling's archetypal CLI-only action — it wants
//! progress and a job registry. The registry now exists (jobs.rs, the
//! in-memory surface open-questions sanctioned), and uploads split the
//! problem in two: each file streams to the store's staging area in
//! its own request (client-side progress is the browser's upload
//! meter), then one job ingests the staged set file-by-file. Nothing
//! is ever whole in memory — files run to GBs.
//!
//! Custody over HTTP is always copy (D40's default): the browser
//! cannot move the caller's originals, only send copies.

// Same rationale as http.rs: fallible steps short-circuit with the
// error Response itself.
#![allow(clippy::result_large_err)]

use std::collections::HashSet;
use std::io::Write as _;
use std::sync::Arc;

use axum::Extension;
use axum::body::{Body, Bytes};
use axum::extract::{RawQuery, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::Response;
use datboi_api::{IngestRequest, IngestStartResponse, MatchedEntry, UploadResponse};
use datboi_index::Db;
use datboi_ingest::Ingester;
use futures_core::Stream as _;

use crate::App;
use crate::api::{err, parse_query, require_owner};
use crate::auth::{self, Caller};
use crate::http::{json_response, run_blocking};
use crate::jobs::{StagedUpload, translate};

/// D56 analog for uploads: refuse a declared Content-Length that would
/// land the store filesystem within this margin of full (the value
/// datboi-exec's materialize guard uses). Chunked bodies skip the
/// guard; ENOSPC at write time is the backstop.
const STAGING_SLACK: u64 = 64 << 20;

// ---- POST /v1/ingest/uploads ----

pub(crate) async fn upload(
    State(app): State<Arc<App>>,
    Extension(caller): Extension<Caller>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    body: Body,
) -> Response {
    // Reject before reading the body: an unauthorized caller should
    // not get to fill the disk first.
    if let Err(resp) = require_owner(&caller) {
        return resp;
    }
    let name = match upload_name(query.as_deref()) {
        Ok(name) => name,
        Err(resp) => return resp,
    };

    // Headroom guard (D56 analog) — only when the client declared a
    // length; a chunked body falls through to the ENOSPC backstop.
    let declared = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    if let Some(len) = declared {
        match app.store.available_bytes() {
            Ok(Some(avail)) if avail < len.saturating_add(STAGING_SLACK) => {
                return err(
                    StatusCode::INSUFFICIENT_STORAGE,
                    &format!("insufficient store headroom: need ~{len} bytes, {avail} available"),
                );
            }
            // Unanswerable platforms stay permissive (store.rs says why).
            Ok(_) => {}
            Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
        }
    }

    let hint = name.rsplit('/').next().unwrap_or(&name);
    let path = app.store.staging_path(hint);

    // The house streaming pattern in reverse (http.rs RecvStream):
    // bounded channel into a blocking writer — a slow store write
    // backpressures the socket read instead of ballooning memory.
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Bytes>(4);
    let writer = {
        let path = path.clone();
        tokio::task::spawn_blocking(move || -> std::io::Result<u64> {
            // No fsync: staging is disposable by design. A crash
            // orphan is swept (cleanup_temp); the DURABLE publish is
            // put_new's temp → fsync → rename during ingest.
            let mut file = std::fs::File::create_new(&path)?;
            let mut written = 0u64;
            while let Some(chunk) = rx.blocking_recv() {
                file.write_all(&chunk)?;
                written += chunk.len() as u64;
            }
            Ok(written)
        })
    };

    let mut stream = body.into_data_stream();
    let mut stream_error = false;
    loop {
        let next = std::future::poll_fn(|cx| std::pin::Pin::new(&mut stream).poll_next(cx)).await;
        match next {
            Some(Ok(chunk)) => {
                if tx.send(chunk).await.is_err() {
                    break; // writer died; its Err(io) reports below
                }
            }
            Some(Err(_)) => {
                // Client went away / transport error mid-body.
                stream_error = true;
                break;
            }
            None => break,
        }
    }
    drop(tx); // writer's recv loop ends
    let written = match writer.await {
        Ok(Ok(written)) => written,
        Ok(Err(io)) => {
            let _ = std::fs::remove_file(&path);
            return err(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("staging write: {io}"),
            );
        }
        Err(join) => {
            let _ = std::fs::remove_file(&path);
            return err(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("staging task failed: {join}"),
            );
        }
    };
    // A truncated body must not become a silently-short ROM.
    if stream_error || declared.is_some_and(|len| len != written) {
        let _ = std::fs::remove_file(&path);
        return err(StatusCode::BAD_REQUEST, "upload aborted or short body");
    }
    if written == 0 {
        let _ = std::fs::remove_file(&path);
        return err(StatusCode::BAD_REQUEST, "empty upload");
    }

    let token = match auth::mint_token() {
        Ok(token) => token,
        Err(e) => {
            let _ = std::fs::remove_file(&path);
            return err(StatusCode::INTERNAL_SERVER_ERROR, &format!("entropy: {e}"));
        }
    };
    app.jobs.stage(
        token.clone(),
        StagedUpload {
            path,
            name,
            bytes: written,
        },
    );
    json_response(
        StatusCode::OK,
        &UploadResponse {
            upload: token,
            bytes: written,
        },
    )
}

/// The client-relative name from `?name=`: display identity for the
/// report, so it must be a sane relative path — not a traversal.
fn upload_name(query: Option<&str>) -> Result<String, Response> {
    let name = parse_query(query.unwrap_or(""))
        .into_iter()
        .find(|(k, _)| k == "name")
        .map(|(_, v)| v)
        .unwrap_or_default();
    if name.is_empty() {
        return Err(err(StatusCode::BAD_REQUEST, "missing query param \"name\""));
    }
    if name.starts_with('/')
        || name.contains('\0')
        || name.split('/').any(|seg| seg.is_empty() || seg == "..")
    {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "name must be a relative path without \"..\" segments",
        ));
    }
    Ok(name)
}

// ---- POST /v1/ingest ----

pub(crate) async fn start(
    State(app): State<Arc<App>>,
    Extension(caller): Extension<Caller>,
    axum::Json(req): axum::Json<IngestRequest>,
) -> Response {
    run_blocking(move || {
        require_owner(&caller)?;
        let tokens = req
            .uploads
            .ok_or_else(|| err(StatusCode::BAD_REQUEST, "missing field \"uploads\""))?;
        if tokens.is_empty() {
            return Err(err(StatusCode::BAD_REQUEST, "uploads must not be empty"));
        }
        let staged = app.jobs.take(&tokens).map_err(|t| {
            err(
                StatusCode::BAD_REQUEST,
                &format!("unknown or expired upload: {t}"),
            )
        })?;
        let id = app.jobs.create(&staged, auth::now_unix());
        // A plain thread, not the blocking pool: this is daemon-lifetime
        // background work that may run for an hour — don't pin a
        // request-serving spawn_blocking slot to it.
        let app = Arc::clone(&app);
        std::thread::spawn(move || run_job(&app, id, staged));
        Ok(json_response(
            StatusCode::OK,
            &IngestStartResponse { job: id },
        ))
    })
    .await
}

/// The job body: one Ingester run PER FILE, so the db lock releases
/// between files (a multi-GB extraction blocks reads for that file
/// only, not the whole batch) and progress moves at file boundaries —
/// the honest granularity, since the pipeline reports nothing finer.
fn run_job(app: &App, id: i64, staged: Vec<StagedUpload>) {
    // Baseline for the "matched" report: whatever was satisfied before
    // this job ran isn't news, however the content arrived.
    let before = {
        let db = app
            .db
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match satisfied_entries(&db) {
            Ok(set) => set,
            Err(e) => {
                // Dying before the loop still spends the staged copies.
                for upload in &staged {
                    let _ = std::fs::remove_file(&upload.path);
                }
                app.jobs
                    .fail(id, &format!("matched baseline: {e}"), auth::now_unix());
                return;
            }
        }
    };
    for upload in staged {
        app.jobs.set_current(id, &upload.name);
        let report = {
            let mut db = app
                .db
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            Ingester::new(app.store, &mut db, &app.detectors).ingest(&[&upload.path])
        };
        // Win or lose, the staged copy is spent (a failure is recorded
        // in the report; the bytes that mattered are in the store).
        let _ = std::fs::remove_file(&upload.path);
        app.jobs
            .file_done(id, upload.bytes, translate(report, &upload));
    }
    // The pipeline stores content; identity linking + the D39 rollup
    // refresh are what make the shelf light up. The job owns finishing
    // that thought (dat import and view eval already run the same pair).
    {
        let mut db = app
            .db
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // The matched diff is part of the same thought: trivial reads
        // against the rollups just refreshed, so a failure there is the
        // same infrastructure failure as the refresh itself.
        let refreshed = datboi_catalog::relink_all(&db)
            .and_then(|()| datboi_catalog::refresh_rollups(&mut db, auth::now_unix()))
            .and_then(|()| newly_matched(&db, &before).map_err(Into::into));
        match refreshed {
            Ok((matched, total)) => app.jobs.set_matched(id, matched, total),
            Err(e) => {
                app.jobs
                    .fail(id, &format!("catalog refresh: {e}"), auth::now_unix());
                return;
            }
        }
    }
    app.jobs.finish(id, auth::now_unix());
}

/// The report caps the named matches here; `matched_total` carries the
/// uncapped count so a bulk lightup is never silently truncated.
const MATCHED_CAP: usize = 200;

/// The satisfied-entry set: every required claim covered at verified or
/// claimed grade — the D39 rollup states api.rs's `STATE_CASE` renders
/// as `verified`|`claimed` (nodump rows, required = 0, don't count).
fn satisfied_entries(db: &Db) -> rusqlite::Result<HashSet<i64>> {
    db.cache()
        .prepare(
            "SELECT entry_id FROM entry_audit
             WHERE required > 0 AND have_verified + have_claimed >= required",
        )?
        .query_map([], |row| row.get(0))?
        .collect()
}

/// Diff the satisfied set against the job-start baseline and name the
/// newly satisfied entries: `(capped rows, uncapped total)`.
fn newly_matched(db: &Db, before: &HashSet<i64>) -> rusqlite::Result<(Vec<MatchedEntry>, u64)> {
    let mut new_ids: Vec<i64> = satisfied_entries(db)?
        .into_iter()
        .filter(|id| !before.contains(id))
        .collect();
    let total = new_ids.len() as u64;
    // Deterministic cap survivors: entry_id order (dat import order);
    // display order comes from the query below.
    new_ids.sort_unstable();
    new_ids.truncate(MATCHED_CAP);
    if new_ids.is_empty() {
        return Ok((Vec::new(), total));
    }
    let placeholders = vec!["?"; new_ids.len()].join(",");
    let matched = db
        .cache()
        .prepare(&format!(
            "SELECT e.name, s.provider || '/' || s.system
             FROM entry e
             JOIN dat_revision r ON r.revision_id = e.revision_id
             JOIN dat_source s ON s.source_id = r.source_id
             WHERE e.entry_id IN ({placeholders})
             ORDER BY s.provider, s.system, e.name"
        ))?
        .query_map(rusqlite::params_from_iter(new_ids), |row| {
            Ok(MatchedEntry {
                name: row.get(0)?,
                source: row.get(1)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok((matched, total))
}
