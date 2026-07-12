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
import { router } from '../router.svelte';
import type { EntryState } from '../state';
import type { operations, paths } from './schema';
import type {
  AdminUsersBody,
  BlobDetail,
  BlobsBody,
  BlobsParams,
  DatImportBody,
  DatImportParams,
  EntriesBody,
  EntriesParams,
  EntryDetail,
  GcApplyReport,
  IngestStarted,
  InviteAcceptParams,
  JobDetailBody,
  JobsBody,
  LoginParams,
  MintedInvite,
  MintInviteParams,
  OkBody,
  OrphansBody,
  RevokedSessions,
  SessionInfo,
  StorageBody,
  StorageBreakdownBody,
  SystemsBody,
  UploadParams,
  UploadReceipt,
  ViewDetail,
  ViewFilesBody,
  ViewFilesParams,
  ViewsBody,
  Whoami,
} from './types';

/** Any non-2xx answer, with the server's message (JSON `error` or text). */
export class ApiError extends Error {
  readonly status: number;

  constructor(status: number, message: string) {
    super(message);
    this.name = 'ApiError';
    this.status = status;
  }
}

let unauthorizedHandler: (() => void) | null = null;

/** session.svelte.ts registers here so a mid-flight 401 flips its state. */
export function onUnauthorized(handler: () => void): void {
  unauthorizedHandler = handler;
}

/**
 * api.rs answers `{"error": msg}`; auth.rs validation and the session
 * middleware's 401 answer plain text. ONE decoder for both transports
 * (the fetch client below and uploadRom's XHR).
 */
function envelopeMessage(text: string, fallback: string): string {
  try {
    const parsed: unknown = JSON.parse(text);
    if (
      typeof parsed === 'object' &&
      parsed !== null &&
      typeof (parsed as { error?: unknown }).error === 'string'
    ) {
      return (parsed as { error: string }).error;
    }
  } catch {
    // not JSON — fall through to the raw text
  }
  return text || fallback;
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
    const message = envelopeMessage(text, response.statusText);
    const pathname = new URL(request.url, window.location.origin).pathname;
    handleUnauthorized(response.status, !OPEN_AUTH.has(pathname));
    throw new ApiError(response.status, message);
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

export const storage = async (): Promise<StorageBody> => unwrap(await client.GET('/v1/storage'));

export const storageBreakdown = async (): Promise<StorageBreakdownBody> =>
  unwrap(await client.GET('/v1/storage/breakdown'));

export const blobs = async (params: BlobsParams = {}): Promise<BlobsBody> =>
  unwrap(await client.GET('/v1/blobs', { params: { query: params } }));

export const blobDetail = async (hash: string): Promise<BlobDetail> =>
  unwrap(await client.GET('/v1/blobs/{hash}', { params: { path: { hash } } }));

// ---- gc (D73 review/apply) ----

export const gcOrphans = async (): Promise<OrphansBody> =>
  unwrap(await client.GET('/v1/gc/orphans'));

export const gcKeep = async (hash: string, keep: boolean): Promise<OkBody> =>
  unwrap(await client.POST('/v1/gc/keep', { body: { hash, keep } }));

/** Absent hashes = every reviewable, non-kept candidate. */
export const gcApply = async (hashes?: string[]): Promise<GcApplyReport> =>
  unwrap(await client.POST('/v1/gc/orphans/apply', { body: hashes ? { hashes } : {} }));

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
        resolve(JSON.parse(xhr.responseText) as UploadReceipt);
        return;
      }
      const message = envelopeMessage(xhr.responseText, xhr.statusText);
      handleUnauthorized(xhr.status, true);
      reject(new ApiError(xhr.status, message));
    };
    xhr.onerror = () => reject(new ApiError(0, 'network error during upload'));
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
