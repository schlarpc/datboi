//! HTTP surface: view/snapshot trees with Range semantics (RFC 9110).
//!
//! Every byte served to a client comes out of [`Executor::serve_range`]
//! — the D49 verified path — in bounded windows; the daemon never
//! buffers a whole file. Opaque-routed, non-resident blobs are
//! materialized (one verified replay, D25 machinery) before long
//! streams rather than re-spilled per window; the bytes land in the
//! store where the residency planner can evict them again later —
//! "the store is the cache".

// Fallible steps short-circuit with the error RESPONSE itself (moved
// once, straight to the client) — the "large Err" is the point.
#![allow(clippy::result_large_err)]

use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, Path as UrlPath, RawQuery, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::response::Response;
use axum::routing::{any, delete, get, post};
use axum::{Extension, Router};
use datboi_core::hash::Blake3;
use serde_json::json;

use crate::{admin, api, dats};

use crate::App;
use crate::auth::{self, Caller};
use crate::vfs::{self, LookupError, RowMeta, ViewIndex};

/// Streamed responses move through the verified range path in windows
/// of this size (a multiple of the 16 KiB bao group).
const WINDOW: u64 = 8 << 20;

pub(crate) fn router(app: Arc<App>) -> Router {
    let dav = crate::dav::handler(Arc::clone(&app));
    let dav_route = move |req: axum::extract::Request| {
        let dav = dav.clone();
        async move { dav.handle(req).await.map(Body::new) }
    };
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        // Auth surface (D30/D68): open endpoints — the invitee/login
        // caller has no identity yet by definition.
        .route("/v1/auth/whoami", get(auth::whoami))
        .route("/v1/auth/invite/accept", post(auth::invite_accept))
        .route("/v1/auth/login", post(auth::login))
        .route("/v1/auth/logout", post(auth::logout))
        // The M5 read-model API (api.rs) + admin management (admin.rs).
        // Owner-only except the view surface (friends see grants, D68).
        .route("/v1/systems", get(api::systems))
        .route("/v1/systems/{id}/entries", get(api::system_entries))
        .route("/v1/systems/{id}/entries/{*name}", get(api::system_entry))
        // The one mutating catalog action the web owns (dats.rs) —
        // request-sized, unlike the CLI-only pipeline actions. The
        // route-level limit replaces axum's 2 MiB default; real dats
        // run to hundreds of MiB.
        .route(
            "/v1/dats/import",
            post(dats::import).layer(DefaultBodyLimit::max(dats::BODY_LIMIT)),
        )
        .route("/v1/views", get(api::views))
        .route("/v1/views/{name}", get(api::view_detail))
        .route("/v1/views/{name}/files", get(api::view_files))
        .route("/v1/views/{name}/image", get(view_image))
        .route("/v1/storage", get(api::storage))
        .route("/v1/jobs", get(api::jobs))
        .route("/v1/admin/users", get(admin::users))
        .route("/v1/admin/invites", post(admin::invite_create))
        .route(
            "/v1/admin/invites/{token_hash}",
            delete(admin::invite_delete),
        )
        .route("/v1/admin/grants", post(admin::grant_create))
        .route(
            "/v1/admin/grants/{username}/{view}",
            delete(admin::grant_delete),
        )
        .route(
            "/v1/admin/sessions/{username}",
            delete(admin::sessions_delete),
        )
        .route("/view/{name}", get(view_bare))
        .route("/view/{name}/", get(view_root))
        .route("/view/{name}/{*path}", get(view_path))
        .route("/snap/{hash}", get(snap_bare))
        .route("/snap/{hash}/", get(snap_root))
        .route("/snap/{hash}/{*path}", get(snap_path))
        .route("/dav", any(dav_route.clone()))
        .route("/dav/", any(dav_route.clone()))
        .route("/dav/{*path}", any(dav_route))
        // `/` and every path the API didn't claim belong to the web UI
        // (D67): embedded dist with an SPA fallback. The old plaintext
        // root listing died with it — its content is `/v1/views`.
        .fallback(crate::web::fallback)
        // Identity resolution + per-class enforcement (D68) wraps
        // everything, fallback included; the gate also runs the D70
        // Fetch-Metadata CSRF check before any handler.
        .layer(axum::middleware::from_fn_with_state(
            Arc::clone(&app),
            auth::gate,
        ))
        // Security headers (D70) go outermost so every response out of
        // the router wears them — handlers, the SPA fallback, DAV, and
        // the gate's own rejections alike.
        .layer(axum::middleware::from_fn(
            crate::hardening::security_headers,
        ))
        .with_state(app)
}

// ---- handlers ----
// (the /v1 JSON read models live in api.rs; the byte-serving surfaces
// — the view/snap trees and the minted-image download — live here)

async fn view_bare(UrlPath(name): UrlPath<String>) -> Response {
    redirect(&format!("/view/{}/", enc_seg(&name)))
}

async fn snap_bare(UrlPath(hash): UrlPath<String>) -> Response {
    redirect(&format!("/snap/{}/", enc_seg(&hash)))
}

async fn view_root(
    State(app): State<Arc<App>>,
    Extension(caller): Extension<Caller>,
    UrlPath(name): UrlPath<String>,
    method: Method,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
) -> Response {
    serve_tree(
        app,
        caller,
        TreeRef::View(name),
        String::new(),
        method,
        headers,
        query,
    )
    .await
}

async fn view_path(
    State(app): State<Arc<App>>,
    Extension(caller): Extension<Caller>,
    UrlPath((name, path)): UrlPath<(String, String)>,
    method: Method,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
) -> Response {
    serve_tree(
        app,
        caller,
        TreeRef::View(name),
        path,
        method,
        headers,
        query,
    )
    .await
}

async fn snap_root(
    State(app): State<Arc<App>>,
    Extension(caller): Extension<Caller>,
    UrlPath(hash): UrlPath<String>,
    method: Method,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
) -> Response {
    serve_tree(
        app,
        caller,
        TreeRef::Snap(hash),
        String::new(),
        method,
        headers,
        query,
    )
    .await
}

async fn snap_path(
    State(app): State<Arc<App>>,
    Extension(caller): Extension<Caller>,
    UrlPath((hash, path)): UrlPath<(String, String)>,
    method: Method,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
) -> Response {
    serve_tree(
        app,
        caller,
        TreeRef::Snap(hash),
        path,
        method,
        headers,
        query,
    )
    .await
}

/// GET /v1/views/{name}/image — the minted SD image (D62 `image/<name>`
/// tag), friend-visible per grant like the rest of the view surface.
/// The image blob is content-addressed with verified windows like any
/// manifest row, so it rides the exact same Range/ETag/windowed-stream
/// path `/view/{name}/{path}` uses; only the provenance header and the
/// download name differ.
async fn view_image(
    State(app): State<Arc<App>>,
    Extension(caller): Extension<Caller>,
    UrlPath(name): UrlPath<String>,
    method: Method,
    headers: HeaderMap,
) -> Response {
    run_blocking(move || {
        let row = {
            let db = app
                .db
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            // View-scoped resource: denial answers exactly like a miss
            // (auth.rs convention) so probing learns nothing. Errors
            // wear the typed /v1 shape (D69) — this is a /v1 route,
            // however binary its success body.
            if !auth::view_allowed(&db, &caller, &name) {
                return Err(api::err(StatusCode::NOT_FOUND, "no such view"));
            }
            let hash = db
                .get_tag(&format!("image/{name}"))
                .map_err(|e| api::err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))?
                .ok_or_else(|| api::err(StatusCode::NOT_FOUND, "no image minted for this view"))?;
            // A tag pointing at an unindexed/sizeless blob is server-side
            // damage, not a client miss.
            let blob = db
                .blob_by_hash(&hash)
                .map_err(|e| api::err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))?
                .ok_or_else(|| {
                    api::err(StatusCode::INTERNAL_SERVER_ERROR, "image blob not indexed")
                })?;
            let size = blob.size.ok_or_else(|| {
                api::err(StatusCode::INTERNAL_SERVER_ERROR, "image blob has no size")
            })?;
            // D27 class, derived exactly like snapshot rows record it:
            // resident reads affinely, otherwise the best route's class
            // (the mint recipe is affine assemble, D62).
            let seek = if blob.residency == datboi_index::Residency::Resident {
                0
            } else {
                db.recipes_for_output(blob.blob_id)
                    .map_err(|e| api::err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))?
                    .iter()
                    .filter(|r| r.verify != datboi_index::VerifyState::Failed)
                    .map(|r| match r.seek_class {
                        datboi_index::SeekClass::Affine => 0,
                        datboi_index::SeekClass::ManifestSeekable => 1,
                        datboi_index::SeekClass::Opaque => 2,
                    })
                    .min()
                    .unwrap_or(2)
            };
            RowMeta { hash, size, seek }
        };
        // The tag is mutable (re-mint moves it), so no `immutable`
        // caching; the strong ETag keeps revalidation free.
        let disposition = format!("attachment; filename=\"{}.img\"", filename_safe(&name));
        Ok(file_response(
            &app,
            row,
            &method,
            &headers,
            false,
            None,
            Some(&disposition),
        ))
    })
    .await
}

/// Conservative quoted-string filename: anything outside printable
/// ASCII (or a quote/backslash) becomes `_` so the header value never
/// needs escaping rules the receiving side might disagree about.
fn filename_safe(name: &str) -> String {
    name.chars()
        .map(|c| match c {
            ' '..='~' if c != '"' && c != '\\' => c,
            _ => '_',
        })
        .collect()
}

/// How the request named the tree: a mutable view tag (resolved per
/// request, `no-cache`) or an immutable snapshot hash (`immutable`).
enum TreeRef {
    View(String),
    Snap(String),
}

#[allow(clippy::too_many_arguments)]
async fn serve_tree(
    app: Arc<App>,
    caller: Caller,
    tree: TreeRef,
    path: String,
    method: Method,
    headers: HeaderMap,
    query: Option<String>,
) -> Response {
    run_blocking(move || {
        // ACL (D68): owners see everything; friends see granted views
        // and their current snapshots. Denials answer exactly like
        // misses so probing leaks nothing about what exists.
        let (idx, immutable, url_base) = match &tree {
            TreeRef::View(name) => {
                let allowed = {
                    let db = app
                        .db
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    auth::view_allowed(&db, &caller, name)
                };
                if !allowed {
                    return Err(text(StatusCode::NOT_FOUND, "no such view"));
                }
                (
                    vfs::view_index(&app, name)
                        .map_err(|e| map_lookup(&e, StatusCode::INTERNAL_SERVER_ERROR))?,
                    false,
                    format!("/view/{}", enc_seg(name)),
                )
            }
            TreeRef::Snap(hex) => {
                let hash: Blake3 = hex
                    .parse()
                    .map_err(|_| text(StatusCode::BAD_REQUEST, "not a snapshot hash"))?;
                let allowed = {
                    let db = app
                        .db
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    auth::snap_allowed(&db, &caller, &hash)
                };
                if !allowed {
                    return Err(text(StatusCode::NOT_FOUND, "snapshot not in store"));
                }
                (
                    vfs::snapshot_index(&app, hash)
                        .map_err(|e| map_lookup(&e, StatusCode::NOT_FOUND))?,
                    true,
                    format!("/snap/{hex}"),
                )
            }
        };
        let want_json = query.as_deref() == Some("json");
        Ok(tree_response(
            &app, &idx, &path, &method, &headers, want_json, immutable, &url_base,
        ))
    })
    .await
}

// ---- tree dispatch (blocking context) ----

#[allow(clippy::too_many_arguments)]
fn tree_response(
    app: &Arc<App>,
    idx: &ViewIndex,
    path: &str,
    method: &Method,
    headers: &HeaderMap,
    want_json: bool,
    immutable: bool,
    url_base: &str,
) -> Response {
    if let Some(prefix) = path
        .strip_suffix('/')
        .or(if path.is_empty() { Some("") } else { None })
    {
        // Directory form. Manifest paths are canonical, so lookups are
        // pure string comparisons — no filesystem is ever consulted.
        if idx.is_dir(prefix) {
            return listing_response(idx, prefix, want_json, immutable);
        }
        return text(StatusCode::NOT_FOUND, "no such directory");
    }
    if let Some(row) = idx.file(path) {
        return file_response(
            app,
            row,
            method,
            headers,
            immutable,
            Some(idx.snapshot),
            None,
        );
    }
    if idx.is_dir(path) {
        // Canonicalize to the trailing-slash form so relative links in
        // listings resolve.
        return redirect(&format!("{url_base}/{}/", enc_path(path)));
    }
    text(StatusCode::NOT_FOUND, "no such path in snapshot")
}

// ---- resolution (blocking context) ----

/// Map a shared-VFS lookup failure to a status. `missing` distinguishes
/// a client-supplied snapshot hash (404) from a tagged snapshot whose
/// blob is gone — server-side damage, 500.
fn map_lookup(e: &LookupError, missing: StatusCode) -> Response {
    match e {
        LookupError::NoSuchView => text(StatusCode::NOT_FOUND, "no such view"),
        LookupError::SnapshotMissing => text(missing, "snapshot not in store"),
        LookupError::Corrupt(_) | LookupError::Internal(_) => {
            text(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string())
        }
    }
}

// ---- file serving (blocking context) ----

/// Serve one content-addressed blob (a manifest row, or the minted
/// image posing as one) with Range/conditional semantics. `snapshot`
/// stamps the provenance header when the bytes came out of a snapshot
/// tree; `disposition` names the download (the image route).
#[allow(clippy::too_many_arguments)]
fn file_response(
    app: &Arc<App>,
    row: RowMeta,
    method: &Method,
    headers: &HeaderMap,
    immutable: bool,
    snapshot: Option<Blake3>,
    disposition: Option<&str>,
) -> Response {
    let etag = format!("\"{}\"", row.hash.to_hex());
    let cache_control = if immutable {
        "public, max-age=31536000, immutable"
    } else {
        // View URLs re-resolve the tag per request; the strong ETag
        // (content hash) makes revalidation free.
        "no-cache"
    };
    let base = |status: StatusCode| {
        let mut b = Response::builder()
            .status(status)
            .header(header::ETAG, &etag)
            .header(header::CACHE_CONTROL, cache_control)
            .header(header::ACCEPT_RANGES, "bytes");
        if let Some(snapshot) = snapshot {
            b = b.header("datboi-snapshot", snapshot.to_hex());
        }
        if let Some(disposition) = disposition {
            b = b.header(header::CONTENT_DISPOSITION, disposition);
        }
        b
    };

    if if_none_match(headers, &etag) {
        return base(StatusCode::NOT_MODIFIED)
            .body(Body::empty())
            .expect("static headers");
    }

    // Range applies to GET only (RFC 9110 §14.2); HEAD reports the
    // full representation.
    let range = if *method == Method::GET {
        parse_range(
            headers.get(header::RANGE).and_then(|v| v.to_str().ok()),
            row.size,
        )
    } else {
        RangeOutcome::Full
    };
    let range = match range {
        RangeOutcome::Partial { .. } if !if_range_allows(headers, &etag) => RangeOutcome::Full,
        other => other,
    };

    let (status, start, end) = match range {
        RangeOutcome::Unsatisfiable => {
            return base(StatusCode::RANGE_NOT_SATISFIABLE)
                .header(header::CONTENT_RANGE, format!("bytes */{}", row.size))
                .body(Body::empty())
                .expect("static headers");
        }
        RangeOutcome::Full => (StatusCode::OK, 0, row.size),
        RangeOutcome::Partial { start, end } => (StatusCode::PARTIAL_CONTENT, start, end),
    };
    let span = end - start;
    let mut builder = base(status)
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(header::CONTENT_LENGTH, span);
    if status == StatusCode::PARTIAL_CONTENT {
        builder = builder.header(
            header::CONTENT_RANGE,
            format!("bytes {}-{}/{}", start, end - 1, row.size),
        );
    }

    if *method == Method::HEAD || span == 0 {
        return builder.body(Body::empty()).expect("static headers");
    }

    if span <= WINDOW {
        // Small enough to answer in one verified read — and to report
        // failures as proper statuses instead of a broken stream.
        let result = {
            let db = app
                .db
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            app.exec.serve_range(&db, &row.hash, start, span)
        };
        return match result {
            Ok(bytes) if bytes.len() as u64 == span => {
                builder.body(Body::from(bytes)).expect("static headers")
            }
            Ok(bytes) => text(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("short read: {} of {span} bytes", bytes.len()),
            ),
            Err(e) => text(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("serving {}: {e}", row.hash),
            ),
        };
    }

    // Long body: verified windows through a bounded channel. A failure
    // mid-stream aborts the connection — the client never sees bad
    // bytes presented as success (the D49 EIO analog).
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(2);
    let app = Arc::clone(app);
    tokio::task::spawn_blocking(move || feed_windows(&app, row, start, end, &tx));
    builder
        .body(Body::from_stream(RecvStream(rx)))
        .expect("static headers")
}

fn feed_windows(
    app: &App,
    row: RowMeta,
    start: u64,
    end: u64,
    tx: &tokio::sync::mpsc::Sender<Result<Bytes, std::io::Error>>,
) {
    // An opaque route re-spills its whole upstream for EVERY window —
    // quadratic over a long body. One verified replay into the store
    // instead (D25 machinery); the planner may evict the bytes again
    // later. Affine/manifest-seekable routes stream window-by-window
    // as-is.
    if row.seek == 2 {
        let resident = {
            let db = app
                .db
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match db.blob_by_hash(&row.hash) {
                Ok(Some(blob)) => blob.residency == datboi_index::Residency::Resident,
                Ok(None) => false,
                Err(e) => {
                    let _ = tx.blocking_send(Err(std::io::Error::other(e.to_string())));
                    return;
                }
            }
        };
        if !resident {
            let db = app
                .db
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Err(e) = app.exec.materialize(&db, &row.hash) {
                let _ = tx.blocking_send(Err(std::io::Error::other(e.to_string())));
                return;
            }
        }
    }
    let mut off = start;
    while off < end {
        let want = WINDOW.min(end - off);
        let result = {
            let db = app
                .db
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            app.exec.serve_range(&db, &row.hash, off, want)
        };
        match result {
            Ok(bytes) if bytes.len() as u64 == want => {
                if tx.blocking_send(Ok(Bytes::from(bytes))).is_err() {
                    return; // client went away
                }
            }
            Ok(bytes) => {
                let _ = tx.blocking_send(Err(std::io::Error::other(format!(
                    "short read at {off}: {} of {want} bytes",
                    bytes.len()
                ))));
                return;
            }
            Err(e) => {
                let _ = tx.blocking_send(Err(std::io::Error::other(e.to_string())));
                return;
            }
        }
        off += want;
    }
}

/// mpsc receiver as a response-body stream (no futures runtime dep).
struct RecvStream(tokio::sync::mpsc::Receiver<Result<Bytes, std::io::Error>>);

impl futures_core::Stream for RecvStream {
    type Item = Result<Bytes, std::io::Error>;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.0.poll_recv(cx)
    }
}

// ---- range / conditional parsing ----

#[derive(Debug, PartialEq, Eq)]
enum RangeOutcome {
    Full,
    /// Half-open byte window, already clamped to the representation.
    Partial {
        start: u64,
        end: u64,
    },
    Unsatisfiable,
}

/// Single-range `bytes=` parser. Multi-range and malformed headers are
/// ignored (RFC 9110 permits ignoring Range entirely) — the client
/// gets a correct 200 instead of a guessy 206.
fn parse_range(header: Option<&str>, total: u64) -> RangeOutcome {
    let Some(value) = header else {
        return RangeOutcome::Full;
    };
    let Some(spec) = value.trim().strip_prefix("bytes=") else {
        return RangeOutcome::Full;
    };
    if spec.contains(',') {
        return RangeOutcome::Full;
    }
    let spec = spec.trim();
    if let Some(suffix) = spec.strip_prefix('-') {
        // suffix form: last N bytes
        let Ok(n) = suffix.parse::<u64>() else {
            return RangeOutcome::Full;
        };
        if n == 0 || total == 0 {
            return RangeOutcome::Unsatisfiable;
        }
        return RangeOutcome::Partial {
            start: total.saturating_sub(n),
            end: total,
        };
    }
    let Some((first, last)) = spec.split_once('-') else {
        return RangeOutcome::Full;
    };
    let Ok(start) = first.parse::<u64>() else {
        return RangeOutcome::Full;
    };
    if start >= total {
        return RangeOutcome::Unsatisfiable;
    }
    if last.is_empty() {
        return RangeOutcome::Partial { start, end: total };
    }
    let Ok(last) = last.parse::<u64>() else {
        return RangeOutcome::Full;
    };
    if last < start {
        return RangeOutcome::Full;
    }
    RangeOutcome::Partial {
        start,
        end: last.saturating_add(1).min(total),
    }
}

fn if_none_match(headers: &HeaderMap, etag: &str) -> bool {
    let Some(value) = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
    else {
        return false;
    };
    value.split(',').any(|candidate| {
        let candidate = candidate.trim();
        candidate == "*" || candidate.strip_prefix("W/").unwrap_or(candidate) == etag
    })
}

/// `If-Range` gates a Partial: serve the range only when the validator
/// still matches. Date validators are treated as mismatch (we don't
/// emit Last-Modified), which safely degrades to a full 200.
fn if_range_allows(headers: &HeaderMap, etag: &str) -> bool {
    match headers.get(header::IF_RANGE).and_then(|v| v.to_str().ok()) {
        None => true,
        Some(value) => value.trim() == etag,
    }
}

// ---- listings ----

fn listing_response(idx: &ViewIndex, prefix: &str, want_json: bool, immutable: bool) -> Response {
    let listing = idx.list(prefix);
    let cache_control = if immutable {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    };
    if want_json {
        let body = json!({
            "view": idx.view_name,
            "snapshot": idx.snapshot.to_hex(),
            "created_at": idx.created_at,
            "path": prefix,
            "dirs": listing.dirs,
            "files": listing.files.iter().map(|(name, meta)| json!({
                "name": name,
                "hash": meta.hash.to_hex(),
                "size": meta.size,
                "seek": meta.seek,
            })).collect::<Vec<_>>(),
        });
        let mut resp = json_response(StatusCode::OK, &body);
        resp.headers_mut().insert(
            header::CACHE_CONTROL,
            HeaderValue::from_static(if immutable {
                "public, max-age=31536000, immutable"
            } else {
                "no-cache"
            }),
        );
        return resp;
    }
    let title = if prefix.is_empty() {
        format!("{}/", idx.view_name)
    } else {
        format!("{}/{prefix}/", idx.view_name)
    };
    let mut body = format!(
        "<!doctype html><meta charset=\"utf-8\"><title>{0}</title><h1>{0}</h1><ul>",
        html_escape(&title)
    );
    if !prefix.is_empty() {
        body.push_str("<li><a href=\"../\">../</a></li>");
    }
    for dir in &listing.dirs {
        body.push_str(&format!(
            "<li><a href=\"{}/\">{}/</a></li>",
            enc_seg(dir),
            html_escape(dir)
        ));
    }
    for (name, meta) in &listing.files {
        body.push_str(&format!(
            "<li><a href=\"{}\">{}</a> <small>{} bytes</small></li>",
            enc_seg(name),
            html_escape(name),
            meta.size
        ));
    }
    body.push_str(&format!(
        "</ul><footer><small>snapshot <code>{}</code></small></footer>",
        idx.snapshot.to_hex()
    ));
    let mut resp = html(StatusCode::OK, body);
    resp.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(cache_control),
    );
    resp
}

// ---- small helpers ----

pub(crate) async fn run_blocking(
    f: impl FnOnce() -> Result<Response, Response> + Send + 'static,
) -> Response {
    match tokio::task::spawn_blocking(f).await {
        Ok(Ok(resp) | Err(resp)) => resp,
        Err(join) => text(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("request task failed: {join}"),
        ),
    }
}

pub(crate) fn text(status: StatusCode, msg: &str) -> Response {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .body(Body::from(format!("{msg}\n")))
        .expect("static headers")
}

fn html(status: StatusCode, body: String) -> Response {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
        .body(Body::from(body))
        .expect("static headers")
}

/// Serialize any contract type (datboi-api, D69) — or a plain
/// `serde_json::Value` for the non-/v1 listing surface — as a JSON
/// response.
pub(crate) fn json_response<T: serde::Serialize>(status: StatusCode, value: &T) -> Response {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            serde_json::to_string(value).expect("contract types serialize"),
        ))
        .expect("static headers")
}

fn redirect(location: &str) -> Response {
    Response::builder()
        .status(StatusCode::PERMANENT_REDIRECT)
        .header(
            header::LOCATION,
            HeaderValue::from_str(location).expect("percent-encoded"),
        )
        .body(Body::empty())
        .expect("static headers")
}

fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// Percent-encode one path segment (RFC 3986 unreserved kept verbatim).
pub(crate) fn enc_seg(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Percent-encode a manifest path, preserving `/` separators.
fn enc_path(s: &str) -> String {
    s.split('/').map(enc_seg).collect::<Vec<_>>().join("/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn range_parser_covers_the_rfc_shapes() {
        use RangeOutcome::{Full, Partial, Unsatisfiable};
        let total = 100;
        assert_eq!(parse_range(None, total), Full);
        assert_eq!(
            parse_range(Some("bytes=0-49"), total),
            Partial { start: 0, end: 50 }
        );
        assert_eq!(
            parse_range(Some("bytes=50-"), total),
            Partial {
                start: 50,
                end: 100
            }
        );
        assert_eq!(
            parse_range(Some("bytes=-10"), total),
            Partial {
                start: 90,
                end: 100
            }
        );
        // last-byte-pos beyond EOF clamps
        assert_eq!(
            parse_range(Some("bytes=90-1000"), total),
            Partial {
                start: 90,
                end: 100
            }
        );
        // suffix longer than the file = whole file
        assert_eq!(
            parse_range(Some("bytes=-1000"), total),
            Partial { start: 0, end: 100 }
        );
        assert_eq!(parse_range(Some("bytes=100-"), total), Unsatisfiable);
        assert_eq!(parse_range(Some("bytes=200-300"), total), Unsatisfiable);
        assert_eq!(parse_range(Some("bytes=-0"), total), Unsatisfiable);
        assert_eq!(parse_range(Some("bytes=0-"), 0), Unsatisfiable);
        // ignored shapes → Full
        assert_eq!(parse_range(Some("bytes=5-2"), total), Full);
        assert_eq!(parse_range(Some("bytes=0-1,5-6"), total), Full);
        assert_eq!(parse_range(Some("items=0-1"), total), Full);
        assert_eq!(parse_range(Some("bytes=abc-"), total), Full);
    }

    #[test]
    fn encoders_are_conservative() {
        assert_eq!(enc_seg("Alpha (USA).gba"), "Alpha%20%28USA%29.gba");
        assert_eq!(enc_path("a b/c.bin"), "a%20b/c.bin");
        assert_eq!(html_escape("<b>&\"'"), "&lt;b&gt;&amp;&quot;&#39;");
        assert_eq!(filename_safe("gba-everdrive"), "gba-everdrive");
        assert_eq!(filename_safe("a\"b\\c\néö"), "a_b_c___");
    }
}
