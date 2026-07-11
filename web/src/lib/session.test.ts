import { afterEach, describe, expect, test, vi } from 'vitest';
import { installFetch } from '../test/mock-api';
import { session } from './session.svelte';
import { systems } from './api/client';

afterEach(() => {
  vi.unstubAllGlobals();
});

describe('session store transitions', () => {
  test('boot: loopback whoami lands authenticated with no username', async () => {
    installFetch({ whoami: { authenticated: true, role: 'owner', via: 'loopback' } });
    await session.init();
    expect(session.status).toBe('authenticated');
    expect(session.username).toBeNull();
    expect(session.role).toBe('owner');
    expect(session.via).toBe('loopback');
  });

  test('boot: anonymous whoami lands on anonymous', async () => {
    installFetch({ whoami: { authenticated: false } });
    await session.init();
    expect(session.status).toBe('anonymous');
    expect(session.username).toBeNull();
  });

  test('boot: network failure fails closed to anonymous', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => {
        throw new Error('daemon unreachable');
      }),
    );
    await session.init();
    expect(session.status).toBe('anonymous');
  });

  test('apply: a login answer flips to authenticated with the username', () => {
    session.apply({ authenticated: true, username: 'sam', role: 'friend', expires_at: 9999 });
    expect(session.status).toBe('authenticated');
    expect(session.username).toBe('sam');
    expect(session.role).toBe('friend');
    expect(session.via).toBe('session');
  });

  test('a mid-flight 401 flips the store to anonymous', async () => {
    session.apply({ authenticated: true, username: 'sam', role: 'friend', expires_at: 9999 });
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => new Response('authentication required', { status: 401 })),
    );
    await expect(systems()).rejects.toMatchObject({ status: 401 });
    expect(session.status).toBe('anonymous');
  });

  test('logout clears even when the request fails', async () => {
    session.apply({ authenticated: true, username: 'sam', role: 'friend', expires_at: 9999 });
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => {
        throw new Error('gone');
      }),
    );
    await session.logout().catch(() => {});
    expect(session.status).toBe('anonymous');
    expect(session.username).toBeNull();
  });
});
