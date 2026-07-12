//! Runtime response compression (D78): gzip/brotli negotiated from
//! `Accept-Encoding`, applied ONLY to text-shaped responses.
//!
//! The predicate is an allowlist, not a denylist. The daemon's byte
//! surfaces — /view, /snap, DAV file bodies, blob downloads — serve
//! verified content as `application/octet-stream`, usually ROM/archive
//! data that is already entropy-dense and often under Range semantics;
//! re-encoding it would burn CPU for nothing and an encoded 206 is a
//! coherence bug waiting for a client. Only content types that are
//! text by construction opt in, and anything carrying `Content-Range`
//! never compresses regardless of type.

use axum::body::HttpBody;
use axum::http::header;
use tower_http::compression::CompressionLayer;
use tower_http::compression::predicate::{And, Predicate, SizeAbove};

/// Below this many bytes the gzip/brotli framing outweighs the win.
const MIN_BYTES: u16 = 256;

/// The text-by-construction content types this daemon actually emits:
/// /v1 JSON, the SPA shell + assets (html/css/js/svg), the /view and
/// /snap HTML listings, and DAV's PROPFIND XML.
const TEXTUAL: [&str; 5] = [
    "application/json",
    "application/javascript",
    "application/xml",
    "image/svg+xml",
    "text/",
];

#[derive(Clone)]
pub(crate) struct TextOnly;

impl Predicate for TextOnly {
    fn should_compress<B>(&self, response: &axum::http::Response<B>) -> bool
    where
        B: HttpBody,
    {
        if response.headers().contains_key(header::CONTENT_RANGE) {
            return false;
        }
        let content_type = response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        TEXTUAL.iter().any(|p| content_type.starts_with(p))
    }
}

/// The layer http.rs wires into the router. tower-http handles the
/// negotiation (`Accept-Encoding`), skips already-encoded responses,
/// and stamps `Vary: Accept-Encoding` on everything it might encode.
pub(crate) fn layer() -> CompressionLayer<And<TextOnly, SizeAbove>> {
    CompressionLayer::new().compress_when(TextOnly.and(SizeAbove::new(MIN_BYTES)))
}
