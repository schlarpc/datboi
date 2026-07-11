//! Owner-only admin management (D30/D68): users, invites, grants,
//! session revocation. Web-minted invites answer a fragment URL
//! (`/invite#<token>`) so the token never appears in server logs or
//! Referer headers; the database stores only `blake3(token)`, same as
//! the CLI mint path.

// Same rationale as http.rs: fallible steps short-circuit with the
// error Response itself.
#![allow(clippy::result_large_err)]

use std::collections::HashMap;
use std::sync::Arc;

use axum::Extension;
use axum::Json;
use axum::extract::{Path as UrlPath, State};
use axum::http::StatusCode;
use axum::response::Response;
use datboi_core::hash::Blake3;
use datboi_index::Role;
use serde_json::json;

use crate::App;
use crate::api::{err, hex, require_owner};
use crate::auth::{self, Caller};
use crate::http::{json_response, run_blocking};

fn internal(e: impl std::fmt::Display) -> Response {
    err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string())
}

fn lock_db(app: &App) -> std::sync::MutexGuard<'_, datboi_index::Db> {
    app.db
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

// ---- GET /v1/admin/users ----

pub(crate) async fn users(
    State(app): State<Arc<App>>,
    Extension(caller): Extension<Caller>,
) -> Response {
    run_blocking(move || {
        require_owner(&caller)?;
        let now = auth::now_unix();
        let db = lock_db(&app);
        let users = db.list_users().map_err(internal)?;
        let mut grants: HashMap<i64, Vec<String>> = HashMap::new();
        for (user_id, view) in db.all_grants().map_err(internal)? {
            grants.entry(user_id).or_default().push(view);
        }
        let mut sessions: HashMap<i64, u64> = HashMap::new();
        for session in db.list_sessions().map_err(internal)? {
            if session.expires_at > now {
                *sessions.entry(session.user_id).or_default() += 1;
            }
        }
        let by_id: HashMap<i64, &str> = users
            .iter()
            .map(|u| (u.user_id, u.username.as_str()))
            .collect();
        // Pending invites only: consumed ones live on as the user's
        // provenance, expired ones are dead weight the UI need not show.
        let invites: Vec<_> = db
            .list_invites()
            .map_err(internal)?
            .into_iter()
            .filter(|invite| invite.used_by.is_none() && invite.expires_at > now)
            .map(|invite| {
                json!({
                    "token_hash": hex(&invite.token_hash),
                    "role": auth::role_str(invite.role),
                    "expires_at": invite.expires_at,
                    "created_by": invite.created_by.and_then(|id| by_id.get(&id)),
                })
            })
            .collect();
        let users: Vec<_> = users
            .iter()
            .map(|user| {
                json!({
                    "username": user.username,
                    "role": auth::role_str(user.role),
                    "created_at": user.created_at,
                    "grants": grants.get(&user.user_id).cloned().unwrap_or_default(),
                    "sessions": sessions.get(&user.user_id).copied().unwrap_or(0),
                })
            })
            .collect();
        Ok(json_response(
            StatusCode::OK,
            &json!({"users": users, "invites": invites}),
        ))
    })
    .await
}

// ---- POST /v1/admin/invites ----

const DAY_SECS: i64 = 24 * 60 * 60;

pub(crate) async fn invite_create(
    State(app): State<Arc<App>>,
    Extension(caller): Extension<Caller>,
    Json(body): Json<serde_json::Value>,
) -> Response {
    run_blocking(move || {
        require_owner(&caller)?;
        let role = match body.get("role").and_then(serde_json::Value::as_str) {
            None | Some("friend") => Role::Friend,
            Some("owner") => Role::Owner,
            Some(_) => return Err(err(StatusCode::BAD_REQUEST, "role must be owner or friend")),
        };
        // D68 default: 7 days. Bounded — an effectively-eternal invite
        // is a standing credential, which is what invites exist to avoid.
        let days = body
            .get("expires_days")
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(7);
        if !(1..=365).contains(&days) {
            return Err(err(
                StatusCode::BAD_REQUEST,
                "expires_days must be between 1 and 365",
            ));
        }
        let token = auth::mint_token()
            .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, &format!("entropy: {e}")))?;
        let expires_at = auth::now_unix() + days * DAY_SECS;
        // Web mints record the minting user; loopback (CLI-equivalent
        // shell access) mints with no user row, same as `datboi user
        // invite`.
        let created_by = match &caller {
            Caller::User { user_id, .. } => Some(*user_id),
            _ => None,
        };
        let db = lock_db(&app);
        db.mint_invite(&auth::token_hash(&token), created_by, role, expires_at)
            .map_err(internal)?;
        Ok(json_response(
            StatusCode::OK,
            &json!({
                "url_path": format!("/invite#{token}"),
                "expires_at": expires_at,
            }),
        ))
    })
    .await
}

// ---- DELETE /v1/admin/invites/{token_hash_hex} ----

pub(crate) async fn invite_delete(
    State(app): State<Arc<App>>,
    Extension(caller): Extension<Caller>,
    UrlPath(token_hash_hex): UrlPath<String>,
) -> Response {
    run_blocking(move || {
        require_owner(&caller)?;
        // The stored key is 32 bytes; Blake3's hex parser is exactly
        // the 64-hex-chars decoder this needs.
        let hash: Blake3 = token_hash_hex
            .parse()
            .map_err(|_| err(StatusCode::BAD_REQUEST, "not a token hash"))?;
        let db = lock_db(&app);
        if db.delete_invite(&hash.0).map_err(internal)? {
            Ok(json_response(StatusCode::OK, &json!({"ok": true})))
        } else {
            Err(err(StatusCode::NOT_FOUND, "no such pending invite"))
        }
    })
    .await
}

// ---- POST /v1/admin/grants + DELETE /v1/admin/grants/{user}/{view} ----

fn user_id_by_name(db: &datboi_index::Db, username: &str) -> Result<i64, Response> {
    db.user_by_name(username)
        .map_err(internal)?
        .map(|user| user.user_id)
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "no such user"))
}

pub(crate) async fn grant_create(
    State(app): State<Arc<App>>,
    Extension(caller): Extension<Caller>,
    Json(body): Json<serde_json::Value>,
) -> Response {
    run_blocking(move || {
        require_owner(&caller)?;
        let field = |key: &str| {
            body.get(key)
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| err(StatusCode::BAD_REQUEST, &format!("missing field {key:?}")))
        };
        let username = field("username")?;
        let view = field("view")?;
        let db = lock_db(&app);
        let user_id = user_id_by_name(&db, username)?;
        // A grant on a view that exists nowhere (no tag, no definition)
        // is a typo, not policy — refuse it.
        let tagged = db
            .get_tag(&format!("view/{view}"))
            .map_err(internal)?
            .is_some();
        let defined = datboi_catalog::get_view(&db, view)
            .map_err(internal)?
            .is_some();
        if !tagged && !defined {
            return Err(err(StatusCode::NOT_FOUND, "no such view"));
        }
        db.grant_view(user_id, view).map_err(internal)?;
        Ok(json_response(StatusCode::OK, &json!({"ok": true})))
    })
    .await
}

pub(crate) async fn grant_delete(
    State(app): State<Arc<App>>,
    Extension(caller): Extension<Caller>,
    UrlPath((username, view)): UrlPath<(String, String)>,
) -> Response {
    run_blocking(move || {
        require_owner(&caller)?;
        let db = lock_db(&app);
        let user_id = user_id_by_name(&db, &username)?;
        if db.revoke_view(user_id, &view).map_err(internal)? {
            Ok(json_response(StatusCode::OK, &json!({"ok": true})))
        } else {
            Err(err(StatusCode::NOT_FOUND, "no such grant"))
        }
    })
    .await
}

// ---- DELETE /v1/admin/sessions/{username} ----

pub(crate) async fn sessions_delete(
    State(app): State<Arc<App>>,
    Extension(caller): Extension<Caller>,
    UrlPath(username): UrlPath<String>,
) -> Response {
    run_blocking(move || {
        require_owner(&caller)?;
        let db = lock_db(&app);
        let user_id = user_id_by_name(&db, &username)?;
        let revoked = db.delete_sessions_for_user(user_id).map_err(internal)?;
        Ok(json_response(StatusCode::OK, &json!({"revoked": revoked})))
    })
    .await
}
