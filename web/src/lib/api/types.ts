/**
 * D69 contract shim: every shape here is a type ALIAS into the generated
 * OpenAPI types (`./schema.d.ts`, produced by `npm run generate` from the
 * checked-in `crates/datboi-api/openapi.json`). Nothing in this file
 * declares a structure of its own — the rust `datboi-api` crate owns the
 * shapes, the spec is the seam, and screens keep their existing import
 * paths. After a rust API change: `cargo run -p datboi-api --bin
 * gen-openapi`, then `npm run generate` here.
 */

import type { components, operations } from './schema';

type Schemas = components['schemas'];

// ---- /v1/auth/* ----

/** GET /v1/auth/whoami — open; answers `authenticated: false`, never 401. */
export type Whoami = Schemas['WhoamiResponse'];

export type Role = Schemas['Role'];

/** POST /v1/auth/login and /v1/auth/invite/accept success body. */
export type SessionInfo = Schemas['SessionResponse'];

export type LoginParams = Schemas['LoginRequest'];
export type InviteAcceptParams = Schemas['InviteAcceptRequest'];

export type OkBody = Schemas['OkResponse'];

// ---- GET /v1/systems ----

export type SystemRevision = Schemas['Revision'];
export type System = Schemas['System'];
export type SystemsBody = Schemas['SystemsResponse'];

// ---- GET /v1/systems/{id}/entries ----

export type EntriesParams = NonNullable<operations['system_entries']['parameters']['query']>;
export type EntryRow = Schemas['EntryRow'];
export type EntriesBody = Schemas['EntriesPage'];

// ---- GET /v1/systems/{id}/entries/{name} ----

export type EntryRevision = Schemas['Revision'];
export type ClaimState = Schemas['ClaimState'];
export type RomBlob = Schemas['BlobInfo'];
export type RomRoute = Schemas['RouteInfo'];
export type Rom = Schemas['RomClaim'];
export type EntryDetail = Schemas['EntryDetail'];

// ---- POST /v1/dats/import ----

export type DatImportParams = NonNullable<operations['dat_import']['parameters']['query']>;
export type DatImportBody = Schemas['DatImportResponse'];

// ---- GET /v1/views (+ /{name}) ----

export type OneGOneR = Schemas['OneGOneR'];
export type ImageParams = Schemas['ImageParams'];
export type ViewDefBody = Schemas['Definition'];
export type View = Schemas['ViewSummary'];
export type ViewsBody = Schemas['ViewsResponse'];
export type MintedImage = Schemas['ImageStatus'];
export type ViewDetail = Schemas['ViewDetail'];

// ---- GET /v1/views/{name}/files ----

export type ViewFilesParams = NonNullable<operations['view_files']['parameters']['query']>;
export type ViewFileRow = Schemas['FileRow'];
export type ViewFilesBody = Schemas['ViewFilesPage'];

// ---- GET /v1/storage ----

export type QuarantineItem = Schemas['QuarantineItem'];
export type StorageBody = Schemas['StorageResponse'];

// ---- GET /v1/storage/breakdown ----

export type ClassBytes = Schemas['ClassBytes'];
export type SourceBytes = Schemas['SourceBytes'];
export type StorageBreakdownBody = Schemas['StorageBreakdown'];

// ---- GET /v1/blobs (+ /{hash}) ----

export type BlobsParams = NonNullable<operations['blobs']['parameters']['query']>;
export type ResidencyState = Schemas['ResidencyState'];
export type BlobRow = Schemas['BlobRow'];
export type BlobsBody = Schemas['BlobsPage'];
export type BlobDigests = Schemas['BlobDigests'];
export type ProvenanceRow = Schemas['ProvenanceRow'];
export type HashRef = Schemas['HashRef'];
export type RouteEdge = Schemas['RouteEdge'];
export type ClaimRef = Schemas['ClaimRef'];
export type BlobDetail = Schemas['BlobDetail'];

// ---- GET /v1/gc/orphans (+ keep / apply) ----

export type OrphanItem = Schemas['OrphanItem'];
export type OrphansBody = Schemas['OrphansResponse'];
export type GcApplyReport = Schemas['GcApplyResponse'];

// ---- POST /v1/ingest/uploads + /v1/ingest ----

/** The upload's query params: `name` is required and typed here even
 * though the XHR transport builds its own URL. */
export type UploadParams = operations['ingest_upload']['parameters']['query'];
export type UploadReceipt = Schemas['UploadResponse'];
export type IngestParams = Schemas['IngestRequest'];
export type IngestStarted = Schemas['IngestStartResponse'];
export type IngestReport = Schemas['IngestReportBody'];
export type DatImportedItem = Schemas['DatImportedItem'];

// ---- GET /v1/jobs (+ /{id}) ----

export type Job = Schemas['Job'];
export type JobsBody = Schemas['JobsResponse'];
export type JobDetailBody = Schemas['JobDetail'];
export type MatchedEntry = Schemas['MatchedEntry'];

// ---- /v1/admin/* ----

export type AdminUser = Schemas['UserRow'];
export type PendingInvite = Schemas['InviteRow'];
export type AdminUsersBody = Schemas['AdminUsersResponse'];
export type MintInviteParams = Schemas['InviteMintRequest'];
export type MintedInvite = Schemas['InviteMintResponse'];
export type GrantParams = Schemas['GrantAddRequest'];
export type RevokedSessions = Schemas['SessionsRevokedResponse'];

// ---- errors ----

/** Every /v1 error body: `{"error": "<message>"}`. */
export type ApiErrorBody = Schemas['ApiError'];
