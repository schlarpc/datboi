/**
 * Thin typed fetch wrapper over the /v1 API. Sessions ride the
 * `datboi_session` cookie (D68), so every call is `credentials:
 * same-origin` and carries no auth header of its own.
 *
 * 401 handling: a 401 from a session-authenticated endpoint means the
 * session died under us — the client tells the session store (via the
 * registered handler, avoiding a client↔session import cycle) and
 * bounces to /login. The open auth endpoints opt out: a 401 from
 * `login` is "invalid credentials", not an expired session.
 */

import { router } from '../router.svelte';
import type { EntryState } from '../state';
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
  GrantParams,
  IngestParams,
  IngestStarted,
  InviteAcceptParams,
  JobDetailBody,
  JobsBody,
  LoginParams,
  MintedInvite,
  MintInviteParams,
  OkBody,
  RevokedSessions,
  SessionInfo,
  StorageBody,
  StorageBreakdownBody,
  SystemsBody,
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

interface RequestOpts {
  body?: unknown;
  /** Raw upload body (dat import); mutually exclusive with `body`. */
  rawBody?: Blob;
  /** false = open auth endpoint: 401 is a typed error, not session death. */
  sessionAuth?: boolean;
}

async function request<T>(method: string, path: string, opts: RequestOpts = {}): Promise<T> {
  let headers: Record<string, string> | undefined;
  let body: BodyInit | undefined;
  if (opts.rawBody !== undefined) {
    headers = { 'content-type': 'application/octet-stream' };
    body = opts.rawBody;
  } else if (opts.body !== undefined) {
    headers = { 'content-type': 'application/json' };
    body = JSON.stringify(opts.body);
  }
  const resp = await fetch(path, { method, credentials: 'same-origin', headers, body });
  if (resp.ok) {
    return (await resp.json()) as T;
  }
  const message = await errorMessage(resp);
  if (resp.status === 401 && opts.sessionAuth !== false) {
    unauthorizedHandler?.();
    router.replace('/login');
  }
  throw new ApiError(resp.status, message);
}

/** api.rs answers `{"error": msg}`; auth.rs validation answers plain text. */
async function errorMessage(resp: Response): Promise<string> {
  const text = await resp.text().catch(() => '');
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
  return text || resp.statusText;
}

// ---- auth ----

export const whoami = (): Promise<Whoami> =>
  request('GET', '/v1/auth/whoami', { sessionAuth: false });

export const login = (username: string, password: string): Promise<SessionInfo> =>
  request('POST', '/v1/auth/login', {
    body: { username, password } satisfies LoginParams,
    sessionAuth: false,
  });

export const logout = (): Promise<OkBody> =>
  request('POST', '/v1/auth/logout', { sessionAuth: false });

export const acceptInvite = (
  token: string,
  username: string,
  password: string,
): Promise<SessionInfo> =>
  request('POST', '/v1/auth/invite/accept', {
    body: { token, username, password } satisfies InviteAcceptParams,
    sessionAuth: false,
  });

// ---- read models ----

export const systems = (): Promise<SystemsBody> => request('GET', '/v1/systems');

export function systemEntries(
  systemId: number | string,
  params: EntriesParams = {},
): Promise<EntriesBody> {
  const query = new URLSearchParams();
  if (params.q) query.set('q', params.q);
  if (params.state) query.set('state', params.state);
  if (params.offset !== undefined) query.set('offset', String(params.offset));
  if (params.limit !== undefined) query.set('limit', String(params.limit));
  const qs = query.toString();
  return request('GET', `/v1/systems/${systemId}/entries${qs ? `?${qs}` : ''}`);
}

export const entryDetail = (systemId: number | string, name: string): Promise<EntryDetail> =>
  request('GET', `/v1/systems/${systemId}/entries/${encodeURIComponent(name)}`);

/**
 * POST /v1/dats/import — the raw dat file bytes ARE the body (no
 * multipart), same operation as `datboi dat import`. Provider/system
 * overrides ride the query string; omitted, they resolve from the dat
 * header server-side. The web UI no longer calls this — its drop
 * surfaces route through the unified ingest flow (uploadRom +
 * startIngest; the job classifies dats by content) — but the endpoint
 * is versioned contract for direct API users, so the fn stays.
 */
export function importDat(file: Blob, params: DatImportParams = {}): Promise<DatImportBody> {
  const query = new URLSearchParams();
  if (params.provider) query.set('provider', params.provider);
  if (params.system) query.set('system', params.system);
  const qs = query.toString();
  return request('POST', `/v1/dats/import${qs ? `?${qs}` : ''}`, { rawBody: file });
}

export const views = (): Promise<ViewsBody> => request('GET', '/v1/views');

export const viewDetail = (name: string): Promise<ViewDetail> =>
  request('GET', `/v1/views/${encodeURIComponent(name)}`);

export function viewFiles(name: string, params: ViewFilesParams = {}): Promise<ViewFilesBody> {
  const query = new URLSearchParams();
  if (params.q) query.set('q', params.q);
  if (params.offset !== undefined) query.set('offset', String(params.offset));
  if (params.limit !== undefined) query.set('limit', String(params.limit));
  const qs = query.toString();
  return request('GET', `/v1/views/${encodeURIComponent(name)}/files${qs ? `?${qs}` : ''}`);
}

// ---- content URLs (real anchors, not fetches — the browser downloads) ----

/** `/view/{name}/{path}` — the verified byte-serving tree (http.rs). */
export const viewFileUrl = (name: string, path: string): string =>
  `/view/${encodeURIComponent(name)}/${path.split('/').map(encodeURIComponent).join('/')}`;

/** `/v1/views/{name}/image` — the minted SD image download. */
export const viewImageUrl = (name: string): string =>
  `/v1/views/${encodeURIComponent(name)}/image`;

export const storage = (): Promise<StorageBody> => request('GET', '/v1/storage');

export const storageBreakdown = (): Promise<StorageBreakdownBody> =>
  request('GET', '/v1/storage/breakdown');

export function blobs(params: BlobsParams = {}): Promise<BlobsBody> {
  const query = new URLSearchParams();
  if (params.q) query.set('q', params.q);
  if (params.ns) query.set('ns', params.ns);
  if (params.residency) query.set('residency', params.residency);
  if (params.offset !== undefined) query.set('offset', String(params.offset));
  if (params.limit !== undefined) query.set('limit', String(params.limit));
  const qs = query.toString();
  return request('GET', `/v1/blobs${qs ? `?${qs}` : ''}`);
}

export const blobDetail = (hash: string): Promise<BlobDetail> =>
  request('GET', `/v1/blobs/${encodeURIComponent(hash)}`);

// ---- ingest ----

/**
 * POST /v1/ingest/uploads — stage one file. XHR, not fetch: upload
 * progress events don't exist on fetch, and multi-GB uploads without
 * a progress bar are a hostile UI. Error/401 handling mirrors
 * request().
 */
export function uploadRom(
  name: string,
  file: Blob,
  onProgress?: (sent: number, total: number) => void,
): Promise<UploadReceipt> {
  return new Promise((resolve, reject) => {
    const xhr = new XMLHttpRequest();
    xhr.open('POST', `/v1/ingest/uploads?name=${encodeURIComponent(name)}`);
    xhr.setRequestHeader('content-type', 'application/octet-stream');
    xhr.upload.onprogress = (e) => {
      if (e.lengthComputable) onProgress?.(e.loaded, e.total);
    };
    xhr.onload = () => {
      if (xhr.status >= 200 && xhr.status < 300) {
        resolve(JSON.parse(xhr.responseText) as UploadReceipt);
        return;
      }
      let message = xhr.statusText;
      try {
        const parsed: unknown = JSON.parse(xhr.responseText);
        if (
          typeof parsed === 'object' &&
          parsed !== null &&
          typeof (parsed as { error?: unknown }).error === 'string'
        ) {
          message = (parsed as { error: string }).error;
        }
      } catch {
        // not JSON — keep the status text
      }
      if (xhr.status === 401) {
        unauthorizedHandler?.();
        router.replace('/login');
      }
      reject(new ApiError(xhr.status, message));
    };
    xhr.onerror = () => reject(new ApiError(0, 'network error during upload'));
    xhr.send(file);
  });
}

/** POST /v1/ingest — spend staged tokens, start the background job. */
export const startIngest = (uploads: string[]): Promise<IngestStarted> =>
  request('POST', '/v1/ingest', { body: { uploads } satisfies IngestParams });

export const jobs = (): Promise<JobsBody> => request('GET', '/v1/jobs');

export const jobDetail = (id: number): Promise<JobDetailBody> =>
  request('GET', `/v1/jobs/${id}`);

// ---- admin ----

export const adminUsers = (): Promise<AdminUsersBody> => request('GET', '/v1/admin/users');

export const adminMintInvite = (params: MintInviteParams = {}): Promise<MintedInvite> =>
  request('POST', '/v1/admin/invites', { body: params });

export const adminRevokeInvite = (tokenHashHex: string): Promise<OkBody> =>
  request('DELETE', `/v1/admin/invites/${encodeURIComponent(tokenHashHex)}`);

export const adminGrant = (username: string, view: string): Promise<OkBody> =>
  request('POST', '/v1/admin/grants', { body: { username, view } satisfies GrantParams });

export const adminRevoke = (username: string, view: string): Promise<OkBody> =>
  request(
    'DELETE',
    `/v1/admin/grants/${encodeURIComponent(username)}/${encodeURIComponent(view)}`,
  );

export const adminRevokeSessions = (username: string): Promise<RevokedSessions> =>
  request('DELETE', `/v1/admin/sessions/${encodeURIComponent(username)}`);

// Re-exported so screens can filter with the canonical list.
export type { EntryState };
