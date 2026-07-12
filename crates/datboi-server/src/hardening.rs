//! Browser hardening (D70): strict security headers on every response
//! and a token-less Fetch-Metadata CSRF gate on state-changing methods.
//!
//! The CSRF design is the one Go ships as `http.CrossOriginProtection`
//! (Valsorda): trust `Sec-Fetch-Site` where a browser sent it, fall
//! back to an `Origin`-vs-`Host` comparison for older browsers, and let
//! header-less clients (curl/ureq/the CLI/WebDAV tools) through — they
//! carry no ambient cookie, so there is nothing to forge. This matters
//! MORE here than in a normal app: loopback-is-owner (D68) is ambient
//! authority, and DNS rebinding hands a hostile page a loopback origin,
//! so the gate applies to loopback callers too.

use axum::extract::Request;
use axum::http::header::{HeaderName, HeaderValue};
use axum::http::{HeaderMap, Method, header};
use axum::middleware::Next;
use axum::response::Response;

/// The exact D70 policy, tightened by D76. The vite index.html carries
/// no inline `<script>` or `<style>` (verified against the built dist:
/// one external module script, one external stylesheet), so
/// `script-src 'self'` and `style-src 'self'` hold without
/// hash-sources. The SPA's dynamic styles are Svelte `style:`
/// directives, which compile to CSSOM writes (`el.style.setProperty`/
/// `cssText`) — CSP governs parsed `style=` attributes and `<style>`
/// blocks, not CSSOM, and the built bundle contains zero `style=`
/// attributes and no `setAttribute("style")`, so `'unsafe-inline'` is
/// not needed. `object-src 'none'` closes the legacy plugin surface
/// that would otherwise inherit `'self'` from default-src.
pub(crate) const CSP: &str = "default-src 'self'; script-src 'self'; \
     style-src 'self'; img-src 'self' data:; font-src 'self'; \
     connect-src 'self'; object-src 'none'; frame-ancestors 'none'; \
     base-uri 'none'; form-action 'self'";

/// Response-path middleware: stamp the D70 header set on EVERY response
/// out of the router — handlers, errors, the SPA fallback, the DAV and
/// view/snap byte surfaces alike. Layered outermost so even the auth
/// gate's early rejections wear them. No HSTS (plain-HTTP LAN is the
/// deployment, D70).
pub(crate) async fn security_headers(req: Request, next: Next) -> Response {
    let mut resp = next.run(req).await;
    let h = resp.headers_mut();
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(CSP),
    );
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    h.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    h.insert(
        HeaderName::from_static("cross-origin-opener-policy"),
        HeaderValue::from_static("same-origin"),
    );
    // CORP is browser-enforced only: it stops OTHER origins' pages from
    // embedding our bytes (Spectre-class exfil); curl/ureq/emulator
    // frontends and every other non-browser consumer ignore it, so the
    // API/DAV/NFS-adjacent surfaces lose nothing.
    h.insert(
        HeaderName::from_static("cross-origin-resource-policy"),
        HeaderValue::from_static("same-origin"),
    );
    // Belt for user agents that predate CSP frame-ancestors (which
    // supersedes XFO everywhere modern).
    h.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    // The UI requests no powerful browser feature; deny the lot so a
    // future injection or embedding can't quietly turn one on.
    h.insert(
        HeaderName::from_static("permissions-policy"),
        HeaderValue::from_static("camera=(), geolocation=(), microphone=(), payment=(), usb=()"),
    );
    resp
}

// ---- Fetch-Metadata CSRF (D70) ----

/// The 403 body for every CSRF rejection.
pub(crate) const CSRF_REJECTED: &str = "cross-origin request rejected (D70)";

/// Decide whether a request passes the cross-origin gate. Pure over
/// (method, headers) so the whole matrix is unit-testable.
///
/// Decision table (Go's `http.CrossOriginProtection`):
///   1. GET/HEAD/OPTIONS → allow (safe methods; CSRF is about state).
///   2. `Authorization: Bearer` present → allow: a bearer token is not
///      an ambient credential — a hostile page cannot attach that
///      header cross-origin without a CORS preflight we never grant, so
///      its presence proves a deliberate client. (Covers the CLI and
///      scripted API users behind proxies that add fetch metadata.)
///   3. `Sec-Fetch-Site: same-origin` or `none` (user-typed URL /
///      bookmark) → allow; any other value (`cross-site`, `same-site` —
///      still another origin — or future unknowns) → reject.
///   4. No `Sec-Fetch-Site` (pre-2023 browser or non-browser): if
///      `Origin` is present, allow only when it names the same
///      host:port the request was sent to (`Host`); `Origin: null`
///      (sandboxed iframe) and malformed origins reject.
///   5. Neither header → allow: curl/ureq/WebDAV clients (PROPFIND et
///      al. land here) are not browsers and carry no ambient cookie.
///
/// The SPA's login/invite-accept POSTs are `fetch()` calls from the
/// served page itself — `Sec-Fetch-Site: same-origin` — so no redirect
/// flow ever needs a cross-site exemption.
pub(crate) fn csrf_check(method: &Method, headers: &HeaderMap) -> Result<(), &'static str> {
    if matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS) {
        return Ok(());
    }
    if headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("Bearer "))
    {
        return Ok(());
    }
    match headers.get("sec-fetch-site").and_then(|v| v.to_str().ok()) {
        Some("same-origin" | "none") => Ok(()),
        Some(_) => Err(CSRF_REJECTED),
        None => {
            let Some(origin) = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) else {
                return Ok(());
            };
            // No Host header (malformed HTTP/1.1) fails the comparison:
            // an Origin-bearing request that we can't verify rejects.
            let host = headers
                .get(header::HOST)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("");
            if origin_matches_host(origin, host) {
                Ok(())
            } else {
                Err(CSRF_REJECTED)
            }
        }
    }
}

/// Does `Origin` name the same authority as the `Host` header?
/// Scheme-insensitive (a TLS-terminating proxy in front of the plain-
/// http daemon makes the browser say `https://…` while the connection
/// is http) but port-aware: both sides normalize by dropping a default
/// port (80/443) and lowercasing, so `http://host:80` matches `host`
/// while `http://host:8080` never matches `host:9090`.
fn origin_matches_host(origin: &str, host: &str) -> bool {
    // `Origin: null` (sandboxed iframe, some redirects) and anything
    // that isn't an http(s) origin never match — same as Go, where an
    // unparseable Origin rejects.
    let Some((scheme, hostport)) = origin.split_once("://") else {
        return false;
    };
    if !scheme.eq_ignore_ascii_case("http") && !scheme.eq_ignore_ascii_case("https") {
        return false;
    }
    // A serialized origin is exactly scheme://host[:port] — a path,
    // userinfo, or query means someone hand-built the header; reject.
    if hostport.contains(['/', '?', '#', '@']) {
        return false;
    }
    match (normalize_hostport(hostport), normalize_hostport(host)) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    }
}

/// Canonicalize `host[:port]` for comparison: lowercase the host, parse
/// the port numerically (so `:0080` == `:80`), and drop it when it is a
/// default (80/443 — scheme-insensitive comparison needs both treated
/// as "default class"). IPv6 literals keep their brackets so
/// `[::1]:8080` can never collide with the address `[::1:8080]`.
fn normalize_hostport(hostport: &str) -> Option<String> {
    let (host, port) = split_hostport(hostport)?;
    if host.is_empty() {
        return None;
    }
    let host = host.to_ascii_lowercase();
    match port {
        None => Some(host),
        Some(p) => {
            let n: u16 = p.parse().ok()?;
            if n == 80 || n == 443 {
                Some(host)
            } else {
                Some(format!("{host}:{n}"))
            }
        }
    }
}

/// Split an authority into (host, port), IPv6-bracket-aware. The host
/// keeps its brackets when bracketed. Returns None for shapes that are
/// not a valid authority (garbage after `]`, empty port).
fn split_hostport(hostport: &str) -> Option<(&str, Option<&str>)> {
    if hostport.starts_with('[') {
        // IPv6 literal: [addr] or [addr]:port
        let end = hostport.find(']')?;
        let (host, tail) = hostport.split_at(end + 1);
        return match tail {
            "" => Some((host, None)),
            _ => tail.strip_prefix(':').map(|p| (host, Some(p))),
        };
    }
    match hostport.rsplit_once(':') {
        // A second colon in the head means a bare (bracket-less) IPv6
        // literal — invalid in Host/Origin; treat the whole string as
        // the host so it can only match its own exact spelling.
        Some((h, _)) if h.contains(':') => Some((hostport, None)),
        Some((h, p)) => Some((h, Some(p))),
        None => Some((hostport, None)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (k, v) in pairs {
            map.append(
                HeaderName::from_bytes(k.as_bytes()).expect("name"),
                HeaderValue::from_str(v).expect("value"),
            );
        }
        map
    }

    fn allowed(method: &Method, pairs: &[(&str, &str)]) -> bool {
        csrf_check(method, &headers(pairs)).is_ok()
    }

    #[test]
    fn safe_methods_always_pass() {
        for m in [Method::GET, Method::HEAD, Method::OPTIONS] {
            assert!(allowed(&m, &[("sec-fetch-site", "cross-site")]), "{m}");
            assert!(
                allowed(&m, &[("origin", "http://evil.example")]),
                "{m} with mismatched origin"
            );
        }
    }

    #[test]
    fn sec_fetch_site_governs_unsafe_methods() {
        let post = Method::POST;
        let propfind = Method::from_bytes(b"PROPFIND").expect("extension method");
        for m in [
            &post,
            &Method::PUT,
            &Method::DELETE,
            &Method::PATCH,
            &propfind,
        ] {
            assert!(allowed(m, &[("sec-fetch-site", "same-origin")]), "{m}");
            // user-typed URL / bookmark navigation (matches Go)
            assert!(allowed(m, &[("sec-fetch-site", "none")]), "{m}");
            assert!(!allowed(m, &[("sec-fetch-site", "cross-site")]), "{m}");
            // same-site is still another origin
            assert!(!allowed(m, &[("sec-fetch-site", "same-site")]), "{m}");
            // unknown future values fail closed
            assert!(!allowed(m, &[("sec-fetch-site", "sideways")]), "{m}");
        }
    }

    #[test]
    fn bearer_requests_are_exempt() {
        // Authorization: Bearer cannot be attached cross-origin without
        // a CORS preflight we never grant — no ambient credential.
        assert!(allowed(
            &Method::POST,
            &[
                ("sec-fetch-site", "cross-site"),
                ("authorization", "Bearer tok123"),
            ],
        ));
        assert!(allowed(
            &Method::DELETE,
            &[
                ("origin", "http://evil.example"),
                ("host", "daemon.lan:7777"),
                ("authorization", "Bearer tok123"),
            ],
        ));
        // ...but only Bearer: other Authorization schemes ARE ambient
        // (browsers replay Basic credentials).
        assert!(!allowed(
            &Method::POST,
            &[
                ("sec-fetch-site", "cross-site"),
                ("authorization", "Basic dXNlcjpwdw=="),
            ],
        ));
    }

    #[test]
    fn missing_header_fallback_uses_origin_vs_host() {
        // neither header: not a browser (curl/ureq/DAV clients) → allow
        assert!(allowed(&Method::POST, &[]));
        assert!(allowed(&Method::from_bytes(b"MKCOL").unwrap(), &[]));

        // Origin matching Host → allow
        assert!(allowed(
            &Method::POST,
            &[
                ("origin", "http://127.0.0.1:7777"),
                ("host", "127.0.0.1:7777")
            ],
        ));
        // mismatch → reject
        assert!(!allowed(
            &Method::POST,
            &[
                ("origin", "http://evil.example"),
                ("host", "127.0.0.1:7777")
            ],
        ));
        // Origin: null (sandboxed iframe) → reject
        assert!(!allowed(
            &Method::POST,
            &[("origin", "null"), ("host", "127.0.0.1:7777")],
        ));
        // Origin present but no Host to verify against → reject
        assert!(!allowed(
            &Method::POST,
            &[("origin", "http://127.0.0.1:7777")],
        ));
        // Sec-Fetch-Site wins over a matching Origin when both present
        assert!(!allowed(
            &Method::POST,
            &[
                ("sec-fetch-site", "cross-site"),
                ("origin", "http://a.example"),
                ("host", "a.example"),
            ],
        ));
    }

    #[test]
    fn origin_host_comparison_edges() {
        let m = |o: &str, h: &str| origin_matches_host(o, h);

        // defaults normalize away, case-insensitively, scheme-insensitively
        assert!(m("http://example.com", "example.com"));
        assert!(m("http://example.com:80", "example.com"));
        assert!(m("http://example.com:0080", "example.com"));
        assert!(m("http://Example.COM", "example.com"));
        assert!(m("https://example.com", "example.com")); // TLS proxy in front
        assert!(m("https://example.com:443", "example.com"));
        assert!(m("http://example.com:8080", "EXAMPLE.com:8080"));

        // ports are otherwise load-bearing
        assert!(!m("http://example.com:8080", "example.com"));
        assert!(!m("http://example.com", "example.com:8080"));
        assert!(!m("http://example.com:8080", "example.com:9090"));

        // host mismatch, subdomains, suffix tricks
        assert!(!m("http://evil.example", "example.com"));
        assert!(!m("http://sub.example.com", "example.com"));
        assert!(!m("http://example.com.evil.net", "example.com"));

        // IPv6 literals: bracket-aware port split, no bracket collision
        assert!(m("http://[::1]:7777", "[::1]:7777"));
        assert!(m("http://[::1]", "[::1]"));
        assert!(m("http://[::1]:80", "[::1]"));
        assert!(m("http://[2001:DB8::1]:7777", "[2001:db8::1]:7777"));
        assert!(!m("http://[::1]:7777", "[::1]:8888"));
        assert!(!m("http://[::1:8080]", "[::1]:8080")); // address vs port
        assert!(m("http://127.0.0.1:7777", "127.0.0.1:7777"));

        // malformed / non-http origins never match
        assert!(!m("null", "example.com"));
        assert!(!m("example.com", "example.com")); // no scheme
        assert!(!m("ftp://example.com", "example.com"));
        assert!(!m("http://example.com/path", "example.com"));
        assert!(!m("http://user@example.com", "example.com"));
        assert!(!m("http://example.com:notaport", "example.com"));
        assert!(!m("http://example.com:", "example.com"));
        assert!(!m("http://", "example.com"));
        assert!(!m("http://[::1", "[::1")); // unclosed bracket
        assert!(!m("http://[::1]junk", "[::1]"));
    }

    #[test]
    fn csp_is_the_pinned_string() {
        assert_eq!(
            CSP,
            "default-src 'self'; script-src 'self'; style-src 'self'; \
             img-src 'self' data:; font-src 'self'; connect-src 'self'; \
             object-src 'none'; frame-ancestors 'none'; base-uri 'none'; \
             form-action 'self'"
        );
    }
}
