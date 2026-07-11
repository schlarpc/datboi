import { render, screen } from '@testing-library/svelte';
import { loadLocale } from 'wuchale/load-utils';
import { afterEach, expect, test, vi } from 'vitest';
import App from './App.svelte';
import { router } from './lib/router.svelte';
import { installFetch } from './test/mock-api';

// happy-dom emulates the client, so catalogs load the client way: register
// the loaders (App imports them) and await the locale before rendering.
await loadLocale('en');

afterEach(() => {
  vi.unstubAllGlobals();
  router.replace('/');
});

test('anonymous boot redirects to the login card', async () => {
  installFetch({ whoami: { authenticated: false } });
  render(App);

  expect(await screen.findByText('log in')).toBeTruthy();
  expect(window.location.pathname).toBe('/login');
  // The card is the open page — no owner chrome leaks out.
  expect(screen.queryByText('Library')).toBeNull();
});

test('authenticated boot (loopback owner) lands in the shell', async () => {
  installFetch({
    whoami: { authenticated: true, role: 'owner', via: 'loopback' },
    systems: [],
  });
  render(App);

  // Shell chrome: nav, wordmark, jobs tray, and the Library home.
  expect(await screen.findByText('The shelf')).toBeTruthy();
  expect(screen.getByText('Library')).toBeTruthy();
  expect(screen.getByText('datboi')).toBeTruthy();
  expect(await screen.findByText('▸ jobs (0)')).toBeTruthy();
});

test('a named session shows the avatar initial', async () => {
  installFetch({
    whoami: { authenticated: true, username: 'sam', role: 'owner', via: 'session' },
    systems: [],
  });
  render(App);

  await screen.findByText('The shelf');
  expect(screen.getByTitle('sam').textContent).toBe('s');
});

test('a friend session gets friend chrome — no owner surfaces leak', async () => {
  installFetch({
    whoami: { authenticated: true, username: 'sam', role: 'friend', via: 'session' },
    views: [],
  });
  render(App);

  // Shelves home + account chip; no nav tabs, no jobs tray, no health chip.
  expect(await screen.findByText('Your shelves')).toBeTruthy();
  expect(screen.getByText('shared with you by the owner')).toBeTruthy();
  expect(screen.getByText('sam')).toBeTruthy();
  expect(screen.queryByText('Library')).toBeNull();
  expect(screen.queryByText('Ingest')).toBeNull();
  expect(screen.queryByText('Admin')).toBeNull();
  expect(screen.queryByText(/▸ jobs/)).toBeNull();
  expect(screen.queryByText('healthy')).toBeNull();
});

test('owner routes refuse friends — bounced back to the shelves', async () => {
  installFetch({
    whoami: { authenticated: true, username: 'sam', role: 'friend', via: 'session' },
    views: [],
  });
  router.replace('/admin');
  render(App);

  expect(await screen.findByText('Your shelves')).toBeTruthy();
  expect(window.location.pathname).toBe('/');
  expect(screen.queryByText('+ mint invite URL')).toBeNull();
});
