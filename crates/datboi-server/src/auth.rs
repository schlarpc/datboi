//! Auth v1 (D30/D68): identity resolution, enforcement, and the
//! `/v1/auth/*` endpoints.
//!
//! Identities are `user` rows (argon2id password hashes, role ∈
//! {owner, friend}). Tokens — invite, session, bearer — are 32 random
//! bytes rendered URL-safe; the database stores only `blake3(token)`,
//! so a stolen state.db mints nothing. Browsers carry the session in
//! the `datboi_session` cookie; tools send the same token as
//! `Authorization: Bearer`. Loopback connections are implicitly owner:
//! a local shell already owns the daemon's files, so cookie-auth on
//! 127.0.0.1 would be theater.

// Same rationale as http.rs: fallible steps short-circuit with the
// error Response itself.
#![allow(clippy::result_large_err)]

use std::net::IpAddr;
use std::sync::{Arc, LazyLock};
use std::time::{SystemTime, UNIX_EPOCH};

use argon2::Argon2;
use argon2::password_hash::{PasswordHash, PasswordHasher as _, PasswordVerifier as _, SaltString};
use axum::extract::{ConnectInfo, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::Next;
use axum::response::Response;
use datboi_api::{InviteAcceptRequest, LoginRequest, OkResponse, SessionResponse, WhoamiResponse};
use datboi_core::hash::Blake3;
use datboi_index::{Db, InviteOutcome, Role};

use crate::App;
use crate::api::err;
use crate::http::{ApiJson, json_response, run_blocking, text};

/// The browser session cookie (D68).
pub(crate) const SESSION_COOKIE: &str = "datboi_session";
/// Sessions (cookie and bearer alike) live 30 days (D68).
pub(crate) const SESSION_TTL_SECS: i64 = 30 * 24 * 60 * 60;

// ---- tokens ----

/// Mint a fresh token: 32 random bytes as URL-safe base64, no padding
/// (43 chars). Public because the CLI mints invites and bearer tokens
/// against the database directly (D68: local shell access = admin).
///
/// # Errors
/// OS entropy failure.
pub fn mint_token() -> Result<String, getrandom::Error> {
    let mut bytes = [0u8; 32];
    getrandom::getrandom(&mut bytes)?;
    Ok(b64url(&bytes))
}

/// The stored key for any token: blake3 of the token STRING bytes (the
/// exact characters the client presents — no decode step to disagree
/// about).
#[must_use]
pub fn token_hash(token: &str) -> [u8; 32] {
    Blake3::compute(token.as_bytes()).0
}

/// URL-safe base64 without padding (RFC 4648 §5). Hand-rolled: one
/// fixed-size encode does not justify a dependency.
fn b64url(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..=chunk.len() {
            out.push(ALPHABET[(n >> (18 - 6 * i)) as usize & 0x3f] as char);
        }
    }
    out
}

pub(crate) fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

// ---- identity resolution ----

/// Who is making this request, resolved once per request by the
/// [`gate`] middleware and carried as a request extension.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Caller {
    /// Loopback peer: implicitly the owner (D68), no user row needed.
    Local,
    /// A session/bearer token that resolved to a live user row.
    User {
        user_id: i64,
        username: String,
        role: Role,
        via: Via,
    },
    Anonymous,
}

/// How a [`Caller::User`] presented its token (whoami reports it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Via {
    Session,
    Bearer,
}

impl Caller {
    /// Owners (and loopback) pass every ACL.
    pub(crate) fn is_owner(&self) -> bool {
        match self {
            Self::Local => true,
            Self::User { role, .. } => *role == Role::Owner,
            Self::Anonymous => false,
        }
    }
}

/// Resolve a request's identity: loopback is owner; otherwise the
/// session cookie, then a bearer token, may name a live session. Takes
/// the peer address as a parameter so tests can exercise the
/// non-loopback matrix without a non-loopback listener. Database
/// errors resolve to Anonymous — fail closed.
pub(crate) fn resolve(db: &Db, peer: IpAddr, headers: &HeaderMap, now: i64) -> Caller {
    if peer.is_loopback() {
        return Caller::Local;
    }
    let candidates = session_cookie(headers)
        .map(|t| (t, Via::Session))
        .into_iter()
        .chain(bearer_token(headers).map(|t| (t, Via::Bearer)));
    for (token, via) in candidates {
        if let Ok(Some((user_id, username, role))) = db.session_user(&token_hash(token), now) {
            return Caller::User {
                user_id,
                username,
                role,
                via,
            };
        }
    }
    Caller::Anonymous
}

/// Find the session cookie by hand — one name in a `;`-separated list
/// is not worth a cookie dependency.
fn session_cookie(headers: &HeaderMap) -> Option<&str> {
    for value in headers.get_all(header::COOKIE) {
        let Ok(s) = value.to_str() else { continue };
        for pair in s.split(';') {
            if let Some(v) = pair
                .trim()
                .strip_prefix(SESSION_COOKIE)
                .and_then(|rest| rest.strip_prefix('='))
            {
                return Some(v);
            }
        }
    }
    None
}

fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
        .map(str::trim)
}

// ---- enforcement ----

/// Route classes (D68). Non-loopback: `/healthz`, the auth endpoints,
/// and the static UI are open; `/v1`, `/view`, `/snap` require a valid
/// identity; DAV stays loopback-only in M5 (authenticated DAV is a
/// recorded open question, not half-shipped). NFS is a separate
/// listener and untouched here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Access {
    Open,
    Authenticated,
    LoopbackOnly,
}

pub(crate) fn route_access(path: &str) -> Access {
    if path == "/healthz" || under(path, "/v1/auth") {
        return Access::Open;
    }
    if under(path, "/v1") || under(path, "/view") || under(path, "/snap") {
        return Access::Authenticated;
    }
    if under(path, "/dav") {
        return Access::LoopbackOnly;
    }
    // Everything else is the embedded web UI's URL space (D67), open so
    // an anonymous browser can reach the login/invite pages.
    Access::Open
}

/// Segment-accurate prefix: `/view` and `/view/...`, not `/viewfoo`.
fn under(path: &str, prefix: &str) -> bool {
    path.strip_prefix(prefix)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

/// Router-wide middleware: resolve the caller once, stash it as an
/// extension, and enforce the route class.
pub(crate) async fn gate(
    State(app): State<Arc<App>>,
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
    mut req: Request,
    next: Next,
) -> Response {
    // Fetch-Metadata CSRF (D70) first: header-only, no DB. It applies
    // to loopback callers too — DNS rebinding hands a hostile page an
    // origin that resolves to 127.0.0.1, and loopback-is-owner (D68)
    // makes that ambient authority; this check is what closes it.
    if let Err(msg) = crate::hardening::csrf_check(req.method(), req.headers()) {
        return err(StatusCode::FORBIDDEN, msg);
    }
    let caller = if peer.ip().is_loopback() {
        Caller::Local // no DB touch on the hot local path
    } else {
        let headers = req.headers().clone();
        tokio::task::spawn_blocking(move || {
            let db = app
                .db
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            resolve(&db, peer.ip(), &headers, now_unix())
        })
        .await
        .unwrap_or(Caller::Anonymous)
    };
    match route_access(req.uri().path()) {
        Access::Open => {}
        Access::Authenticated => {
            if caller == Caller::Anonymous {
                return unauthorized("authentication required");
            }
        }
        Access::LoopbackOnly => {
            if caller != Caller::Local {
                return unauthorized("this surface is loopback-only (M5, D68)");
            }
        }
    }
    req.extensions_mut().insert(caller);
    next.run(req).await
}

fn unauthorized(msg: &str) -> Response {
    let mut resp = text(StatusCode::UNAUTHORIZED, msg);
    resp.headers_mut()
        .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
    resp
}

/// May `caller` see view `name`? Owners see everything; friends need a
/// `view_grant` row (D68). Errors deny — fail closed.
pub(crate) fn view_allowed(db: &Db, caller: &Caller, name: &str) -> bool {
    if caller.is_owner() {
        return true;
    }
    let Caller::User { user_id, .. } = caller else {
        return false;
    };
    db.grants_for_user(*user_id)
        .is_ok_and(|grants| grants.iter().any(|g| g == name))
}

/// May `caller` fetch snapshot `hash`? Friends may reach exactly the
/// snapshots their granted views currently point at (D33: snapshots
/// are what friends consume; historical hashes stay owner-only).
pub(crate) fn snap_allowed(db: &Db, caller: &Caller, snapshot: &Blake3) -> bool {
    if caller.is_owner() {
        return true;
    }
    let Caller::User { user_id, .. } = caller else {
        return false;
    };
    let Ok(grants) = db.grants_for_user(*user_id) else {
        return false;
    };
    grants.iter().any(|name| {
        db.get_tag(&format!("view/{name}"))
            .is_ok_and(|tag| tag.as_ref() == Some(snapshot))
    })
}

// ---- password hashing (argon2id, default params) ----

fn hash_password(password: &str) -> Result<String, String> {
    let mut salt_bytes = [0u8; 16];
    getrandom::getrandom(&mut salt_bytes).map_err(|e| e.to_string())?;
    let salt = SaltString::encode_b64(&salt_bytes).map_err(|e| e.to_string())?;
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| e.to_string())
}

fn verify_password(password: &str, phc: &str) -> bool {
    PasswordHash::new(phc).is_ok_and(|parsed| {
        Argon2::default()
            .verify_password(password.as_bytes(), &parsed)
            .is_ok()
    })
}

/// Verified against when the username doesn't exist, so "no such user"
/// and "wrong password" cost the same wall-clock time.
static DUMMY_HASH: LazyLock<String> =
    LazyLock::new(|| hash_password("decoy").expect("hashing a constant"));

// ---- /v1/auth/* handlers ----

/// GET /v1/auth/whoami — open: answers `authenticated: false` instead
/// of 401 so the SPA can probe without special-casing errors.
pub(crate) async fn whoami(req: Request) -> Response {
    let body = match req.extensions().get::<Caller>() {
        Some(Caller::Local) => WhoamiResponse {
            authenticated: true,
            username: None, // loopback has no user row (D68)
            role: Some(datboi_api::Role::Owner),
            via: Some(datboi_api::Via::Loopback),
        },
        Some(Caller::User {
            username,
            role,
            via,
            ..
        }) => WhoamiResponse {
            authenticated: true,
            username: Some(username.clone()),
            role: Some(role_of(*role)),
            via: Some(match via {
                Via::Session => datboi_api::Via::Session,
                Via::Bearer => datboi_api::Via::Bearer,
            }),
        },
        _ => WhoamiResponse {
            authenticated: false,
            username: None,
            role: None,
            via: None,
        },
    };
    json_response(StatusCode::OK, &body)
}

/// Index role → contract role (datboi-api owns the wire spelling, D69).
pub(crate) fn role_of(role: Role) -> datboi_api::Role {
    match role {
        Role::Owner => datboi_api::Role::Owner,
        Role::Friend => datboi_api::Role::Friend,
    }
}

/// Lowercase alnum plus `-`/`_`, 1–32 chars.
fn valid_username(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 32
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
}

/// POST /v1/auth/invite/accept {token, username, password} — open: the
/// invitee has no account yet. Consumes the invite atomically, creates
/// the user with the invite's role, and starts a session.
pub(crate) async fn invite_accept(
    State(app): State<Arc<App>>,
    ApiJson(body): ApiJson<InviteAcceptRequest>,
) -> Response {
    run_blocking(move || {
        let InviteAcceptRequest {
            token,
            username,
            password,
        } = &body;
        if !valid_username(username) {
            return Err(err(
                StatusCode::BAD_REQUEST,
                "username must be 1-32 characters of [a-z0-9_-]",
            ));
        }
        if password.len() < 8 {
            return Err(err(
                StatusCode::BAD_REQUEST,
                "password must be at least 8 characters",
            ));
        }
        // Hash before taking the DB lock: argon2 is deliberately slow.
        let phc = hash_password(password)
            .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, &format!("hashing: {e}")))?;
        let now = now_unix();
        let db = app
            .db
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let outcome = db
            .accept_invite(&token_hash(token), username, &phc, now)
            .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))?;
        match outcome {
            InviteOutcome::Accepted { user_id, role } => {
                session_response(&db, user_id, username, role, now)
            }
            InviteOutcome::InviteInvalid => {
                Err(err(StatusCode::FORBIDDEN, "invalid or expired invite"))
            }
            InviteOutcome::UsernameTaken => {
                Err(err(StatusCode::CONFLICT, "username already taken"))
            }
        }
    })
    .await
}

/// POST /v1/auth/login {username, password}. One uniform failure
/// answer; unknown users still pay for an argon2 verify (timing).
pub(crate) async fn login(
    State(app): State<Arc<App>>,
    ApiJson(body): ApiJson<LoginRequest>,
) -> Response {
    run_blocking(move || {
        let LoginRequest { username, password } = &body;
        let user = {
            let db = app
                .db
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            db.user_by_name(username)
                .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))?
        };
        // Verify OUTSIDE the DB lock — argon2 is ~100ms of pure CPU.
        let ok = match &user {
            Some(u) => verify_password(password, &u.argon2),
            None => {
                let _ = verify_password(password, &DUMMY_HASH);
                false
            }
        };
        let (Some(user), true) = (user, ok) else {
            return Err(err(StatusCode::UNAUTHORIZED, "invalid credentials"));
        };
        let now = now_unix();
        let db = app
            .db
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _ = db.delete_expired_sessions(now); // opportunistic sweep
        session_response(&db, user.user_id, &user.username, user.role, now)
    })
    .await
}

/// POST /v1/auth/logout — deletes the presented session (cookie or
/// bearer) and clears the cookie either way.
pub(crate) async fn logout(State(app): State<Arc<App>>, req: Request) -> Response {
    let presented = session_cookie(req.headers())
        .or_else(|| bearer_token(req.headers()))
        .map(token_hash);
    run_blocking(move || {
        if let Some(hash) = presented {
            let db = app
                .db
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let _ = db.delete_session(&hash);
        }
        let mut resp = json_response(StatusCode::OK, &OkResponse { ok: true });
        resp.headers_mut().insert(
            header::SET_COOKIE,
            HeaderValue::from_str(&cookie_clear()).expect("static cookie"),
        );
        Ok(resp)
    })
    .await
}

/// Mint a session for a fresh login/acceptance and answer with the
/// cookie plus a whoami-shaped body.
fn session_response(
    db: &Db,
    user_id: i64,
    username: &str,
    role: Role,
    now: i64,
) -> Result<Response, Response> {
    let token = mint_token()
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, &format!("entropy: {e}")))?;
    let expires_at = now + SESSION_TTL_SECS;
    db.create_session(&token_hash(&token), user_id, expires_at)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))?;
    let mut resp = json_response(
        StatusCode::OK,
        &SessionResponse {
            authenticated: true,
            username: username.to_owned(),
            role: role_of(role),
            expires_at,
        },
    );
    resp.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_str(&cookie_set(&token)).expect("token is base64url"),
    );
    Ok(resp)
}

fn cookie_set(token: &str) -> String {
    // HttpOnly + SameSite=Lax + Path=/ + 30 d per D68. No `Secure`
    // attribute: the deployment reality is plain HTTP on a LAN — Secure
    // would strand the cookie everywhere the daemon actually runs.
    // Revisit if/when TLS termination guidance ships.
    format!("{SESSION_COOKIE}={token}; HttpOnly; SameSite=Lax; Path=/; Max-Age={SESSION_TTL_SECS}")
}

fn cookie_clear() -> String {
    format!("{SESSION_COOKIE}=; HttpOnly; SameSite=Lax; Path=/; Max-Age=0")
}

#[cfg(test)]
mod tests {
    use super::*;
    use datboi_index::Role;

    const REMOTE: IpAddr = IpAddr::V4(std::net::Ipv4Addr::new(192, 168, 1, 7));
    const LOCAL: IpAddr = IpAddr::V4(std::net::Ipv4Addr::LOCALHOST);

    fn open_db() -> (tempfile::TempDir, Db) {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = Db::open(dir.path()).expect("open");
        (dir, db)
    }

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (k, v) in pairs {
            map.append(
                axum::http::HeaderName::from_bytes(k.as_bytes()).expect("name"),
                HeaderValue::from_str(v).expect("value"),
            );
        }
        map
    }

    #[test]
    fn tokens_are_urlsafe_and_hash_stably() {
        let token = mint_token().expect("entropy");
        assert_eq!(token.len(), 43, "32 bytes, base64url, no padding");
        assert!(
            token
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
            "{token}"
        );
        assert_ne!(token, mint_token().expect("entropy"));
        assert_eq!(token_hash(&token), token_hash(&token));
        // b64url agrees with the RFC 4648 §5 test vectors.
        assert_eq!(b64url(b""), "");
        assert_eq!(b64url(b"f"), "Zg");
        assert_eq!(b64url(b"fo"), "Zm8");
        assert_eq!(b64url(b"foo"), "Zm9v");
        assert_eq!(b64url(&[0xfb, 0xff]), "-_8");
    }

    #[test]
    fn header_parsers_find_the_token() {
        let h = headers(&[("cookie", "a=b; datboi_session=tok123; c=d")]);
        assert_eq!(session_cookie(&h), Some("tok123"));
        let h = headers(&[("cookie", "a=b"), ("cookie", "datboi_session=tok456")]);
        assert_eq!(session_cookie(&h), Some("tok456"));
        assert_eq!(session_cookie(&headers(&[])), None);
        let h = headers(&[("authorization", "Bearer  tok789 ")]);
        assert_eq!(bearer_token(&h), Some("tok789"));
        assert_eq!(
            bearer_token(&headers(&[("authorization", "Basic x")])),
            None
        );
    }

    #[test]
    fn resolve_covers_the_identity_matrix() {
        let (_dir, db) = open_db();
        let user = db.create_user("mika", "$fake$", Role::Friend, 100).unwrap();
        let token = mint_token().unwrap();
        db.create_session(&token_hash(&token), user, 1_000).unwrap();

        // loopback is owner, credentials or not
        assert_eq!(resolve(&db, LOCAL, &headers(&[]), 500), Caller::Local);

        // non-loopback, no credentials
        assert_eq!(resolve(&db, REMOTE, &headers(&[]), 500), Caller::Anonymous);

        // session cookie
        let h = headers(&[("cookie", &format!("datboi_session={token}"))]);
        assert_eq!(
            resolve(&db, REMOTE, &h, 500),
            Caller::User {
                user_id: user,
                username: "mika".into(),
                role: Role::Friend,
                via: Via::Session,
            }
        );

        // same token as bearer
        let h = headers(&[("authorization", &format!("Bearer {token}"))]);
        assert!(matches!(
            resolve(&db, REMOTE, &h, 500),
            Caller::User {
                via: Via::Bearer,
                ..
            }
        ));

        // expired session and garbage token both resolve anonymous
        let h = headers(&[("cookie", &format!("datboi_session={token}"))]);
        assert_eq!(resolve(&db, REMOTE, &h, 2_000), Caller::Anonymous);
        let h = headers(&[("authorization", "Bearer nope")]);
        assert_eq!(resolve(&db, REMOTE, &h, 500), Caller::Anonymous);
    }

    #[test]
    fn route_classes_match_d68() {
        use Access::{Authenticated, LoopbackOnly, Open};
        for (path, expect) in [
            ("/healthz", Open),
            ("/v1/auth/whoami", Open),
            ("/v1/auth/login", Open),
            ("/v1/views", Authenticated),
            ("/v1", Authenticated),
            ("/view/test/", Authenticated),
            ("/view", Authenticated),
            ("/snap/abc123/", Authenticated),
            ("/dav", LoopbackOnly),
            ("/dav/test/file.bin", LoopbackOnly),
            // UI space stays open — including lookalike prefixes that
            // the router sends to the SPA fallback.
            ("/", Open),
            ("/invite", Open),
            ("/viewfoo", Open),
            ("/davfoo", Open),
            ("/assets/index-abc.js", Open),
        ] {
            assert_eq!(route_access(path), expect, "{path}");
        }
    }

    #[test]
    fn view_and_snap_acls_gate_friends() {
        let (_dir, db) = open_db();
        let snap_a = Blake3::compute(b"snap a");
        let snap_b = Blake3::compute(b"snap b");
        db.set_tag("view/gba", &snap_a, 1).unwrap();
        db.set_tag("view/psx", &snap_b, 1).unwrap();

        let owner_id = db.create_user("own", "$x$", Role::Owner, 1).unwrap();
        let friend_id = db.create_user("pal", "$x$", Role::Friend, 1).unwrap();
        db.grant_view(friend_id, "gba").unwrap();

        let owner = Caller::User {
            user_id: owner_id,
            username: "own".into(),
            role: Role::Owner,
            via: Via::Session,
        };
        let friend = Caller::User {
            user_id: friend_id,
            username: "pal".into(),
            role: Role::Friend,
            via: Via::Session,
        };

        for (caller, gba, psx) in [
            (&Caller::Local, true, true),
            (&owner, true, true),
            (&friend, true, false),
            (&Caller::Anonymous, false, false),
        ] {
            assert_eq!(view_allowed(&db, caller, "gba"), gba, "{caller:?}");
            assert_eq!(view_allowed(&db, caller, "psx"), psx, "{caller:?}");
        }

        // snapshots: a friend reaches exactly the granted view's
        // CURRENT snapshot
        assert!(snap_allowed(&db, &friend, &snap_a));
        assert!(!snap_allowed(&db, &friend, &snap_b));
        assert!(snap_allowed(&db, &owner, &snap_b));
        assert!(!snap_allowed(&db, &Caller::Anonymous, &snap_a));
        // ...and loses the old one when the tag flips (D33)
        db.set_tag("view/gba", &snap_b, 2).unwrap();
        assert!(!snap_allowed(&db, &friend, &snap_a));
        assert!(snap_allowed(&db, &friend, &snap_b));
    }

    #[test]
    fn password_hashing_round_trips() {
        let phc = hash_password("hunter22").expect("hash");
        assert!(phc.starts_with("$argon2id$"), "{phc}");
        assert!(verify_password("hunter22", &phc));
        assert!(!verify_password("hunter23", &phc));
        assert!(!verify_password("hunter22", "not a phc string"));
    }

    #[test]
    fn username_validation() {
        assert!(valid_username("mika"));
        assert!(valid_username("a"));
        assert!(valid_username("user_name-2"));
        assert!(!valid_username(""));
        assert!(!valid_username("Mika"));
        assert!(!valid_username("a b"));
        assert!(!valid_username("héllo"));
        assert!(!valid_username(&"a".repeat(33)));
    }
}
