/**
 * Typed /v1 client over openapi-fetch, bound to the GENERATED `paths`
 * types (schema.d.ts): every path string, method, and path/query param
 * is compile-checked against the contract, so a server-side rename or
 * an added param is a type error here, not a silent runtime 404.
 * Sessions ride the `datboi_session` cookie (D68), so every call is
 * `credentials: same-origin` and carries no auth header of its own.
 *
 * 401 handling: a 401 from a session-authenticated endpoint means the
 * session died under us — the client tells the session store (via the
 * registered handler, avoiding a client↔session import cycle) and
 * bounces to /login. The open auth endpoints opt out: a 401 from
 * `login` is "invalid credentials", not an expired session.
 */

import createClient from 'openapi-fetch';
import { loginReturn, router } from '../router.svelte';
import type { EntryState } from '../state';
import type { operations, paths } from './schema';
import type {
  AdminUsersBody,
  AnalyzerConfigParams,
  AnalyzerInfo,
  AnalyzersBody,
  BlobDetail,
  BlobsBody,
  BlobsParams,
  ClonelistBody,
  DatDiffBody,
  DatFetchBody,
  DatImportBody,
  DatImportParams,
  EntriesBody,
  EntriesParams,
  EntryDetail,
  ErrorCode,
  EvictPlanBody,
  GcApplyReport,
  GcConfigBody,
  GcConfigParams,
  IngestStarted,
  InviteAcceptParams,
  JobDetailBody,
  JobsBody,
  JobStarted,
  LoginParams,
  MintedInvite,
  MintInviteParams,
  OkBody,
  OrphansBody,
  P2pStatusBody,
  RevokedSessions,
  ScrubParams,
  SessionInfo,
  SnapshotBody,
  StorageBody,
  StorageBreakdownBody,
  SystemsBody,
  UploadParams,
  UploadReceipt,
  VerifyStarted,
  ViewDetail,
  ViewFilesBody,
  ViewFilesParams,
  ViewsBody,
  Whoami,
} from './types';

/** Any non-2xx answer: the machine-readable code (when the body wore
 * the D77 envelope) plus the server's diagnostic message. Screens
 * present the code's translated copy (errors.svelte.ts), never the
 * message — the message is for logs and power users. */
export class ApiError extends Error {
  readonly status: number;
  readonly code: ErrorCode | undefined;

  constructor(status: number, message: string, code?: ErrorCode) {
    super(message);
    this.name = 'ApiError';
    this.status = status;
    this.code = code;
  }
}

let unauthorizedHandler: (() => void) | null = null;

/** session.svelte.ts registers here so a mid-flight 401 flips its state. */
export function onUnauthorized(handler: () => void): void {
  unauthorizedHandler = handler;
}

/**
 * Every /v1 error wears `{"error": msg, "code": code}` (D77). ONE
 * decoder for both transports (the fetch client below and uploadRom's
 * XHR); a non-envelope body (proxy error page, route-level 404)
 * decodes to the raw text with no code.
 */
function decodeEnvelope(text: string, fallback: string): { message: string; code?: ErrorCode } {
  try {
    const parsed: unknown = JSON.parse(text);
    if (
      typeof parsed === 'object' &&
      parsed !== null &&
      typeof (parsed as { error?: unknown }).error === 'string'
    ) {
      const code = (parsed as { code?: unknown }).code;
      return {
        message: (parsed as { error: string }).error,
        // Trust the contract for the cast; a code this build doesn't
        // know simply misses the message map and falls back.
        code: typeof code === 'string' ? (code as ErrorCode) : undefined,
      };
    }
  } catch {
    // not JSON — fall through to the raw text
  }
  return { message: text || fallback };
}

/** Open auth endpoints: a 401 here is a typed answer (bad credentials,
 * no session to log out), never session death. */
const OPEN_AUTH: ReadonlySet<string> = new Set<keyof paths>([
  '/v1/auth/whoami',
  '/v1/auth/login',
  '/v1/auth/logout',
  '/v1/auth/invite/accept',
]);

/** Session death on a session-authenticated endpoint: tell the session
 * store and bounce — shared by the fetch client and uploadRom's XHR. */
function handleUnauthorized(status: number, sessionAuth: boolean): void {
  if (status === 401 && sessionAuth) {
    unauthorizedHandler?.();
    loginReturn.stash(window.location.pathname);
    router.replace('/login');
  }
}

const client = createClient<paths>({
  credentials: 'same-origin',
  // Late-bound: createClient would otherwise capture globalThis.fetch at
  // import time, dodging the test suite's stubbed fetch.
  fetch: (request) => globalThis.fetch(request),
});

// Every non-2xx becomes a thrown ApiError (with the error-envelope
// message) before openapi-fetch's data/error split, so per-operation
// functions keep their promise-rejection contract.
client.use({
  async onResponse({ request, response }) {
    if (response.ok) {
      return undefined;
    }
    const text = await response.text().catch(() => '');
    const { message, code } = decodeEnvelope(text, response.statusText);
    const pathname = new URL(request.url, window.location.origin).pathname;
    handleUnauthorized(response.status, !OPEN_AUTH.has(pathname));
    throw new ApiError(response.status, message, code);
  },
});

/** onResponse throws on every non-2xx, so `data` is always present. */
const unwrap = <T>(result: { data?: T }): T => result.data as T;

// ---- auth ----

export const whoami = async (): Promise<Whoami> => unwrap(await client.GET('/v1/auth/whoami'));

export const login = async (username: string, password: string): Promise<SessionInfo> =>
  unwrap(
    await client.POST('/v1/auth/login', {
      body: { username, password } satisfies LoginParams,
    }),
  );

export const logout = async (): Promise<OkBody> => unwrap(await client.POST('/v1/auth/logout'));

export const acceptInvite = async (
  token: string,
  username: string,
  password: string,
): Promise<SessionInfo> =>
  unwrap(
    await client.POST('/v1/auth/invite/accept', {
      body: { token, username, password } satisfies InviteAcceptParams,
    }),
  );

// ---- read models ----

export const systems = async (): Promise<SystemsBody> => unwrap(await client.GET('/v1/systems'));

export const systemEntries = async (
  systemId: number | string,
  params: EntriesParams = {},
): Promise<EntriesBody> =>
  unwrap(
    await client.GET('/v1/systems/{id}/entries', {
      params: { path: { id: Number(systemId) }, query: params },
    }),
  );

export const entryDetail = async (
  systemId: number | string,
  name: string,
): Promise<EntryDetail> =>
  unwrap(
    await client.GET('/v1/systems/{id}/entries/{name}', {
      params: { path: { id: Number(systemId), name } },
    }),
  );

/** Raw upload bodies: the contract spells the bytes `string` (binary),
 * the transport wants the Blob untouched — one cast, one serializer. */
const rawBody = (blob: Blob): string => blob as unknown as string;
const passthrough = (body: string): BodyInit => body as unknown as Blob;
const OCTET_STREAM = { 'content-type': 'application/octet-stream' } as const;

/**
 * POST /v1/dats/import — the raw dat file bytes ARE the body (no
 * multipart), same operation as `datboi dat import`. Provider/system
 * overrides ride the query string; omitted, they resolve from the dat
 * header server-side. The web UI no longer calls this — its drop
 * surfaces route through the unified ingest flow (uploadRom +
 * startIngest; the job classifies dats by content) — but the endpoint
 * is versioned contract for direct API users, so the fn stays.
 */
export const importDat = async (
  file: Blob,
  params: DatImportParams = {},
): Promise<DatImportBody> =>
  unwrap(
    await client.POST('/v1/dats/import', {
      params: { query: params },
      body: rawBody(file),
      bodySerializer: passthrough,
      headers: OCTET_STREAM,
    }),
  );

/**
 * POST /v1/dats/fetch — fetch a dat over HTTP and import it (Redump
 * auto-fetch, D16). `source` is a full URL or `redump/<slug>`; the
 * receipt carries the resolved URL and the import outcome.
 */
export const datFetch = async (
  source: string,
  provider?: string,
  system?: string,
): Promise<DatFetchBody> =>
  unwrap(
    await client.POST('/v1/dats/fetch', {
      body: {
        source,
        ...(provider != null && provider !== '' ? { provider } : {}),
        ...(system != null && system !== '' ? { system } : {}),
      },
    }),
  );

/** GET /v1/dats/{provider}/{system}/diff — previous → current (D38). */
export const datDiff = async (provider: string, system: string): Promise<DatDiffBody> =>
  unwrap(
    await client.GET('/v1/dats/{provider}/{system}/diff', {
      params: { path: { provider, system } },
    }),
  );

/** `/v1/dats/{provider}/{system}/export` — the current revision as a
 * Logiqx dat download (a real anchor, not a fetch). */
const DAT_EXPORT_PATH = '/v1/dats/{provider}/{system}/export' satisfies keyof paths;
export const datExportUrl = (provider: string, system: string): string =>
  DAT_EXPORT_PATH.replace('{provider}', encodeURIComponent(provider)).replace(
    '{system}',
    encodeURIComponent(system),
  );

/** POST /v1/dats/{provider}/{system}/clonelist — link a retool clonelist
 * (D57); the raw JSON bytes ARE the body. */
export const datClonelist = async (
  provider: string,
  system: string,
  file: Blob,
): Promise<ClonelistBody> =>
  unwrap(
    await client.POST('/v1/dats/{provider}/{system}/clonelist', {
      params: { path: { provider, system } },
      body: rawBody(file),
      bodySerializer: passthrough,
      headers: OCTET_STREAM,
    }),
  );

export const views = async (): Promise<ViewsBody> => unwrap(await client.GET('/v1/views'));

export const viewDetail = async (name: string): Promise<ViewDetail> =>
  unwrap(await client.GET('/v1/views/{name}', { params: { path: { name } } }));

export const viewFiles = async (
  name: string,
  params: ViewFilesParams = {},
): Promise<ViewFilesBody> =>
  unwrap(
    await client.GET('/v1/views/{name}/files', {
      params: { path: { name }, query: params },
    }),
  );

// ---- content URLs (real anchors, not fetches — the browser downloads) ----

/** `/view/{name}/{path}` — the verified byte-serving tree (http.rs;
 * deliberately outside the /v1 spec, so hand-built). */
export const viewFileUrl = (name: string, path: string): string =>
  `/view/${encodeURIComponent(name)}/${path.split('/').map(encodeURIComponent).join('/')}`;

/** `/v1/views/{name}/image` — the minted SD image download. The
 * template is pinned to the contract even though anchors skip fetch. */
const VIEW_IMAGE_PATH = '/v1/views/{name}/image' satisfies keyof paths;
export const viewImageUrl = (name: string): string =>
  VIEW_IMAGE_PATH.replace('{name}', encodeURIComponent(name));

/** `/v1/blobs/{hash}/bytes` — raw verified blob bytes, owner-only.
 * The Play screen fetches BIOS slots and blob-sourced ROMs (D85)
 * through it; the URL is the content hash. */
const BLOB_BYTES_PATH = '/v1/blobs/{hash}/bytes' satisfies keyof paths;
export const blobBytesUrl = (hash: string): string => BLOB_BYTES_PATH.replace('{hash}', hash);

export const storage = async (): Promise<StorageBody> => unwrap(await client.GET('/v1/storage'));

export const storageBreakdown = async (): Promise<StorageBreakdownBody> =>
  unwrap(await client.GET('/v1/storage/breakdown'));

export const blobs = async (params: BlobsParams = {}): Promise<BlobsBody> =>
  unwrap(await client.GET('/v1/blobs', { params: { query: params } }));

export const blobDetail = async (hash: string): Promise<BlobDetail> =>
  unwrap(await client.GET('/v1/blobs/{hash}', { params: { path: { hash } } }));

/** D80: verify one blob right now; poll the returned job. */
export const blobVerify = async (hash: string): Promise<VerifyStarted> =>
  unwrap(await client.POST('/v1/blobs/{hash}/verify', { params: { path: { hash } } }));

/** D96: rematerialize an evicted/absent blob by replaying its rebuild
 * route (synchronous; an already-resident blob is a no-op success). */
export const blobMaterialize = async (hash: string): Promise<OkBody> =>
  unwrap(await client.POST('/v1/blobs/{hash}/materialize', { params: { path: { hash } } }));

// ---- gc (D73 review/apply) ----

export const gcOrphans = async (): Promise<OrphansBody> =>
  unwrap(await client.GET('/v1/gc/orphans'));

export const gcKeep = async (hash: string, keep: boolean): Promise<OkBody> =>
  unwrap(await client.POST('/v1/gc/keep', { body: { hash, keep } }));

/** Absent hashes = every reviewable, non-kept candidate. */
export const gcApply = async (hashes?: string[]): Promise<GcApplyReport> =>
  unwrap(await client.POST('/v1/gc/orphans/apply', { body: hashes ? { hashes } : {} }));

/** GET /v1/gc/config — current watermark + grace policy. */
export const gcConfig = async (): Promise<GcConfigBody> =>
  unwrap(await client.GET('/v1/gc/config'));

/** PUT /v1/gc/config — set any subset; answers the full updated policy. */
export const gcConfigSet = async (body: GcConfigParams): Promise<GcConfigBody> =>
  unwrap(await client.PUT('/v1/gc/config', { body }));

/** POST /v1/evict dry_run — the D27 plan preview (what would drop at a
 * target). The endpoint returns EvictPlan for dry_run; the cast pins
 * that half of the 200|202 union. */
export const evictPlan = async (targetBytes: number): Promise<EvictPlanBody> =>
  unwrap(
    await client.POST('/v1/evict', { body: { target_bytes: targetBytes, dry_run: true } }),
  ) as EvictPlanBody;

/** POST /v1/evict — the guarded real drop; poll the returned Gc job. */
export const evict = async (targetBytes: number, license = false): Promise<JobStarted> =>
  unwrap(
    await client.POST('/v1/evict', { body: { target_bytes: targetBytes, license } }),
  ) as JobStarted;

// ---- maintenance (D96) ----

/** POST /v1/scrub — corpus scrub; poll the returned job for the report. */
export const scrub = async (body: ScrubParams = {}): Promise<JobStarted> =>
  unwrap(await client.POST('/v1/scrub', { body }));

/** POST /v1/snapshot — mint a state snapshot now (synchronous); the
 * manual trigger beside the daemon's auto-cadence. */
export const snapshot = async (): Promise<SnapshotBody> =>
  unwrap(await client.POST('/v1/snapshot'));

/** GET /v1/analyzers — the analyzer families and their enable/params. */
export const analyzers = async (): Promise<AnalyzersBody> =>
  unwrap(await client.GET('/v1/analyzers'));

/** PUT /v1/analyzers/{family} — set a family's full config; answers the
 * updated row. `params_hex` must be preserved by the caller (the body
 * sets the whole config, D60). */
export const analyzerConfig = async (
  family: string,
  body: AnalyzerConfigParams,
): Promise<AnalyzerInfo> =>
  unwrap(await client.PUT('/v1/analyzers/{family}', { params: { path: { family } }, body }));

/** POST /v1/sweep — run one analyzer sweep round; poll the Refine job. */
export const sweep = async (analyzer: string): Promise<JobStarted> =>
  unwrap(await client.POST('/v1/sweep', { body: { analyzer } }));

// ---- p2p (D101) ----

/** GET /v1/p2p — is the seedbox live, and our shareable endpoint id. */
export const p2pStatus = async (): Promise<P2pStatusBody> =>
  unwrap(await client.GET('/v1/p2p'));

/** POST /v1/p2p/sync — reconcile with a peer and fetch the diff (D100);
 * poll the returned Sync job, whose detail's `sync` field carries the
 * savings summary. No wants = mirror mode. */
export const p2pSync = async (peer: string, wants: string[] = []): Promise<JobStarted> =>
  unwrap(await client.POST('/v1/p2p/sync', { body: { peer, wants } }));

// ---- ingest ----

const UPLOAD_PATH = '/v1/ingest/uploads' satisfies keyof paths;

/**
 * POST /v1/ingest/uploads — stage one file. XHR, not fetch: upload
 * progress events don't exist on fetch, and multi-GB uploads without
 * a progress bar are a hostile UI. Error decoding and 401 session
 * death share envelopeMessage/handleUnauthorized with the fetch path.
 */
export function uploadRom(
  name: UploadParams['name'],
  file: Blob,
  onProgress?: (sent: number, total: number) => void,
): Promise<UploadReceipt> {
  return new Promise((resolve, reject) => {
    const xhr = new XMLHttpRequest();
    xhr.open('POST', `${UPLOAD_PATH}?name=${encodeURIComponent(name)}`);
    xhr.setRequestHeader('content-type', 'application/octet-stream');
    xhr.upload.onprogress = (e) => {
      if (e.lengthComputable) onProgress?.(e.loaded, e.total);
    };
    xhr.onload = () => {
      if (xhr.status >= 200 && xhr.status < 300) {
        // This runs in an event handler, not the executor: a throw here
        // would vanish into the event loop and the promise would hang
        // forever — every exit must be an explicit resolve/reject.
        try {
          resolve(JSON.parse(xhr.responseText) as UploadReceipt);
        } catch {
          reject(new ApiError(xhr.status, 'malformed upload receipt'));
        }
        return;
      }
      const { message, code } = decodeEnvelope(xhr.responseText, xhr.statusText);
      handleUnauthorized(xhr.status, true);
      reject(new ApiError(xhr.status, message, code));
    };
    xhr.onerror = () => reject(new ApiError(0, 'network error during upload'));
    xhr.onabort = () => reject(new ApiError(0, 'upload aborted'));
    // No xhr.timeout: it caps TOTAL duration, and a legitimate multi-GB
    // upload to a NAS can run for hours — a fixed cap would kill it.
    xhr.send(file);
  });
}

/** POST /v1/ingest — spend staged tokens, start the background job. */
export const startIngest = async (uploads: string[]): Promise<IngestStarted> =>
  unwrap(await client.POST('/v1/ingest', { body: { uploads } }));

export const jobs = async (): Promise<JobsBody> => unwrap(await client.GET('/v1/jobs'));

export const jobDetail = async (id: number): Promise<JobDetailBody> =>
  unwrap(await client.GET('/v1/jobs/{id}', { params: { path: { id } } }));

// ---- admin ----

export const adminUsers = async (): Promise<AdminUsersBody> =>
  unwrap(await client.GET('/v1/admin/users'));

export const adminMintInvite = async (params: MintInviteParams = {}): Promise<MintedInvite> =>
  unwrap(await client.POST('/v1/admin/invites', { body: params }));

export const adminRevokeInvite = async (tokenHashHex: string): Promise<OkBody> =>
  unwrap(
    await client.DELETE('/v1/admin/invites/{token_hash}', {
      params: { path: { token_hash: tokenHashHex } },
    }),
  );

export const adminGrant = async (username: string, view: string): Promise<OkBody> =>
  unwrap(await client.POST('/v1/admin/grants', { body: { username, view } }));

export const adminRevoke = async (username: string, view: string): Promise<OkBody> =>
  unwrap(
    await client.DELETE('/v1/admin/grants/{username}/{view}', {
      params: { path: { username, view } },
    }),
  );

export const adminRevokeSessions = async (username: string): Promise<RevokedSessions> =>
  unwrap(
    await client.DELETE('/v1/admin/sessions/{username}', {
      params: { path: { username } },
    }),
  );

// Re-exported so screens can filter with the canonical list.
export type { EntryState };
