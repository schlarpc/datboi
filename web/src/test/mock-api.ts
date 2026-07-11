/**
 * Test-only fetch stub that emulates the /v1 read-model API over
 * in-memory fixtures, including the server's entries query semantics
 * (case-insensitive substring `q`, `state` filter, offset/limit paging
 * — api.rs parse_page/entries_body) so component tests exercise real
 * filter+search composition instead of canned responses.
 */

import { vi } from 'vitest';
import type {
  EntryDetail,
  EntryRow,
  Job,
  StorageBody,
  System,
  Whoami,
} from '../lib/api/types';

export interface MockUniverse {
  whoami?: Whoami;
  systems?: System[];
  entries?: EntryRow[];
  /** Entry detail by name; default derives a minimal one from the row. */
  detail?: (name: string) => EntryDetail | undefined;
  storage?: StorageBody;
  jobs?: Job[];
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
  const handler = vi.fn(async (input: RequestInfo | URL): Promise<Response> => {
    const url = new URL(String(input instanceof Request ? input.url : input), 'http://mock');
    const path = decodeURIComponent(url.pathname);

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
    if (path === '/v1/storage') {
      return json(200, universe.storage ?? emptyStorage);
    }
    if (path === '/v1/jobs') {
      return json(200, { jobs: universe.jobs ?? [] });
    }
    return json(404, { error: `unmocked route ${path}` });
  });
  vi.stubGlobal('fetch', handler);
  return handler;
}
