import { afterEach, describe, expect, test, vi } from 'vitest';
import { router } from '../router.svelte';
import { ApiError, entryDetail, login, onUnauthorized, systemEntries, systems } from './client';

function stub(status: number, body: unknown, contentType = 'application/json') {
  const fn = vi.fn(
    async (_input: RequestInfo | URL, _init?: RequestInit) =>
      new Response(typeof body === 'string' ? body : JSON.stringify(body), {
        status,
        headers: { 'content-type': contentType },
      }),
  );
  vi.stubGlobal('fetch', fn);
  return fn;
}

afterEach(() => {
  vi.unstubAllGlobals();
  onUnauthorized(() => {});
  router.replace('/');
});

describe('401 handling', () => {
  test('session endpoints: 401 fires the handler and bounces to /login', async () => {
    stub(401, 'authentication required', 'text/plain');
    const handler = vi.fn();
    onUnauthorized(handler);
    await expect(systems()).rejects.toMatchObject({ status: 401 });
    expect(handler).toHaveBeenCalledOnce();
    expect(window.location.pathname).toBe('/login');
  });

  test('login: 401 means bad credentials, not session death — no redirect', async () => {
    stub(401, 'invalid credentials', 'text/plain');
    const handler = vi.fn();
    onUnauthorized(handler);
    const err = await login('sam', 'nope').catch((e: unknown) => e);
    expect(err).toBeInstanceOf(ApiError);
    expect((err as ApiError).status).toBe(401);
    expect((err as ApiError).message).toBe('invalid credentials');
    expect(handler).not.toHaveBeenCalled();
    expect(window.location.pathname).toBe('/');
  });
});

describe('error bodies', () => {
  test('api.rs JSON errors surface the message', async () => {
    stub(404, { error: 'no such system' });
    const err = await systems().catch((e: unknown) => e);
    expect(err).toBeInstanceOf(ApiError);
    expect((err as ApiError).message).toBe('no such system');
    expect((err as ApiError).status).toBe(404);
  });
});

/** The client sends whole Requests (openapi-fetch); pull the wire URL. */
function sentUrl(fn: ReturnType<typeof stub>): URL {
  const input = fn.mock.calls[0][0];
  return new URL(String(input instanceof Request ? input.url : input), 'http://mock');
}

describe('request shapes', () => {
  test('systemEntries composes q/state/offset/limit into the query', async () => {
    const fn = stub(200, { entries: [], total: 0, offset: 10, limit: 50 });
    await systemEntries(3, { q: 'zelda (usa)', state: 'missing', offset: 10, limit: 50 });
    const url = sentUrl(fn);
    expect(url.pathname).toBe('/v1/systems/3/entries');
    expect(Object.fromEntries(url.searchParams)).toEqual({
      q: 'zelda (usa)',
      state: 'missing',
      offset: '10',
      limit: '50',
    });
  });

  test('bare systemEntries sends no query string', async () => {
    const fn = stub(200, { entries: [], total: 0, offset: 0, limit: 200 });
    await systemEntries(3);
    const url = sentUrl(fn);
    expect(url.pathname).toBe('/v1/systems/3/entries');
    expect(url.search).toBe('');
  });

  test('entryDetail percent-encodes the name path param', async () => {
    const fn = stub(200, {});
    await entryDetail(3, 'Zelda / Link (USA)');
    expect(sentUrl(fn).pathname).toBe('/v1/systems/3/entries/Zelda%20%2F%20Link%20(USA)');
  });

  test('requests carry same-origin credentials (cookie sessions, D68)', async () => {
    const fn = stub(200, { systems: [] });
    await systems();
    const request = fn.mock.calls[0][0] as Request;
    expect(request.credentials).toBe('same-origin');
  });
});
