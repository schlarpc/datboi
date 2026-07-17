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
  BlobDetail,
  BlobRow,
  DatImportBody,
  EntryDetail,
  EntryRow,
  Job,
  JobDetailBody,
  MintedInvite,
  SnapshotBody,
  StorageBody,
  StorageBreakdownBody,
  System,
  ViewDetail,
  ViewFileRow,
  Whoami,
  OrphansBody,
} from '../lib/api/types';

export interface MockUniverse {
  whoami?: Whoami;
  systems?: System[];
  entries?: EntryRow[];
  /** Entry detail by name; default derives a minimal one from the row. */
  detail?: (name: string) => EntryDetail | undefined;
  storage?: StorageBody;
  /** GET /v1/storage/breakdown aggregates. */
  breakdown?: StorageBreakdownBody;
  /** GET /v1/blobs rows, served through the endpoint's q(hex-prefix)/
   * ns/residency/offset/limit semantics (api.rs blobs_body). */
  blobRows?: BlobRow[];
  /** GET /v1/blobs/{hash} bodies by lowercase hash; misses answer 404
   * (non-hex answers 400 like the server). */
  blobDetails?: Record<string, BlobDetail>;
  /** GET /v1/gc/orphans body (D73 review surface); defaults empty. */
  orphans?: OrphansBody;
  /** GET /v1/gc/orphans answers 500 — exercises the per-card error. */
  orphansFail?: boolean;
  /** Entries pages with offset ≥ this answer 500 (0 = every page) —
   * exercises rows-only errors and the load-more rejection path. */
  entriesFailFromOffset?: number;
  /** GET /v1/jobs rows (the in-memory registry's tray render). */
  jobs?: Job[];
  /** GET /v1/jobs answers 500 — exercises the tray's unreachable arm.
   * Mutable mid-test: flip it to script a daemon blip and recovery. */
  jobsFail?: boolean;
  /** GET /v1/jobs/{id} script: each poll SHIFTS one entry until the
   * last, which then repeats — tests script running→done timelines. */
  jobTimeline?: JobDetailBody[];
  /** GET /v1/jobs/{id} answers 500 — exercises followJob's grace.
   * Mutable mid-test: flip it to script a blip and recovery. */
  jobDetailFail?: boolean;
  /** POST /v1/ingest answer; defaults to job 1. */
  ingestJob?: number;
  /** POST /v1/blobs/{hash}/verify answer (D80); defaults to job 1. */
  verifyJob?: number;
  /** POST /v1/scrub answer (D96 maintenance); defaults to job 1. */
  scrubJob?: number;
  /** POST /v1/snapshot receipt (D96); default is a minimal one. */
  snapshot?: SnapshotBody;
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
  last_scrub: null,
};

export const emptyBreakdown: StorageBreakdownBody = {
  by_class: [],
  by_source: [],
  largest: [],
};

/** The path of a recorded fetch call — fetch may get a string, URL, or
 * Request (the openapi-fetch client sends Requests). */
export function calledPath(input: RequestInfo | URL): string {
  return new URL(String(input instanceof Request ? input.url : input), 'http://mock').pathname;
}

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
        if (
          universe.entriesFailFromOffset !== undefined &&
          offset >= universe.entriesFailFromOffset
        ) {
          return json(500, { error: 'induced entries failure' });
        }
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
      if (path === '/v1/storage/breakdown') {
        return json(200, universe.breakdown ?? emptyBreakdown);
      }
      if (path === '/v1/blobs') {
        const q = url.searchParams.get('q')?.toLowerCase() ?? null;
        const ns = url.searchParams.get('ns');
        const residency = url.searchParams.get('residency');
        const offset = Number(url.searchParams.get('offset') ?? 0);
        const limit = Math.min(Number(url.searchParams.get('limit') ?? 200), 1000);
        const filtered = (universe.blobRows ?? []).filter(
          (b) =>
            (q === null || b.hash.startsWith(q)) &&
            (ns === null || b.namespace === ns) &&
            (residency === null || b.residency === residency),
        );
        return json(200, {
          blobs: filtered.slice(offset, offset + limit),
          total: filtered.length,
          offset,
          limit,
        });
      }
      // Before the detail matcher: its greedy (.+) would eat the
      // /verify suffix and answer 400.
      if (/^\/v1\/blobs\/[0-9a-f]{64}\/verify$/i.test(path) && method === 'POST') {
        return json(202, { job: universe.verifyJob ?? 1 });
      }
      const blobMatch = path.match(/^\/v1\/blobs\/(.+)$/);
      if (blobMatch) {
        const hash = blobMatch[1].toLowerCase();
        if (!/^[0-9a-f]{64}$/.test(hash)) {
          return json(400, { error: 'not a blake3 hex hash' });
        }
        const body = universe.blobDetails?.[hash];
        return body ? json(200, body) : json(404, { error: 'no such blob' });
      }
      if (path === '/v1/gc/orphans' && method === 'GET') {
        if (universe.orphansFail === true) {
          return json(500, { error: 'induced orphans failure' });
        }
        return json(
          200,
          universe.orphans ?? { orphans: [], reclaimable_bytes: 0, grace_secs: 86_400 },
        );
      }
      if (path === '/v1/gc/keep' && method === 'POST') {
        return json(200, { ok: true });
      }
      if (path === '/v1/gc/orphans/apply' && method === 'POST') {
        return json(200, { deleted: 0, bytes_reclaimed: 0, skipped: 0 });
      }
      if (path === '/v1/scrub' && method === 'POST') {
        return json(202, { job: universe.scrubJob ?? 1 });
      }
      if (path === '/v1/snapshot' && method === 'POST') {
        return json(
          200,
          universe.snapshot ?? {
            hash: '0'.repeat(64),
            sequence: 1,
            sources: 0,
            alias_rows: 0,
            analysis_rows: 0,
            new_batch_blobs: 0,
          },
        );
      }
      if (path === '/v1/jobs') {
        if (universe.jobsFail === true) {
          return json(500, { error: 'induced jobs failure' });
        }
        return json(200, { jobs: universe.jobs ?? [] });
      }
      const jobMatch = path.match(/^\/v1\/jobs\/(\d+)$/);
      if (jobMatch) {
        if (universe.jobDetailFail === true) {
          return json(500, { error: 'induced job-detail failure' });
        }
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
