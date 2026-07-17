//! The /v1 operations, as standalone `#[utoipa::path]` contract fns.
//!
//! They live HERE, not on the axum handlers: D69 keeps every utoipa
//! derive inside this crate (datboi-server never links utoipa), and
//! the `OpenApi` derive collects them without reaching into another
//! crate's private handler fns. Each fn body is empty — the operation
//! metadata is the deliverable; datboi-server's routing table is the
//! implementation, and its `v1_router_matches_the_contract` test holds
//! the two together (method+path set equality against this spec).
//!
//! Auth model in one line (D68): loopback peers are implicitly the
//! owner; everyone else presents the `datboi_session` cookie or the
//! same token as `Authorization: Bearer`. Owner-only misses answer
//! 403; view-scoped misses answer 404 exactly like nonexistent views
//! so probing leaks nothing. The middleware's 401 for missing
//! credentials is `text/plain`, predating the JSON error shape.

// The contract fns are never called; the path derive consumes them.
#![allow(dead_code)]

use utoipa::openapi::security::{ApiKey, ApiKeyValue, HttpAuthScheme, HttpBuilder, SecurityScheme};
use utoipa::{Modify, OpenApi, ToSchema};

use crate::{
    AdminUsersResponse, AnalyzerConfigRequest, AnalyzerInfo, AnalyzersResponse, ApiError,
    BlobDetail, BlobsPage, DatImportResponse, EntriesPage,
    EntryDetail, EntryState, GcApplyRequest, GcApplyResponse, GcConfig, GcConfigRequest,
    GcKeepRequest, GrantAddRequest,
    IngestRequest, IngestStartResponse, InviteAcceptRequest, InviteMintRequest, InviteMintResponse,
    JobDetail, JobStartResponse, JobsResponse, LoginRequest, OkResponse, OrphansResponse,
    ResidencyState, ScrubRequest,
    SessionResponse, SessionsRevokedResponse, StorageBreakdown, StorageResponse, SystemsResponse,
    UploadResponse, VerifyStartResponse, ViewDefineRequest, ViewDefineResponse, ViewDetail,
    ViewFilesPage, ViewProfilesResponse, ViewsResponse, WhoamiResponse,
};

/// Marker schema for the minted-image download body: raw octets, not
/// JSON.
#[derive(ToSchema)]
#[schema(value_type = String, format = Binary)]
struct ImageBytes(Vec<u8>);

/// Marker schema for the dat-import upload body: the raw dat file
/// bytes, not JSON (and not multipart — one file IS the request).
#[derive(ToSchema)]
#[schema(value_type = String, format = Binary)]
struct DatBytes(Vec<u8>);

/// Marker schema for the ingest upload body: the raw file bytes,
/// streamed to staging — same one-file-IS-the-request shape as dats.
#[derive(ToSchema)]
#[schema(value_type = String, format = Binary)]
struct RomBytes(Vec<u8>);

// ---- auth (open: the caller has no identity yet by definition) ----

/// Who am I? Open: answers `authenticated: false` instead of 401 so
/// the SPA can probe without special-casing errors.
#[utoipa::path(
    get,
    path = "/v1/auth/whoami",
    tag = "auth",
    responses(
        (status = 200, description = "Caller identity (possibly anonymous)", body = WhoamiResponse),
    ),
)]
fn whoami() {}

/// Accept an invite: consumes it atomically, creates the user with the
/// invite's role, starts a session (Set-Cookie on the response).
#[utoipa::path(
    post,
    path = "/v1/auth/invite/accept",
    tag = "auth",
    request_body = InviteAcceptRequest,
    responses(
        (status = 200, description = "User created, session started (session cookie set)", body = SessionResponse),
        (status = 400, description = "Missing field / bad username / short password", body = ApiError),
        (status = 403, description = "Invalid, expired, or already-consumed invite", body = ApiError),
        (status = 409, description = "Username already taken", body = ApiError),
    ),
)]
fn invite_accept() {}

/// Log in. One uniform 401 for wrong password and unknown user
/// (unknown users still pay for an argon2 verify — timing).
#[utoipa::path(
    post,
    path = "/v1/auth/login",
    tag = "auth",
    request_body = LoginRequest,
    responses(
        (status = 200, description = "Session started (session cookie set)", body = SessionResponse),
        (status = 400, description = "Missing field", body = ApiError),
        (status = 401, description = "Invalid credentials", body = ApiError),
    ),
)]
fn login() {}

/// Log out: deletes the presented session (cookie or bearer) and
/// clears the cookie either way.
#[utoipa::path(
    post,
    path = "/v1/auth/logout",
    tag = "auth",
    responses(
        (status = 200, description = "Session deleted, cookie cleared", body = OkResponse),
    ),
)]
fn logout() {}

// ---- library read models (owner-only) ----

/// Imported dat sources with their audit rollups — the JSON render of
/// `datboi audit`.
#[utoipa::path(
    get,
    path = "/v1/systems",
    tag = "systems",
    security(("session_cookie" = []), ("bearer_token" = [])),
    responses(
        (status = 200, description = "All systems (dat sources) with counts", body = SystemsResponse),
        (status = 403, description = "Owner only", body = ApiError),
    ),
)]
fn systems() {}

/// Page through a system's entries with state filter and
/// case-insensitive substring search.
#[utoipa::path(
    get,
    path = "/v1/systems/{id}/entries",
    tag = "systems",
    security(("session_cookie" = []), ("bearer_token" = [])),
    params(
        ("id" = i64, Path, description = "System id (the `dat_source` surrogate)"),
        ("q" = Option<String>, Query, description = "Case-insensitive substring over entry names; empty = no filter"),
        ("state" = Option<EntryState>, Query, description = "Keep only entries in this state"),
        ("offset" = Option<u64>, Query, description = "Window start (default 0)"),
        ("limit" = Option<u64>, Query, description = "Window size, clamped to 1..=1000 (default 200)"),
    ),
    responses(
        (status = 200, description = "One page; `total` counts the filtered set", body = EntriesPage),
        (status = 400, description = "Bad state/offset/limit value", body = ApiError),
        (status = 403, description = "Owner only", body = ApiError),
        (status = 404, description = "No such system", body = ApiError),
    ),
)]
fn system_entries() {}

/// Entry detail by NAME (unique within a revision), with per-claim
/// hashes, resolved blobs, rebuild routes, and pin lists.
#[utoipa::path(
    get,
    path = "/v1/systems/{id}/entries/{name}",
    tag = "systems",
    security(("session_cookie" = []), ("bearer_token" = [])),
    params(
        ("id" = i64, Path, description = "System id"),
        ("name" = String, Path, description = "Entry name (percent-encoded; may contain slashes)"),
    ),
    responses(
        (status = 200, description = "Entry detail", body = EntryDetail),
        (status = 403, description = "Owner only", body = ApiError),
        (status = 404, description = "No such system or entry", body = ApiError),
    ),
)]
fn system_entry() {}

/// Import a dat file — the same operation as `datboi dat import`. The
/// body is the raw dat bytes in any supported format (Logiqx,
/// ClrMamePro, RomCenter, MAME XML / software list); format detection
/// and the provider/system defaults come from the bytes themselves.
#[utoipa::path(
    post,
    path = "/v1/dats/import",
    tag = "systems",
    security(("session_cookie" = []), ("bearer_token" = [])),
    params(
        ("provider" = Option<String>, Query, description = "Dat source provider override (default derives from the dat header: homepage when it names an org, else author)"),
        ("system" = Option<String>, Query, description = "Dat source system override (default: the dat header's name)"),
    ),
    request_body(content = DatBytes, content_type = "application/octet-stream", description = "The dat file bytes"),
    responses(
        (status = 200, description = "Imported: a new revision of a new or existing source", body = DatImportResponse),
        (status = 400, description = "Empty body or unparseable dat", body = ApiError),
        (status = 403, description = "Owner only", body = ApiError),
        (status = 413, description = "Dat larger than the upload limit (plain-text body — the reject fires below the JSON layer)"),
    ),
)]
fn dat_import() {}

// ---- view authoring (owner-only) ----

/// The built-in constraint profiles a definition may name (owner-only:
/// authoring support, same surface as define).
#[utoipa::path(
    get,
    path = "/v1/view-profiles",
    tag = "views",
    security(("session_cookie" = []), ("bearer_token" = [])),
    responses(
        (status = 200, description = "Built-in constraint profiles", body = ViewProfilesResponse),
        (status = 403, description = "Owner only", body = ApiError),
    ),
)]
fn view_profiles() {}

/// Define or replace a view by name (owner-only; idempotent upsert). The
/// name is the path; the body is the definition. 1G1R and MAME modes are
/// mutually exclusive (400 if both). An unknown profile is a 400.
#[utoipa::path(
    put,
    path = "/v1/views/{name}",
    tag = "views",
    security(("session_cookie" = []), ("bearer_token" = [])),
    params(("name" = String, Path, description = "View name")),
    request_body = ViewDefineRequest,
    responses(
        (status = 200, description = "View defined; stored definition echoed", body = ViewDefineResponse),
        (status = 400, description = "Bad definition (unknown profile, or both selection modes)", body = ApiError),
        (status = 403, description = "Owner only", body = ApiError),
    ),
)]
fn view_define() {}

/// Evaluate a view into a fresh immutable snapshot (owner-only). Long-
/// running, so it starts a background job (D96); poll `GET /v1/jobs/{id}`.
#[utoipa::path(
    post,
    path = "/v1/views/{name}/eval",
    tag = "views",
    security(("session_cookie" = []), ("bearer_token" = [])),
    params(("name" = String, Path, description = "View name")),
    responses(
        (status = 200, description = "Evaluation job started", body = JobStartResponse),
        (status = 403, description = "Owner only", body = ApiError),
        (status = 404, description = "No such view", body = ApiError),
    ),
)]
fn view_eval() {}

/// Mint the view's FAT32 image from its current snapshot (owner-only,
/// D62). Long-running (materialize + mint), so it starts a background
/// job; the minted image downloads at `GET /v1/views/{name}/image`. A
/// never-evaluated view is a 400.
#[utoipa::path(
    post,
    path = "/v1/views/{name}/image",
    tag = "views",
    security(("session_cookie" = []), ("bearer_token" = [])),
    params(("name" = String, Path, description = "View name")),
    responses(
        (status = 200, description = "Mint job started", body = JobStartResponse),
        (status = 400, description = "View has no snapshot yet", body = ApiError),
        (status = 403, description = "Owner only", body = ApiError),
        (status = 404, description = "No such view", body = ApiError),
    ),
)]
fn view_mint() {}

// ---- analyzer config (owner-only, D60/D96) ----

/// The shipped analyzer families and their per-family config.
#[utoipa::path(
    get,
    path = "/v1/analyzers",
    tag = "analyzers",
    security(("session_cookie" = []), ("bearer_token" = [])),
    responses(
        (status = 200, description = "Analyzer families with config", body = AnalyzersResponse),
        (status = 403, description = "Owner only", body = ApiError),
    ),
)]
fn analyzers() {}

/// Set one family's enable state and opaque params (idempotent).
#[utoipa::path(
    put,
    path = "/v1/analyzers/{family}",
    tag = "analyzers",
    security(("session_cookie" = []), ("bearer_token" = [])),
    params(("family" = String, Path, description = "Analyzer family name")),
    request_body = AnalyzerConfigRequest,
    responses(
        (status = 200, description = "Updated family config", body = AnalyzerInfo),
        (status = 400, description = "Unknown family or malformed params hex", body = ApiError),
        (status = 403, description = "Owner only", body = ApiError),
    ),
)]
fn analyzer_config() {}

// ---- views (the friend surface: ACL-filtered, misses look alike) ----

/// Views visible to the caller: owners see everything, friends see
/// exactly their grants (D68).
#[utoipa::path(
    get,
    path = "/v1/views",
    tag = "views",
    security(("session_cookie" = []), ("bearer_token" = [])),
    responses(
        (status = 200, description = "Visible views with snapshot stats", body = ViewsResponse),
    ),
)]
fn views() {}

/// One view with serve endpoints and image-mint status. Denial answers
/// exactly like a miss so probing learns nothing.
#[utoipa::path(
    get,
    path = "/v1/views/{name}",
    tag = "views",
    security(("session_cookie" = []), ("bearer_token" = [])),
    params(("name" = String, Path, description = "View name")),
    responses(
        (status = 200, description = "View detail", body = ViewDetail),
        (status = 404, description = "No such view (or not granted — indistinguishable)", body = ApiError),
    ),
)]
fn view_detail() {}

/// Flat page of the view's CURRENT snapshot manifest — the friend
/// browse surface.
#[utoipa::path(
    get,
    path = "/v1/views/{name}/files",
    tag = "views",
    security(("session_cookie" = []), ("bearer_token" = [])),
    params(
        ("name" = String, Path, description = "View name"),
        ("q" = Option<String>, Query, description = "Case-insensitive substring over full manifest paths"),
        ("offset" = Option<u64>, Query, description = "Window start (default 0)"),
        ("limit" = Option<u64>, Query, description = "Window size, clamped to 1..=1000 (default 200)"),
    ),
    responses(
        (status = 200, description = "One page; `total` counts the filtered set", body = ViewFilesPage),
        (status = 400, description = "Bad offset/limit value", body = ApiError),
        (status = 404, description = "No such view (or not granted)", body = ApiError),
    ),
)]
fn view_files() {}

/// Download the minted FAT32 image (D62). Standard Range/ETag
/// semantics: strong content-hash ETag, `Accept-Ranges: bytes`, 206
/// partial responses, `Content-Disposition: attachment`.
#[utoipa::path(
    get,
    path = "/v1/views/{name}/image",
    tag = "views",
    security(("session_cookie" = []), ("bearer_token" = [])),
    params(("name" = String, Path, description = "View name")),
    responses(
        (status = 200, description = "The image bytes", body = ImageBytes, content_type = "application/octet-stream"),
        (status = 206, description = "Requested range of the image bytes", body = ImageBytes, content_type = "application/octet-stream"),
        (status = 404, description = "No such view, not granted, or no image minted", body = ApiError),
    ),
)]
fn view_image() {}

// ---- ingest (owner-only) ----

/// Stage one file for ingest: the body is the raw bytes, streamed to
/// the store's staging area (never buffered in memory — files run to
/// GBs). The answered token is spent in `POST /v1/ingest`; tokens and
/// staged bytes are ephemeral (daemon restart forgets them, the
/// staging sweep removes them).
#[utoipa::path(
    post,
    path = "/v1/ingest/uploads",
    tag = "ingest",
    security(("session_cookie" = []), ("bearer_token" = [])),
    params(
        ("name" = String, Query, description = "Client-relative file name (`/`-separated, no `..`); report entries wear this name"),
    ),
    request_body(content = RomBytes, content_type = "application/octet-stream", description = "The file bytes"),
    responses(
        (status = 200, description = "Staged; spend the token in POST /v1/ingest", body = UploadResponse),
        (status = 400, description = "Missing/bad name, empty body, or aborted/short body", body = ApiError),
        (status = 403, description = "Owner only", body = ApiError),
        (status = 507, description = "Insufficient store headroom for the declared Content-Length", body = ApiError),
    ),
)]
fn ingest_upload() {}

/// Ingest staged uploads: spends the tokens all-or-nothing, answers a
/// job id immediately, and runs the pipeline in the background — the
/// same hash/claim/archive semantics as `datboi ingest`, one file at a
/// time. Custody over HTTP is always copy: the browser cannot move
/// your originals.
#[utoipa::path(
    post,
    path = "/v1/ingest",
    tag = "ingest",
    security(("session_cookie" = []), ("bearer_token" = [])),
    request_body = IngestRequest,
    responses(
        (status = 200, description = "Job started; poll GET /v1/jobs/{id}", body = IngestStartResponse),
        (status = 400, description = "Missing/empty uploads, or an unknown/expired token", body = ApiError),
        (status = 403, description = "Owner only", body = ApiError),
    ),
)]
fn ingest_start() {}

// ---- storage + jobs (owner-only) ----

/// Storage stats from the blob index — `datboi status` without the
/// filesystem walk.
#[utoipa::path(
    get,
    path = "/v1/storage",
    tag = "storage",
    security(("session_cookie" = []), ("bearer_token" = [])),
    responses(
        (status = 200, description = "Byte accounting + seek quarantine", body = StorageResponse),
        (status = 403, description = "Owner only", body = ApiError),
    ),
)]
fn storage() {}

/// Where the bytes live: per-(namespace, residency) accounting,
/// dat-source attribution (a blob claimed by several sources counts in
/// each; unlinked blobs fold into `(unattributed)`), and the 50
/// largest data blobs.
#[utoipa::path(
    get,
    path = "/v1/storage/breakdown",
    tag = "storage",
    security(("session_cookie" = []), ("bearer_token" = [])),
    responses(
        (status = 200, description = "Aggregate byte attribution", body = StorageBreakdown),
        (status = 403, description = "Owner only", body = ApiError),
    ),
)]
fn storage_breakdown() {}

/// Page through the blob index — the inspector's search surface.
#[utoipa::path(
    get,
    path = "/v1/blobs",
    tag = "storage",
    security(("session_cookie" = []), ("bearer_token" = [])),
    params(
        ("q" = Option<String>, Query, description = "Case-insensitive blake3-hex prefix; empty = no filter"),
        ("ns" = Option<String>, Query, description = "Keep only this namespace: `data` | `meta`"),
        ("residency" = Option<ResidencyState>, Query, description = "Keep only blobs in this residency"),
        ("offset" = Option<u64>, Query, description = "Window start (default 0)"),
        ("limit" = Option<u64>, Query, description = "Window size, clamped to 1..=1000 (default 200)"),
    ),
    responses(
        (status = 200, description = "One page, hash-ordered; `total` counts the filtered set", body = BlobsPage),
        (status = 400, description = "Bad ns/residency/offset/limit value", body = ApiError),
        (status = 403, description = "Owner only", body = ApiError),
    ),
)]
fn blobs() {}

/// One blob, fully explained: digests, provenance, the one-hop recipe
/// DAG around it, the dat claims it satisfies, and the views pinning
/// it.
#[utoipa::path(
    get,
    path = "/v1/blobs/{hash}",
    tag = "storage",
    security(("session_cookie" = []), ("bearer_token" = [])),
    params(("hash" = String, Path, description = "blake3, 64 hex chars (case-insensitive)")),
    responses(
        (status = 200, description = "The inspector card", body = BlobDetail),
        (status = 400, description = "Not a blake3 hex hash", body = ApiError),
        (status = 403, description = "Owner only", body = ApiError),
        (status = 404, description = "No such blob", body = ApiError),
    ),
)]
fn blob_detail() {}

/// Verify one blob right now (D80): re-hash the resident bytes,
/// stamp `verified_at` on match, fail the job with evidence on
/// mismatch. The second pipeline verb to graduate from CLI-only —
/// verification is the product's core promise, and "when was this
/// last checked?" is exactly when the user must be able to act.
/// Resident literals only: a rebuildable blob verifies by replay,
/// which stays CLI.
#[utoipa::path(
    post,
    path = "/v1/blobs/{hash}/verify",
    tag = "storage",
    security(("session_cookie" = []), ("bearer_token" = [])),
    params(("hash" = String, Path, description = "blake3, 64 hex chars (case-insensitive)")),
    responses(
        (status = 202, description = "Verify job started; poll GET /v1/jobs/{id}", body = VerifyStartResponse),
        (status = 400, description = "Not a blake3 hex hash / blob not on disk", body = ApiError),
        (status = 403, description = "Owner only", body = ApiError),
        (status = 404, description = "No such blob", body = ApiError),
    ),
)]
fn blob_verify() {}

/// Rematerialize an evicted/claimed blob by replaying its rebuild route
/// (D25/D27/D96): owner-only, synchronous, idempotent.
#[utoipa::path(
    post,
    path = "/v1/blobs/{hash}/materialize",
    tag = "storage",
    security(("session_cookie" = []), ("bearer_token" = [])),
    params(("hash" = String, Path, description = "blake3, 64 hex chars (case-insensitive)")),
    responses(
        (status = 200, description = "Blob is resident (replayed, or already was)", body = OkResponse),
        (status = 400, description = "Not a blake3 hex hash", body = ApiError),
        (status = 403, description = "Owner only", body = ApiError),
        (status = 404, description = "No such blob", body = ApiError),
        (status = 500, description = "Replay failed (no usable route, or transform error)", body = ApiError),
    ),
)]
fn blob_materialize() {}

/// Raw blob bytes by hash — the fetch half of the D84 BIOS-from-CAS
/// design (emulator cores name accepted system-file hashes; the Play
/// screen asks for each until one answers). Serves through the same
/// verified-range machinery as /view files (ETag = the hash, Range
/// honored). Owner-only like the rest of the blob surface: friends'
/// byte access stays view-scoped.
#[utoipa::path(
    get,
    path = "/v1/blobs/{hash}/bytes",
    tag = "storage",
    security(("session_cookie" = []), ("bearer_token" = [])),
    params(("hash" = String, Path, description = "blake3, 64 hex chars (case-insensitive)")),
    responses(
        (status = 200, description = "The blob bytes", body = ImageBytes, content_type = "application/octet-stream"),
        (status = 206, description = "Requested range of the blob bytes", body = ImageBytes, content_type = "application/octet-stream"),
        (status = 400, description = "Not a blake3 hex hash", body = ApiError),
        (status = 403, description = "Owner only", body = ApiError),
        (status = 404, description = "No such blob (or no bytes to serve)", body = ApiError),
    ),
)]
fn blob_bytes() {}

/// The in-memory job registry: running jobs plus recently finished
/// ones (the registry keeps a bounded tail; a daemon restart forgets
/// everything — durable job reports are a recorded open question).
#[utoipa::path(
    get,
    path = "/v1/jobs",
    tag = "jobs",
    security(("session_cookie" = []), ("bearer_token" = [])),
    responses(
        (status = 200, description = "Running + recently finished jobs, newest first", body = JobsResponse),
        (status = 403, description = "Owner only", body = ApiError),
    ),
)]
fn jobs() {}

/// One job with counters and its (growing, then final) ingest report.
#[utoipa::path(
    get,
    path = "/v1/jobs/{id}",
    tag = "jobs",
    security(("session_cookie" = []), ("bearer_token" = [])),
    params(("id" = i64, Path, description = "Job id from POST /v1/ingest")),
    responses(
        (status = 200, description = "Job detail", body = JobDetail),
        (status = 403, description = "Owner only", body = ApiError),
        (status = 404, description = "No such job (or the registry forgot it)", body = ApiError),
    ),
)]
fn job_detail() {}

// ---- admin (owner-only) ----

/// Users with grants and live-session counts, plus pending invites.
#[utoipa::path(
    get,
    path = "/v1/admin/users",
    tag = "admin",
    security(("session_cookie" = []), ("bearer_token" = [])),
    responses(
        (status = 200, description = "Users + pending invites", body = AdminUsersResponse),
        (status = 403, description = "Owner only", body = ApiError),
    ),
)]
fn admin_users() {}

/// Mint an invite. The token appears exactly once, in the answered
/// fragment URL; the database stores only its blake3.
#[utoipa::path(
    post,
    path = "/v1/admin/invites",
    tag = "admin",
    security(("session_cookie" = []), ("bearer_token" = [])),
    request_body = InviteMintRequest,
    responses(
        (status = 200, description = "Invite minted", body = InviteMintResponse),
        (status = 400, description = "Bad role or expires_days", body = ApiError),
        (status = 403, description = "Owner only", body = ApiError),
    ),
)]
fn invite_create() {}

/// Revoke a pending invite by its stored token hash.
#[utoipa::path(
    delete,
    path = "/v1/admin/invites/{token_hash}",
    tag = "admin",
    security(("session_cookie" = []), ("bearer_token" = [])),
    params(("token_hash" = String, Path, description = "blake3 of the token, 64 hex chars")),
    responses(
        (status = 200, description = "Invite revoked", body = OkResponse),
        (status = 400, description = "Not a token hash", body = ApiError),
        (status = 403, description = "Owner only", body = ApiError),
        (status = 404, description = "No such pending invite", body = ApiError),
    ),
)]
fn invite_delete() {}

/// Grant a friend a view. Grants on views that exist nowhere (no tag,
/// no definition) are refused as typos, not recorded as policy.
#[utoipa::path(
    post,
    path = "/v1/admin/grants",
    tag = "admin",
    security(("session_cookie" = []), ("bearer_token" = [])),
    request_body = GrantAddRequest,
    responses(
        (status = 200, description = "Grant recorded (idempotent)", body = OkResponse),
        (status = 400, description = "Missing field", body = ApiError),
        (status = 403, description = "Owner only", body = ApiError),
        (status = 404, description = "No such user or view", body = ApiError),
    ),
)]
fn grant_create() {}

/// Revoke a grant.
#[utoipa::path(
    delete,
    path = "/v1/admin/grants/{username}/{view}",
    tag = "admin",
    security(("session_cookie" = []), ("bearer_token" = [])),
    params(
        ("username" = String, Path, description = "Grantee"),
        ("view" = String, Path, description = "View name"),
    ),
    responses(
        (status = 200, description = "Grant revoked", body = OkResponse),
        (status = 403, description = "Owner only", body = ApiError),
        (status = 404, description = "No such user or grant", body = ApiError),
    ),
)]
fn grant_delete() {}

/// Revoke every session a user holds.
#[utoipa::path(
    delete,
    path = "/v1/admin/sessions/{username}",
    tag = "admin",
    security(("session_cookie" = []), ("bearer_token" = [])),
    params(("username" = String, Path, description = "Whose sessions to revoke")),
    responses(
        (status = 200, description = "Sessions revoked", body = SessionsRevokedResponse),
        (status = 403, description = "Owner only", body = ApiError),
        (status = 404, description = "No such user", body = ApiError),
    ),
)]
fn sessions_delete() {}

// ---- assembly ----

/// Registers the two credential presentations (D68): the browser's
/// `datboi_session` cookie and the same token as a bearer header.
/// Loopback needs neither — it is implicitly the owner.
struct SecurityAddon;

impl Modify for SecurityAddon {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        let components = openapi
            .components
            .get_or_insert_with(utoipa::openapi::Components::default);
        components.add_security_scheme(
            "session_cookie",
            SecurityScheme::ApiKey(ApiKey::Cookie(ApiKeyValue::with_description(
                "datboi_session",
                "Browser session cookie (HttpOnly, SameSite=Lax), minted by login/invite \
                 acceptance.",
            ))),
        );
        components.add_security_scheme(
            "bearer_token",
            SecurityScheme::Http(
                HttpBuilder::new()
                    .scheme(HttpAuthScheme::Bearer)
                    .description(Some(
                        "The same session token as `Authorization: Bearer` — for tools.",
                    ))
                    .build(),
            ),
        );
    }
}

/// Reviewable orphan candidates (D73): unreferenced data blobs past
/// the grace window, with ingest provenance and keep-marks. Marks are
/// review state — apply re-verifies at delete time.
#[utoipa::path(
    get,
    path = "/v1/gc/orphans",
    tag = "gc",
    responses(
        (status = 200, description = "Reviewable candidates, oldest first", body = OrphansResponse),
        (status = 403, description = "Not the owner", body = ApiError),
    ),
)]
fn gc_orphans() {}

/// Set or clear a keep-mark ("this is not junk") — authoritative
/// state, survives cache rebuilds, excludes the blob from apply.
#[utoipa::path(
    post,
    path = "/v1/gc/keep",
    tag = "gc",
    request_body = GcKeepRequest,
    responses(
        (status = 200, description = "Keep-mark updated", body = OkResponse),
        (status = 400, description = "Missing field / not a blake3 hash", body = ApiError),
        (status = 403, description = "Not the owner", body = ApiError),
    ),
)]
fn gc_keep() {}

/// Apply the reviewed set — the one human-triggered destructive action
/// (D73). Every deletion re-verifies unreferenced + aged + unkept at
/// delete time under the D72 singleton guard; refusals count as
/// `skipped`, never errors.
#[utoipa::path(
    post,
    path = "/v1/gc/orphans/apply",
    tag = "gc",
    request_body = GcApplyRequest,
    responses(
        (status = 200, description = "Deletion report", body = GcApplyResponse),
        (status = 403, description = "Not the owner", body = ApiError),
        (status = 503, description = "GC guard busy (eviction in progress); retry", body = ApiError),
    ),
)]
fn gc_apply() {}

/// Read the eviction/GC policy (owner-only).
#[utoipa::path(
    get,
    path = "/v1/gc/config",
    tag = "gc",
    security(("session_cookie" = []), ("bearer_token" = [])),
    responses(
        (status = 200, description = "Current GC policy", body = GcConfig),
        (status = 403, description = "Owner only", body = ApiError),
    ),
)]
fn gc_config_get() {}

/// Set any subset of the eviction/GC policy (owner-only).
#[utoipa::path(
    put,
    path = "/v1/gc/config",
    tag = "gc",
    security(("session_cookie" = []), ("bearer_token" = [])),
    request_body = GcConfigRequest,
    responses(
        (status = 200, description = "Updated GC policy", body = GcConfig),
        (status = 400, description = "Malformed watermark or negative grace", body = ApiError),
        (status = 403, description = "Owner only", body = ApiError),
    ),
)]
fn gc_config_set() {}

/// Scrub the corpus (owner-only, D96): verify bytes against their names,
/// back-fill aliases + `verified_at`, check pack integrity, and
/// optionally rehabilitate poisoned recipes — the same walk `datboi
/// scrub` runs. Long-running, so it answers a job id.
#[utoipa::path(
    post,
    path = "/v1/scrub",
    tag = "storage",
    security(("session_cookie" = []), ("bearer_token" = [])),
    request_body = ScrubRequest,
    responses(
        (status = 202, description = "Scrub job started; poll GET /v1/jobs/{id}", body = JobStartResponse),
        (status = 400, description = "sample_pct out of range", body = ApiError),
        (status = 403, description = "Owner only", body = ApiError),
    ),
)]
fn scrub() {}

#[derive(OpenApi)]
#[openapi(

    info(
        title = "datboi /v1",
        description = "The datboi daemon's JSON API (D69 contract). Loopback callers are \
                       implicitly the owner (D68); non-loopback callers authenticate with a \
                       session cookie or bearer token. The `/view/*`, `/snap/*`, and `/dav` \
                       byte-serving surfaces are outside this contract.",
    ),
    paths(
        whoami,
        invite_accept,
        login,
        logout,
        systems,
        system_entries,
        system_entry,
        dat_import,
        ingest_upload,
        ingest_start,
        analyzers,
        analyzer_config,
        view_profiles,
        view_define,
        view_eval,
        view_mint,
        views,
        view_detail,
        view_files,
        view_image,
        storage,
        storage_breakdown,
        blobs,
        blob_detail,
        blob_bytes,
        blob_verify,
        blob_materialize,
        jobs,
        job_detail,
        admin_users,
        invite_create,
        invite_delete,
        grant_create,
        grant_delete,
        sessions_delete,
        gc_orphans,
        gc_keep,
        gc_apply,
        gc_config_get,
        gc_config_set,
        scrub,
    ),
    modifiers(&SecurityAddon),
)]
struct ApiDoc;

pub(crate) fn openapi() -> utoipa::openapi::OpenApi {
    ApiDoc::openapi()
}
