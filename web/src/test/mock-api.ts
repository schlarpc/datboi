/**
 * Test-only fetch stub that emulates the /v1 read-model API over
 * in-memory fixtures, including the server's entries query semantics
 * (case-insensitive substring `q`, `state` filter, offset/limit paging
 * — api.rs parse_page/entries_body) so component tests exercise real
 * filter+search composition instead of canned responses. The admin
 * mutation endpoints answer canned bodies (the server's are `{ok}`
 * acknowledgements) with knobs for failure/latency so optimistic-UI
 * paths can be exercised.
 */

import { vi } from 'vitest';
import type {
  AdminUsersBody,
  DatImportBody,
  EntryDetail,
  EntryRow,
  Job,
  JobDetailBody,
  MintedInvite,
  StorageBody,
  System,
  ViewDetail,
  ViewFileRow,
  Whoami,
} from '../lib/api/types';

export interface MockUniverse {
  whoami?: Whoami;
  systems?: System[];
  entries?: EntryRow[];
  /** Entry detail by name; default derives a minimal one from the row. */
  detail?: (name: string) => EntryDetail | undefined;
  storage?: StorageBody;
  /** GET /v1/jobs rows (the in-memory registry's tray render). */
  jobs?: Job[];
  /** GET /v1/jobs/{id} script: each poll SHIFTS one entry until the
   * last, which then repeats — tests script running→done timelines. */
  jobTimeline?: JobDetailBody[];
  /** POST /v1/ingest answer; defaults to job 1. */
  ingestJob?: number;
  /** POST /v1/ingest rejects (unknown token shape). */
  ingestFail?: boolean;
  /** Full detail bodies; the list endpoint serves the same objects
   * (extra fields are harmless — the real list is a subset). */
  views?: ViewDetail[];
  /** Snapshot manifest rows by view name, served through the files
   * endpoint's q/offset/limit semantics (api.rs view_files_body). */
  files?: Record<string, ViewFileRow[]>;
  admin?: AdminUsersBody;
  /** Non-200 for /v1/admin/users (e.g. 403 exercises owner-only). */
  adminStatus?: number;
  minted?: MintedInvite;
  /** POST /v1/dats/import receipt; default is a minimal one. */
  datImport?: DatImportBody;
  /** Dat import answers 400 — exercises the refused-file log line. */
  datImportFail?: boolean;
  /** Grant/revoke answer 500 — exercises the optimistic revert. */
  grantFail?: boolean;
  /** Grant/revoke wait on this before answering — lets a test observe
   * the optimistic state while the request is in flight. */
  grantGate?: Promise<unknown>;
}

export const emptyStorage: StorageBody = {
  blob_count: 0,
  on_disk_bytes: 0,
  represented_bytes: 0,
  literal_only_bytes: 0,
  quarantine: { count: 0, items: [] },
};

function json(status: number, body: unknown): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { 'content-type': 'application/json' },
  });
}

/** Derive a plausible detail body from a listing row (revision r1). */
function defaultDetail(row: EntryRow): EntryDetail {
  return {
    name: row.name,
    state: row.state,
    size: row.size,
    wanted_hash: row.wanted_hash,
    wanted_hash_algo: row.wanted_hash_algo,
    revision: { id: 1, version: 'r1', date: null, imported_at: 1000 },
    roms: [],
  };
}

/** Install the stub; returns the vi.fn so tests can inspect calls. */
export function installFetch(universe: MockUniverse) {
  const entries = universe.entries ?? [];
  const handler = vi.fn(
    async (input: RequestInfo | URL, init?: RequestInit): Promise<Response> => {
      const url = new URL(String(input instanceof Request ? input.url : input), 'http://mock');
      const path = decodeURIComponent(url.pathname);
      const method = (input instanceof Request ? input.method : init?.method) ?? 'GET';

      if (path === '/v1/auth/whoami') {
        return json(200, universe.whoami ?? { authenticated: false });
      }
      if (path === '/v1/systems') {
        return json(200, { systems: universe.systems ?? [] });
      }
      const detailMatch = path.match(/^\/v1\/systems\/[^/]+\/entries\/(.+)$/);
      if (detailMatch) {
        const name = detailMatch[1];
        const body =
          universe.detail?.(name) ??
          (() => {
            const row = entries.find((e) => e.name === name);
            return row ? defaultDetail(row) : undefined;
          })();
        return body ? json(200, body) : json(404, { error: 'no such entry' });
      }
      if (/^\/v1\/systems\/[^/]+\/entries$/.test(path)) {
        const q = url.searchParams.get('q')?.toLowerCase() ?? null;
        const state = url.searchParams.get('state');
        const offset = Number(url.searchParams.get('offset') ?? 0);
        const limit = Math.min(Number(url.searchParams.get('limit') ?? 200), 1000);
        const filtered = entries.filter(
          (e) =>
            (q === null || e.name.toLowerCase().includes(q)) &&
            (state === null || e.state === state),
        );
        return json(200, {
          entries: filtered.slice(offset, offset + limit),
          total: filtered.length,
          offset,
          limit,
        });
      }
      if (path === '/v1/views') {
        return json(200, { views: universe.views ?? [] });
      }
      const filesMatch = path.match(/^\/v1\/views\/([^/]+)\/files$/);
      if (filesMatch) {
        const view = (universe.views ?? []).find((v) => v.name === filesMatch[1]);
        if (!view) {
          return json(404, { error: 'no such view' });
        }
        const q = url.searchParams.get('q')?.toLowerCase() ?? null;
        const offset = Number(url.searchParams.get('offset') ?? 0);
        const limit = Math.min(Number(url.searchParams.get('limit') ?? 200), 1000);
        const rows = universe.files?.[view.name] ?? [];
        const filtered = rows.filter((f) => q === null || f.path.toLowerCase().includes(q));
        return json(200, {
          files: filtered.slice(offset, offset + limit),
          total: filtered.length,
          offset,
          limit,
          snapshot: view.snapshot ?? '0'.repeat(64),
        });
      }
      const viewMatch = path.match(/^\/v1\/views\/(.+)$/);
      if (viewMatch) {
        const view = (universe.views ?? []).find((v) => v.name === viewMatch[1]);
        return view ? json(200, view) : json(404, { error: 'no such view' });
      }
      if (path === '/v1/storage') {
        return json(200, universe.storage ?? emptyStorage);
      }
      if (path === '/v1/jobs') {
        return json(200, { jobs: universe.jobs ?? [] });
      }
      const jobMatch = path.match(/^\/v1\/jobs\/(\d+)$/);
      if (jobMatch) {
        const timeline = universe.jobTimeline;
        if (timeline === undefined || timeline.length === 0) {
          return json(404, { error: 'no such job' });
        }
        const detail = timeline.length > 1 ? timeline.shift() : timeline[0];
        return json(200, detail);
      }
      if (path === '/v1/ingest' && method === 'POST') {
        return universe.ingestFail === true
          ? json(400, { error: 'unknown or expired upload: tok-x' })
          : json(200, { job: universe.ingestJob ?? 1 });
      }
      if (path === '/v1/admin/users') {
        if (universe.adminStatus !== undefined) {
          return json(universe.adminStatus, { error: 'owner only' });
        }
        return json(200, universe.admin ?? { users: [], invites: [] });
      }
      if (path === '/v1/dats/import' && method === 'POST') {
        if (universe.datImportFail === true) {
          return json(400, { error: 'unknown dat format' });
        }
        return json(
          200,
          universe.datImport ?? {
            source_id: 1,
            revision_id: 1,
            dat_blob: '0'.repeat(64),
            provider: 'unknown',
            system: 'unknown',
            entries: 0,
            claims: 0,
            demoted_revisions: [],
          },
        );
      }
      if (path === '/v1/admin/invites' && method === 'POST') {
        return json(200, universe.minted ?? { url_path: '/invite#tok-mock', expires_at: 4200 });
      }
      if (
        (path === '/v1/admin/grants' && method === 'POST') ||
        (/^\/v1\/admin\/grants\/[^/]+\/[^/]+$/.test(path) && method === 'DELETE')
      ) {
        await universe.grantGate;
        return universe.grantFail === true
          ? json(500, { error: 'induced grant failure' })
          : json(200, { ok: true });
      }
      if (/^\/v1\/admin\/sessions\/[^/]+$/.test(path) && method === 'DELETE') {
        return json(200, { revoked: 1 });
      }
      return json(404, { error: `unmocked route ${path}` });
    },
  );
  vi.stubGlobal('fetch', handler);
  return handler;
}

/**
 * Fake XMLHttpRequest for the uploadRom path (XHR doesn't ride the
 * fetch stub): fires one scripted progress event then onload. Returns
 * the record of what was "sent" so tests assert names and sizes.
 */
export function installUploadXhr(opts: { fail?: boolean } = {}) {
  const sent: { name: string; size: number }[] = [];
  let count = 0;
  class FakeXhr {
    upload: {
      onprogress:
        | null
        | ((e: { lengthComputable: boolean; loaded: number; total: number }) => void);
    } = { onprogress: null };

    onload: (() => void) | null = null;
    onerror: (() => void) | null = null;
    status = 0;
    responseText = '';
    private url = '';

    open(_method: string, url: string): void {
      this.url = url;
    }

    setRequestHeader(): void {}

    send(body: Blob): void {
      const name = decodeURIComponent(this.url.split('name=')[1] ?? '');
      const size = body.size;
      sent.push({ name, size });
      count += 1;
      const token = `tok-${count}`;
      queueMicrotask(() => {
        this.upload.onprogress?.({ lengthComputable: true, loaded: size, total: size });
        if (opts.fail === true) {
          this.status = 400;
          this.responseText = JSON.stringify({ error: 'induced upload failure' });
        } else {
          this.status = 200;
          this.responseText = JSON.stringify({ upload: token, bytes: size });
        }
        this.onload?.();
      });
    }
  }
  vi.stubGlobal('XMLHttpRequest', FakeXhr as unknown as typeof XMLHttpRequest);
  return sent;
}

/** Stub navigator.clipboard (happy-dom's needs permission wiring);
 * returns the writeText spy. */
export function installClipboard() {
  const writeText = vi.fn(() => Promise.resolve());
  Object.defineProperty(navigator, 'clipboard', {
    value: { writeText },
    configurable: true,
  });
  return writeText;
}
