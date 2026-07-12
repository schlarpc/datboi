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
const RESERVED_PREFIXES: &[&str] = &["v1", "view", "snap", "dav"];

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

/// Extension → media type for what a vite dist actually contains. A
/// full mime database would be dead weight for a closed set of files.
fn content_type(path: &str) -> &'static str {
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

    /// The ONE inline script (the theme-flash guard) is admitted into
    /// the CSP by hash-source. Recompute the hash from the embedded
    /// dist: edit the script without re-pinning hardening.rs and this
    /// goes red instead of the browser silently refusing to run it.
    #[test]
    fn csp_hash_matches_the_dist_inline_script() {
        let index = WEB_DIST.get_file("index.html").expect("index.html");
        let html = std::str::from_utf8(index.contents()).expect("utf8");
        let start = html.find("<script>").expect("inline theme script") + "<script>".len();
        let end = start + html[start..].find("</script>").expect("script close");
        let script = &html[start..end];

        use sha2::Digest as _;
        let digest = sha2::Sha256::digest(script.as_bytes());
        let hash = base64(&digest);
        assert!(
            crate::hardening::CSP.contains(&format!("'sha256-{hash}'")),
            "CSP does not admit the dist's inline script (sha256-{hash})"
        );
        // ...and it really is the only one.
        assert_eq!(html.matches("<script>").count(), 1);
    }

    /// Standard base64 (RFC 4648), enough for one 32-byte digest —
    /// hand-rolled so the test needs no extra dependency.
    fn base64(bytes: &[u8]) -> String {
        const TABLE: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::new();
        for chunk in bytes.chunks(3) {
            let b = [
                chunk[0],
                *chunk.get(1).unwrap_or(&0),
                *chunk.get(2).unwrap_or(&0),
            ];
            let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
            out.push(TABLE[(n >> 18) as usize & 63] as char);
            out.push(TABLE[(n >> 12) as usize & 63] as char);
            out.push(if chunk.len() > 1 {
                TABLE[(n >> 6) as usize & 63] as char
            } else {
                '='
            });
            out.push(if chunk.len() > 2 {
                TABLE[n as usize & 63] as char
            } else {
                '='
            });
        }
        out
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
