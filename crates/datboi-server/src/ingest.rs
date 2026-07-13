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
//!
//! The job is also the unified drop surface: each staged file is
//! classified BY CONTENT — a dat (loose, or the sole member of a zip,
//! the shape No-Intro/Redump ship) imports via `import_dat` into the
//! report's `dats_imported` lane; everything else runs the pipeline.
//! Names never decide (the house detection philosophy).

// Same rationale as http.rs: fallible steps short-circuit with the
// error Response itself.
#![allow(clippy::result_large_err)]

use std::collections::HashSet;
use std::fs::File;
use std::io::{Read as _, Write as _};
use std::path::Path;
use std::sync::Arc;

use axum::Extension;
use axum::body::{Body, Bytes};
use axum::extract::{RawQuery, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::Response;
use datboi_api::{
    DatImportedItem, ErrorCode, IngestErrorItem, IngestReportBody, IngestRequest,
    IngestStartResponse, MatchedEntry, UploadResponse,
};
use datboi_catalog::{ImportOptions, import_dat};
use datboi_index::Db;
use datboi_ingest::{Ingester, zip};
use futures_core::Stream as _;
use tracing::{info, warn};

use crate::App;
use crate::api::{err, parse_query, require_owner};
use crate::auth::{self, Caller};
use crate::dats::BODY_LIMIT as DAT_LIMIT;
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
                    ErrorCode::StoreFull,
                    &format!("insufficient store headroom: need ~{len} bytes, {avail} available"),
                );
            }
            // Unanswerable platforms stay permissive (store.rs says why).
            Ok(_) => {}
            Err(e) => return err(ErrorCode::Internal, &e.to_string()),
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
            return err(ErrorCode::Internal, &format!("staging write: {io}"));
        }
        Err(join) => {
            let _ = std::fs::remove_file(&path);
            return err(ErrorCode::Internal, &format!("staging task failed: {join}"));
        }
    };
    // A truncated body must not become a silently-short ROM.
    if stream_error || declared.is_some_and(|len| len != written) {
        let _ = std::fs::remove_file(&path);
        return err(ErrorCode::BadRequest, "upload aborted or short body");
    }
    if written == 0 {
        let _ = std::fs::remove_file(&path);
        return err(ErrorCode::BadRequest, "empty upload");
    }

    let token = match auth::mint_token() {
        Ok(token) => token,
        Err(e) => {
            let _ = std::fs::remove_file(&path);
            return err(ErrorCode::Internal, &format!("entropy: {e}"));
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
        return Err(err(ErrorCode::BadRequest, "missing query param \"name\""));
    }
    if name.starts_with('/')
        || name.contains('\0')
        || name.split('/').any(|seg| seg.is_empty() || seg == "..")
    {
        return Err(err(
            ErrorCode::BadRequest,
            "name must be a relative path without \"..\" segments",
        ));
    }
    Ok(name)
}

// ---- POST /v1/ingest ----

pub(crate) async fn start(
    State(app): State<Arc<App>>,
    Extension(caller): Extension<Caller>,
    crate::http::ApiJson(req): crate::http::ApiJson<IngestRequest>,
) -> Response {
    run_blocking(move || {
        require_owner(&caller)?;
        let tokens = req.uploads;
        if tokens.is_empty() {
            return Err(err(ErrorCode::BadRequest, "uploads must not be empty"));
        }
        let staged = app.jobs.take(&tokens).map_err(|t| {
            err(
                ErrorCode::UploadExpired,
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
    // Job boundaries log at INFO per D81 (tracing replaced eprintln).
    info!(
        "ingest job {id}: {} files, {} bytes",
        staged.len(),
        staged.iter().map(|f| f.bytes).sum::<u64>()
    );
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
                let error = format!("matched baseline: {e}");
                warn!("ingest job {id}: FAILED — {error}");
                app.jobs.fail(id, &error, auth::now_unix());
                return;
            }
        }
    };
    // Blob ids that became resident across the whole batch — the
    // fresh slice the refine worker fast-tracks (D71) once the job's
    // closing rollup pass is done.
    let mut fresh_blobs: Vec<i64> = Vec::new();
    for upload in staged {
        app.jobs.set_current(id, &upload.name);
        let (per_file, fresh) = process_upload(app, &upload);
        fresh_blobs.extend(fresh);
        // A dat sliding in on the ROM surface is worth its own line —
        // it changes what the catalog wants, not just what's stored.
        for dat in &per_file.dats_imported {
            info!(
                "ingest job {id}: imported dat {}/{} ({} entries)",
                dat.provider, dat.system, dat.entries
            );
        }
        // Win or lose, the staged copy is spent (a failure is recorded
        // in the report; the bytes that mattered are in the store).
        let _ = std::fs::remove_file(&upload.path);
        app.jobs.file_done(id, upload.bytes, per_file);
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
                let error = format!("catalog refresh: {e}");
                warn!("ingest job {id}: FAILED — {error}");
                app.jobs.fail(id, &error, auth::now_unix());
                return;
            }
        }
    }
    app.jobs.finish(id, auth::now_unix());
    // The narrow slice of new content refines NOW, not on the ambient
    // clock (D71) — this is what makes "drop a zip, watch it become
    // rebuildable" one motion instead of a CLI errand.
    if let Some(refiner) = &app.refiner {
        refiner.notify_fresh(fresh_blobs);
    }
    // The registry accumulated the report; the finish line summarizes
    // it with the web report card's arithmetic (Ingest.svelte): dupes =
    // present + unchanged, members = claimed + extracted, refused =
    // errors + member skips + skipper-capped files.
    if let Some(d) = app.jobs.detail(id) {
        let r = &d.report;
        info!(
            "ingest job {id}: done — {} stored, {} dupes, {} members, {} refused, {} matched",
            r.files_stored,
            r.files_already_present + r.files_unchanged,
            r.members_claimed + r.members_extracted,
            r.errors.len() as u64 + r.member_skips.len() as u64 + r.skipper_skipped_large,
            d.matched_total
        );
    }
}

/// How much of a file the dat sniff reads: `datboi_formats::detect`
/// looks at the first 4 KiB at most, so reading more buys nothing —
/// a 300 MB MAME listxml classifies from its head.
const SNIFF_PREFIX: usize = 4096;

/// What a staged file turned out to be. Content decides, never the
/// name — the house detection philosophy, now on the drop surface too.
enum Classified {
    /// Detected dat bytes (loose, or extracted from a sole-member
    /// zip), fully buffered: `import_dat` parses one contiguous slice,
    /// and dats::BODY_LIMIT's 512 MiB reasoning bounds the buffer.
    Dat(Vec<u8>),
    /// Everything else is the pipeline's problem — including corrupt
    /// or multi-member zips, whose judgment (and reporting) the
    /// Ingester already owns.
    Rom,
}

/// One staged file's outcome as a per-file report body plus the blob
/// ids it made resident (the refine worker's fresh slice): dats fill
/// the `dats_imported` lane (pipeline counters stay pure), ROMs run
/// the pipeline, and classification/import failures land in `errors`
/// under the client's name — the job continues either way.
fn process_upload(app: &App, upload: &StagedUpload) -> (IngestReportBody, Vec<i64>) {
    match classify(&upload.path, upload.bytes) {
        Ok(Classified::Dat(bytes)) => {
            let mut db = app
                .db
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let body = match import_dat_upload(app, &mut db, &bytes, &upload.name) {
                Ok(item) => IngestReportBody {
                    dats_imported: vec![item],
                    ..IngestReportBody::default()
                },
                Err(error) => error_body(&upload.name, &error),
            };
            (body, Vec::new())
        }
        Ok(Classified::Rom) => {
            let mut db = app
                .db
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            // The client's relative name is the source identity —
            // provenance (orphan review included) must never show the
            // throwaway staging path.
            let mut report = Ingester::new(app.store, &mut db, &app.detectors)
                .ingest_file(&upload.path, &upload.name);
            let fresh = std::mem::take(&mut report.fresh_blobs);
            (translate(report, upload), fresh)
        }
        Err(e) => (
            error_body(&upload.name, &format!("classify: {e}")),
            Vec::new(),
        ),
    }
}

/// Classify by content. Zip trouble (unparsable directory, lying
/// sizes) deliberately answers `Rom`, not an error: the pipeline
/// stores the literal and reports the problem with more context than
/// a probe could.
fn classify(path: &Path, size: u64) -> std::io::Result<Classified> {
    let mut file = File::open(path)?;
    let mut head = Vec::with_capacity(SNIFF_PREFIX);
    (&mut file)
        .take(SNIFF_PREFIX as u64)
        .read_to_end(&mut head)?;
    if size <= DAT_LIMIT as u64 && datboi_formats::detect(&head).is_some() {
        return Ok(Classified::Dat(std::fs::read(path)?));
    }
    if zip::looks_like_zip(&head) {
        // The common zipped-dat shape (No-Intro/Redump ship them one
        // per archive): EXACTLY one member whose head detects as a
        // dat. A multi-member zip is a ROM container by construction.
        let probe = match zip::read_sole_member(&mut file, SNIFF_PREFIX as u64) {
            Ok(Some(probe)) => probe,
            Ok(None) | Err(_) => return Ok(Classified::Rom),
        };
        if probe.uncomp_size <= DAT_LIMIT as u64
            && datboi_formats::detect(&probe.bytes).is_some()
            && let Ok(Some(full)) = zip::read_sole_member(&mut file, DAT_LIMIT as u64)
        {
            return Ok(Classified::Dat(full.bytes));
        }
    }
    Ok(Classified::Rom)
}

/// Import classified dat bytes and answer the report row — resolved
/// provider/system the same way dats.rs does (the caller never saw the
/// dat header). No overrides: content arrived nameless.
fn import_dat_upload(
    app: &App,
    db: &mut Db,
    bytes: &[u8],
    name: &str,
) -> Result<DatImportedItem, String> {
    let report = import_dat(
        app.store,
        db,
        bytes,
        &ImportOptions {
            provider: None,
            system: None,
            imported_at: auth::now_unix(),
        },
    )
    .map_err(|e| e.to_string())?;
    let (provider, system) = db
        .cache()
        .query_row(
            "SELECT provider, system FROM dat_source WHERE source_id = ?1",
            [report.source_id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .map_err(|e| e.to_string())?;
    Ok(DatImportedItem {
        path: name.to_owned(),
        provider,
        system,
        entries: report.entries,
    })
}

/// A per-file failure as a report body: one error row, client name.
fn error_body(name: &str, error: &str) -> IngestReportBody {
    IngestReportBody {
        errors: vec![IngestErrorItem {
            path: name.to_owned(),
            error: error.to_owned(),
        }],
        ..IngestReportBody::default()
    }
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
