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
  EntriesBody,
  EntriesParams,
  EntryDetail,
  GrantParams,
  InviteAcceptParams,
  JobsBody,
  LoginParams,
  MintedInvite,
  MintInviteParams,
  OkBody,
  RevokedSessions,
  SessionInfo,
  StorageBody,
  SystemsBody,
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
  /** false = open auth endpoint: 401 is a typed error, not session death. */
  sessionAuth?: boolean;
}

async function request<T>(method: string, path: string, opts: RequestOpts = {}): Promise<T> {
  const resp = await fetch(path, {
    method,
    credentials: 'same-origin',
    headers: opts.body === undefined ? undefined : { 'content-type': 'application/json' },
    body: opts.body === undefined ? undefined : JSON.stringify(opts.body),
  });
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

export const jobs = (): Promise<JobsBody> => request('GET', '/v1/jobs');

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
