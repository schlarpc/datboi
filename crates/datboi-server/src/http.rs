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

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{Path as UrlPath, RawQuery, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::response::Response;
use axum::routing::get;
use datboi_core::hash::Blake3;
use datboi_core::viewsnap::ViewSnapshot;
use serde_json::json;

use crate::App;
use crate::vfs::{RowMeta, ViewIndex};

/// Streamed responses move through the verified range path in windows
/// of this size (a multiple of the 16 KiB bao group).
const WINDOW: u64 = 8 << 20;

pub(crate) fn router(app: Arc<App>) -> Router {
    Router::new()
        .route("/", get(root))
        .route("/healthz", get(|| async { "ok" }))
        .route("/v1/views", get(views_json))
        .route("/view/{name}", get(view_bare))
        .route("/view/{name}/", get(view_root))
        .route("/view/{name}/{*path}", get(view_path))
        .route("/snap/{hash}", get(snap_bare))
        .route("/snap/{hash}/", get(snap_root))
        .route("/snap/{hash}/{*path}", get(snap_path))
        .with_state(app)
}

// ---- handlers ----

async fn root(State(app): State<Arc<App>>) -> Response {
    run_blocking(move || {
        let views = list_view_tags(&app)?;
        let mut body = String::from(
            "<!doctype html><meta charset=\"utf-8\"><title>datboi</title><h1>datboi views</h1><ul>",
        );
        for (name, snapshot) in &views {
            body.push_str(&format!(
                "<li><a href=\"/view/{}/\">{}</a> <small><code>{}</code></small></li>",
                enc_seg(name),
                html_escape(name),
                snapshot.to_hex()
            ));
        }
        body.push_str("</ul>");
        Ok(html(StatusCode::OK, body))
    })
    .await
}

async fn views_json(State(app): State<Arc<App>>) -> Response {
    run_blocking(move || {
        let views = list_view_tags(&app)?;
        let items: Vec<_> = views
            .iter()
            .map(|(name, snapshot)| json!({"name": name, "snapshot": snapshot.to_hex()}))
            .collect();
        Ok(json_response(StatusCode::OK, &json!({"views": items})))
    })
    .await
}

async fn view_bare(UrlPath(name): UrlPath<String>) -> Response {
    redirect(&format!("/view/{}/", enc_seg(&name)))
}

async fn snap_bare(UrlPath(hash): UrlPath<String>) -> Response {
    redirect(&format!("/snap/{}/", enc_seg(&hash)))
}

async fn view_root(
    State(app): State<Arc<App>>,
    UrlPath(name): UrlPath<String>,
    method: Method,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
) -> Response {
    serve_tree(app, TreeRef::View(name), String::new(), method, headers, query).await
}

async fn view_path(
    State(app): State<Arc<App>>,
    UrlPath((name, path)): UrlPath<(String, String)>,
    method: Method,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
) -> Response {
    serve_tree(app, TreeRef::View(name), path, method, headers, query).await
}

async fn snap_root(
    State(app): State<Arc<App>>,
    UrlPath(hash): UrlPath<String>,
    method: Method,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
) -> Response {
    serve_tree(app, TreeRef::Snap(hash), String::new(), method, headers, query).await
}

async fn snap_path(
    State(app): State<Arc<App>>,
    UrlPath((hash, path)): UrlPath<(String, String)>,
    method: Method,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
) -> Response {
    serve_tree(app, TreeRef::Snap(hash), path, method, headers, query).await
}

/// How the request named the tree: a mutable view tag (resolved per
/// request, `no-cache`) or an immutable snapshot hash (`immutable`).
enum TreeRef {
    View(String),
    Snap(String),
}

async fn serve_tree(
    app: Arc<App>,
    tree: TreeRef,
    path: String,
    method: Method,
    headers: HeaderMap,
    query: Option<String>,
) -> Response {
    run_blocking(move || {
        let (idx, immutable, url_base) = match &tree {
            TreeRef::View(name) => (
                resolve_view(&app, name)?,
                false,
                format!("/view/{}", enc_seg(name)),
            ),
            TreeRef::Snap(hex) => {
                let hash: Blake3 = hex
                    .parse()
                    .map_err(|_| text(StatusCode::BAD_REQUEST, "not a snapshot hash"))?;
                (
                    load_index(&app, hash, StatusCode::NOT_FOUND)?,
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
    if let Some(prefix) = path.strip_suffix('/').or(if path.is_empty() {
        Some("")
    } else {
        None
    }) {
        // Directory form. Manifest paths are canonical, so lookups are
        // pure string comparisons — no filesystem is ever consulted.
        if idx.is_dir(prefix) {
            return listing_response(idx, prefix, want_json, immutable);
        }
        return text(StatusCode::NOT_FOUND, "no such directory");
    }
    if let Some(row) = idx.file(path) {
        return file_response(app, idx, row, method, headers, immutable);
    }
    if idx.is_dir(path) {
        // Canonicalize to the trailing-slash form so relative links in
        // listings resolve.
        return redirect(&format!("{url_base}/{}/", enc_path(path)));
    }
    text(StatusCode::NOT_FOUND, "no such path in snapshot")
}

// ---- resolution (blocking context) ----

fn list_view_tags(app: &App) -> Result<Vec<(String, Blake3)>, Response> {
    let db = app.db.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let tags = db.list_tags().map_err(internal)?;
    Ok(tags
        .into_iter()
        .filter_map(|(name, hash)| {
            name.strip_prefix("view/").map(|n| (n.to_owned(), hash))
        })
        .collect())
}

fn resolve_view(app: &App, name: &str) -> Result<Arc<ViewIndex>, Response> {
    let snapshot = {
        let db = app.db.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        db.get_tag(&format!("view/{name}")).map_err(internal)?
    };
    let Some(snapshot) = snapshot else {
        return Err(text(StatusCode::NOT_FOUND, "no such view"));
    };
    // A tagged snapshot whose blob is missing is server-side damage,
    // not a client mistake — hence 500 here vs 404 for /snap/<hash>.
    load_index(app, snapshot, StatusCode::INTERNAL_SERVER_ERROR)
}

fn load_index(
    app: &App,
    snapshot: Blake3,
    missing: StatusCode,
) -> Result<Arc<ViewIndex>, Response> {
    if let Some(idx) = app
        .manifests
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&snapshot)
    {
        return Ok(Arc::clone(idx));
    }
    let mut bytes = Vec::new();
    {
        use std::io::Read as _;
        let Some(mut file) = app
            .store
            .get(datboi_store_fs::Namespace::Meta, &snapshot)
            .map_err(internal)?
        else {
            return Err(text(missing, "snapshot not in store"));
        };
        file.read_to_end(&mut bytes).map_err(internal)?;
    }
    let snap = ViewSnapshot::decode(&bytes)
        .map_err(|e| text(StatusCode::INTERNAL_SERVER_ERROR, &format!("snapshot does not decode: {e}")))?;
    let idx = Arc::new(ViewIndex::from_snapshot(snapshot, snap));
    let mut cache = app
        .manifests
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if cache.len() >= 64 {
        cache.clear(); // immutable entries: dropping is only a re-decode
    }
    cache.insert(snapshot, Arc::clone(&idx));
    Ok(idx)
}

// ---- file serving (blocking context) ----

fn file_response(
    app: &Arc<App>,
    idx: &ViewIndex,
    row: RowMeta,
    method: &Method,
    headers: &HeaderMap,
    immutable: bool,
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
        Response::builder()
            .status(status)
            .header(header::ETAG, &etag)
            .header(header::CACHE_CONTROL, cache_control)
            .header(header::ACCEPT_RANGES, "bytes")
            .header("datboi-snapshot", idx.snapshot.to_hex())
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
        RangeOutcome::Partial { .. }
            if !if_range_allows(headers, &etag) =>
        {
            RangeOutcome::Full
        }
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
            let db = app.db.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            app.exec.serve_range(&db, &row.hash, start, span)
        };
        return match result {
            Ok(bytes) if bytes.len() as u64 == span => builder
                .body(Body::from(bytes))
                .expect("static headers"),
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
            let db = app.db.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
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
            let db = app.db.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
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
            let db = app.db.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
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
    resp.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static(cache_control));
    resp
}

// ---- small helpers ----

async fn run_blocking(
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

fn internal(e: impl std::fmt::Display) -> Response {
    text(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string())
}

fn text(status: StatusCode, msg: &str) -> Response {
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

fn json_response(status: StatusCode, value: &serde_json::Value) -> Response {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(value.to_string()))
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
fn enc_seg(s: &str) -> String {
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
    }
}
