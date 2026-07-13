//! Browser emulator cores (D84, docs/88-emulation.md): the third wasm
//! lane, embedded like the web dist (D66/D67 — `DATBOI_EMU_DS` from the
//! flake or build.rs's `nix build .#emu-ds` dev fallback) and served as
//! static assets under `/emu/{core}/`. The web app's Play screen spawns
//! the core's worker.js and speaks postMessage to it — that boundary is
//! also the GPL-3 line: everything under /emu/nds is dust-derived,
//! everything importing this module is datboi.

use axum::body::Body;
use axum::extract::Path as UrlPath;
use axum::http::{Method, StatusCode, header};
use axum::response::Response;

/// The nix-built emu-ds asset: descriptor.json + worker.js + pkg/
/// (wasm + wasm-bindgen glue), plus the crate's bare test page —
/// unlinked from the UI, but a served debugging surface beats one that
/// only exists in `nix build` output.
static EMU_DS: include_dir::Dir<'static> = include_dir::include_dir!("$DATBOI_EMU_DS");

/// GET /emu/{core}/{*path} — one arm per shipped core. A miss is a
/// plain 404: nothing under /emu belongs to the SPA router.
pub(crate) async fn emu_path(
    method: Method,
    UrlPath((core, path)): UrlPath<(String, String)>,
) -> Response {
    if method != Method::GET && method != Method::HEAD {
        return Response::builder()
            .status(StatusCode::METHOD_NOT_ALLOWED)
            .header(header::ALLOW, "GET, HEAD")
            .body(Body::empty())
            .expect("static headers");
    }
    let dir = match core.as_str() {
        "nds" => &EMU_DS,
        _ => return crate::http::text(StatusCode::NOT_FOUND, "no such core"),
    };
    let Some(file) = dir.get_file(&path) else {
        return crate::http::text(StatusCode::NOT_FOUND, "no such path");
    };
    let bytes = file.contents();
    let body = if method == Method::HEAD {
        Body::empty()
    } else {
        Body::from(bytes)
    };
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, crate::web::content_type(&path))
        // Names aren't content-hashed (wasm-bindgen output is stable),
        // so revalidate like index.html. The planned cache-forever
        // story arrives with hashed asset names, not before.
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::CONTENT_LENGTH, bytes.len())
        .body(body)
        .expect("static headers")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The Play screen's boot sequence names these three files; a
    /// flake/wasm-bindgen change that renames or drops one goes red
    /// here instead of at first click.
    #[test]
    fn asset_contains_the_protocol_surface() {
        for path in ["descriptor.json", "worker.js", "pkg/datboi_emu_ds.js"] {
            assert!(EMU_DS.get_file(path).is_some(), "missing {path}");
        }
        assert!(
            EMU_DS
                .get_file("pkg/datboi_emu_ds_bg.wasm")
                .is_some_and(|f| f.contents().starts_with(b"\0asm")),
            "wasm module missing or not wasm"
        );
    }
}
