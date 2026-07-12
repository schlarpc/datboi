import { beforeEach, describe, expect, test } from 'vitest';
import { loginReturn, matchPath, router } from './router.svelte';

describe('matchPath', () => {
  test.each([
    ['/', 'library'],
    ['/views', 'views'],
    ['/ingest', 'ingest'],
    ['/storage', 'storage'],
    ['/admin', 'admin'],
    ['/login', 'login'],
    ['/invite', 'invite'],
  ] as const)('%s → %s', (path, screen) => {
    expect(matchPath(path).screen).toBe(screen);
  });

  test('library drill-down carries the system id', () => {
    expect(matchPath('/library/3')).toEqual({ screen: 'audit', systemId: '3' });
  });

  test('drill-down segment is percent-decoded', () => {
    expect(matchPath('/library/a%20b')).toEqual({ screen: 'audit', systemId: 'a b' });
  });

  test('blob inspector carries the hash', () => {
    const hash = 'ab'.repeat(32);
    expect(matchPath(`/storage/blob/${hash}`)).toEqual({ screen: 'blob', hash });
  });

  test('friend browse carries the view name, percent-decoded', () => {
    expect(matchPath('/shelf/gba-everdrive')).toEqual({
      screen: 'browse',
      view: 'gba-everdrive',
    });
    expect(matchPath('/shelf/a%20b')).toEqual({ screen: 'browse', view: 'a b' });
  });

  test.each([
    '/bogus',
    '/library',
    '/library/3/extra',
    '/viewsx',
    '/shelf',
    '/shelf/x/y',
    '/storage/blob',
    '/storage/blob/x/y',
  ])(
    'unknown path %s is notfound',
    (path) => {
      expect(matchPath(path).screen).toBe('notfound');
    },
  );

  test.each([
    '/library/abc%',
    '/library/%zz',
    '/shelf/abc%',
    '/storage/blob/ab%',
  ])(
    'malformed percent-sequence %s is notfound, not a throw',
    (path) => {
      expect(() => matchPath(path)).not.toThrow();
      expect(matchPath(path).screen).toBe('notfound');
    },
  );
});

describe('router', () => {
  beforeEach(() => {
    router.replace('/');
  });

  test('navigate pushes history and swaps the route', () => {
    router.navigate('/views');
    expect(window.location.pathname).toBe('/views');
    expect(router.route.screen).toBe('views');
  });

  test('replace swaps without stacking (redirects)', () => {
    router.replace('/login');
    expect(window.location.pathname).toBe('/login');
    expect(router.route.screen).toBe('login');
  });

  test('popstate re-syncs from location (back/forward)', () => {
    // Simulate the browser restoring an older entry.
    window.history.pushState({}, '', '/storage');
    window.dispatchEvent(new PopStateEvent('popstate'));
    expect(router.route.screen).toBe('storage');
  });
});

test('loginReturn: the bounce destination round-trips once, then defaults home', () => {
  loginReturn.stash('/storage/blob/abc');
  expect(loginReturn.consume()).toBe('/storage/blob/abc');
  expect(loginReturn.consume()).toBe('/'); // consumed — no stale replay
});

test('loginReturn: the open pages are never a return destination', () => {
  loginReturn.stash('/login');
  expect(loginReturn.consume()).toBe('/');
  loginReturn.stash('/invite');
  expect(loginReturn.consume()).toBe('/');
});
