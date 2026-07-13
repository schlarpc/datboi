//! The embedded web UI (D67): the vite dist from the flake's
//! `packages.web`, baked into the binary at build time exactly like the
//! wasm components (D66 — one binary, no deploy-time asset directory).
//! `DATBOI_WEB_DIST` comes from the flake on hermetic builds and from
//! build.rs (the `nix build .#web` dev fallback) otherwise.

use axum::body::Body;
use axum::http::{Method, StatusCode, Uri, header};
use axum::response::Response;

/// The whole vite dist, in memory — an SPA shell plus content-hashed
/// assets, small enough that no handler here needs spawn_blocking.
static WEB_DIST: include_dir::Dir<'static> = include_dir::include_dir!("$DATBOI_WEB_DIST");

/// URL namespaces that belong to the daemon, not the client router. The
/// axum routes claim everything real under these; whatever still lands
/// in the fallback is a genuine miss and must say so — serving the SPA
/// shell for `/v1/nope` would turn API typos into confusing 200s.
const RESERVED_PREFIXES: &[&str] = &["v1", "view", "snap", "dav", "emu"];

/// Router fallback: everything the API routes didn't claim is the UI's
/// URL space. Real dist files serve as themselves; anything else gets
/// `index.html` with 200 because the client router owns those paths
/// (the D67 SPA fallback). A miss under `assets/` stays a 404: those
/// names are content-hashed, so a miss is a stale reference that html
/// would only mask.
pub(crate) async fn fallback(method: Method, uri: Uri) -> Response {
    if method != Method::GET && method != Method::HEAD {
        return Response::builder()
            .status(StatusCode::METHOD_NOT_ALLOWED)
            .header(header::ALLOW, "GET, HEAD")
            .body(Body::empty())
            .expect("static headers");
    }
    let path = uri.path().trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };
    if let Some(file) = WEB_DIST.get_file(path) {
        return file_response(path, file.contents(), &method);
    }
    let first = path.split('/').next().unwrap_or("");
    if path.starts_with("assets/") || RESERVED_PREFIXES.contains(&first) {
        return crate::http::text(StatusCode::NOT_FOUND, "no such path");
    }
    let index = WEB_DIST
        .get_file("index.html")
        .expect("vite dist always contains index.html");
    file_response("index.html", index.contents(), &method)
}

fn file_response(path: &str, bytes: &'static [u8], method: &Method) -> Response {
    // Vite content-hashes everything under assets/, so those bytes can
    // never change under their name — cache forever, no validators
    // needed. index.html (the mutable entry point that names the
    // hashes) revalidates on every load.
    let cache_control = if path.starts_with("assets/") {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    };
    let body = if *method == Method::HEAD {
        Body::empty()
    } else {
        Body::from(bytes)
    };
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, content_type(path))
        .header(header::CACHE_CONTROL, cache_control)
        .header(header::CONTENT_LENGTH, bytes.len())
        .body(body)
        .expect("static headers")
}

/// Extension → media type for what a vite dist actually contains (and
/// the emu-core assets, src/emu.rs — js/json/wasm/html, same closed
/// set). A full mime database would be dead weight for a closed set.
pub(crate) fn content_type(path: &str) -> &'static str {
    match path.rsplit_once('.').map(|(_, ext)| ext) {
        Some("html") => "text/html; charset=utf-8",
        Some("js") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("woff2") => "font/woff2",
        Some("woff") => "font/woff",
        Some("json") | Some("map") => "application/json",
        Some("txt") => "text/plain; charset=utf-8",
        Some("wasm") => "application/wasm",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dist_contains_the_spa_shell() {
        let index = WEB_DIST.get_file("index.html").expect("index.html");
        let html = std::str::from_utf8(index.contents()).expect("utf8");
        assert!(html.contains("<div id=\"app\""), "{html}");
    }

    /// D78 killed the theme toggle and with it the ONE inline script
    /// (the flash guard), so the dist must carry ZERO inline scripts
    /// and the CSP must carry no hash-source. A reintroduced inline
    /// script goes red here instead of the browser silently refusing
    /// to run it (script-src is 'self' only).
    #[test]
    fn dist_has_no_inline_scripts_and_csp_admits_none() {
        let index = WEB_DIST.get_file("index.html").expect("index.html");
        let html = std::str::from_utf8(index.contents()).expect("utf8");
        assert_eq!(html.matches("<script>").count(), 0, "{html}");
        assert!(
            !crate::hardening::CSP.contains("sha256-"),
            "CSP still carries a stale inline-script hash-source"
        );
    }

    #[test]
    fn content_types_cover_the_dist() {
        assert_eq!(content_type("index.html"), "text/html; charset=utf-8");
        assert_eq!(
            content_type("assets/index-B60MFKo9.js"),
            "text/javascript; charset=utf-8"
        );
        assert_eq!(content_type("assets/a.woff2"), "font/woff2");
        assert_eq!(content_type("noext"), "application/octet-stream");
    }
}
