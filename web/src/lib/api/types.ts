/**
 * Hand-written TS types for the /v1 API, matched field-for-field to the
 * `json!` literals in crates/datboi-server/src/{api,auth,admin}.rs and
 * pinned by that crate's integration tests (docs/open-questions.md
 * § "Shared API types": no serde derive in the codebase makes codegen
 * non-trivial, so M5 hand-writes these — this file is the client side
 * of that contract; revisit if drift bites).
 */

import type { EntryState, StateCounts } from '../state';

// ---- /v1/auth/* (auth.rs) ----

/** GET /v1/auth/whoami — open; answers `authenticated: false`, never 401. */
export type Whoami =
  | { authenticated: false }
  | {
      authenticated: true;
      /** Absent for loopback callers (no user row, D68). */
      username?: string;
      role: 'owner' | 'friend';
      via: 'loopback' | 'session' | 'bearer';
    };

/** POST /v1/auth/login and /v1/auth/invite/accept success body. */
export interface SessionInfo {
  authenticated: true;
  username: string;
  role: 'owner' | 'friend';
  expires_at: number;
}

export interface OkBody {
  ok: true;
}

// ---- GET /v1/systems (api.rs systems_body) ----

export interface SystemRevision {
  id: number;
  version: string | null;
  date: string | null;
  imported_at: number | null;
}

export interface System {
  /** dat_source surrogate id — a handle, not a durable identity. */
  id: number;
  provider: string;
  system: string;
  /** `{provider}/{system}` convenience. */
  source: string;
  revision: SystemRevision | null;
  counts: StateCounts;
  total: number;
  views: string[];
}

export interface SystemsBody {
  systems: System[];
}

// ---- GET /v1/systems/{id}/entries ----

export interface EntriesParams {
  q?: string;
  state?: EntryState;
  offset?: number;
  /** Server clamps to 1..=1000; default 200. */
  limit?: number;
}

export interface EntryRow {
  name: string;
  state: EntryState;
  /** Total required size in bytes; null for missing/nodump rows. */
  size: number | null;
  /** Full hex; the UI truncates. Null when the entry wants ≠1 ROM. */
  wanted_hash: string | null;
  wanted_hash_algo: 'crc32' | 'md5' | 'sha1' | 'sha256' | null;
}

export interface EntriesBody {
  entries: EntryRow[];
  /** Count under the CURRENT filter, not the page length. */
  total: number;
  offset: number;
  limit: number;
}

// ---- GET /v1/systems/{id}/entries/{name} ----

export interface EntryRevision {
  id: number;
  version: string | null;
  date: string | null;
  imported_at: number;
}

/** Per-claim rollup states include the sub-holdings the UI folds away. */
export type ClaimState = EntryState | 'peer' | 'probable';

export interface RomBlob {
  hash: string;
  residency: 'resident' | 'evicted_covered' | 'absent';
  /** Last full-hash store verification (unix secs); method unrecorded. */
  verified_at: number | null;
}

export interface RomRoute {
  /** Human-readable `verb ← sources` line, rendered as-is. */
  route: string;
  /** The design's source-availability dot. */
  source_present: boolean;
  verify: 'pending' | 'verified' | 'replayed_local';
}

export interface Rom {
  name: string;
  size: number | null;
  state: ClaimState;
  optional: boolean;
  hashes: Partial<Record<'crc32' | 'md5' | 'sha1' | 'sha256', string>>;
  /** Present only when the claim resolved to a local blob. */
  blob?: RomBlob;
  routes?: RomRoute[];
  /** Views whose current snapshots reference the blob (pins, D33). */
  pins?: string[];
}

export interface EntryDetail {
  name: string;
  state: EntryState;
  size: number | null;
  wanted_hash: string | null;
  wanted_hash_algo: string | null;
  revision: EntryRevision;
  roms: Rom[];
}

// ---- GET /v1/views (+ /{name}) ----

export interface OneGOneR {
  mode: 'strict' | 'held_first';
  regions: string[];
  langs: string[];
}

export interface ImageParams {
  cluster_size: number;
  partition: boolean;
  label: string | null;
}

export interface ViewDefBody {
  provider: string;
  system: string;
  template: string;
  one_g_one_r: OneGOneR | null;
  profile: string | null;
  image: ImageParams | null;
  mame_mode: string | null;
}

export interface View {
  name: string;
  snapshot: string | null;
  definition: ViewDefBody | null;
  /** Snapshot stats; absent when the snapshot is missing/undecodable. */
  rows?: number;
  bytes?: number;
  created_at?: number;
}

export interface ViewsBody {
  views: View[];
}

export type MintedImage = { minted: true; hash: string; bytes: number | null } | { minted: false };

export interface ViewDetail extends View {
  endpoints: { http: string; dav: string };
  /** Null when the view has no image profile (D62). */
  image: MintedImage | null;
}

// ---- GET /v1/storage ----

export interface QuarantineItem {
  component: string;
  quarantined_at: number;
  reason: string;
}

export interface StorageBody {
  blob_count: number;
  on_disk_bytes: number;
  represented_bytes: number;
  literal_only_bytes: number;
  quarantine: { count: number; items: QuarantineItem[] };
}

// ---- GET /v1/jobs ----

/**
 * The daemon has no job registry yet — /v1/jobs truthfully answers
 * `{"jobs": []}` (docs/open-questions.md § "Jobs tray backend"). This
 * shape is the TRAY's rendering contract (spec §2.2: name + progress +
 * done label), written forward so the rows exist when jobs arrive;
 * re-pin against the rust JSON when the registry lands.
 */
export interface Job {
  id: number;
  name: string;
  /** 0–100. */
  progress: number;
}

export interface JobsBody {
  jobs: Job[];
}

// ---- /v1/admin/* (admin.rs) ----

export interface AdminUser {
  username: string;
  role: 'owner' | 'friend';
  created_at: number;
  grants: string[];
  sessions: number;
}

export interface PendingInvite {
  token_hash: string;
  role: 'owner' | 'friend';
  expires_at: number;
  created_by: string | null;
}

export interface AdminUsersBody {
  users: AdminUser[];
  invites: PendingInvite[];
}

export interface MintInviteParams {
  role?: 'owner' | 'friend';
  expires_days?: number;
}

export interface MintedInvite {
  /** `/invite#<token>` — the token rides the fragment, never a log line. */
  url_path: string;
  expires_at: number;
}

export interface RevokedSessions {
  revoked: number;
}
