import { render, screen, fireEvent } from '@testing-library/svelte';
import { loadLocale } from 'wuchale/load-utils';
import { afterEach, expect, test, vi } from 'vitest';
import '../locales/main.loader.svelte.js';
import type { ViewDetail } from '../lib/api/types';
import { router } from '../lib/router.svelte';
import { installFetch } from '../test/mock-api';
import Shelves from './Shelves.svelte';

await loadLocale('en');

const nowSecs = Math.floor(Date.now() / 1000);

const shelf: ViewDetail = {
  name: 'gba-everdrive',
  snapshot: `a41f${'0'.repeat(60)}`,
  definition: {
    provider: 'no-intro',
    system: 'gba',
    template: 'Games/{name}.gba',
    one_g_one_r: null,
    profile: 'everdrive',
    image: { cluster_size: 512, partition: true, label: null },
    mame_mode: null,
  },
  rows: 1412,
  bytes: 5.8 * 1024 ** 3,
  created_at: nowSecs - 2 * 3600,
  endpoints: { http: '/view/gba-everdrive/', dav: '/dav/gba-everdrive/' },
  image: { minted: true, hash: 'ab'.repeat(32), bytes: 14.2 * 1024 ** 3 },
};

afterEach(() => {
  vi.unstubAllGlobals();
  router.replace('/');
});

test('shelf cards compose sub, stats, and the trust line', async () => {
  installFetch({ views: [shelf] });
  render(Shelves);

  expect(await screen.findByText('gba-everdrive')).toBeTruthy();
  expect(screen.getByText('browse →')).toBeTruthy();

  const sub = document.querySelector('.sub-line');
  expect(sub?.textContent?.replace(/\s+/g, ' ').trim()).toBe('gba · no-intro · everdrive layout');

  const stats = document.querySelector('.stats');
  expect(stats?.textContent?.replace(/\s+/g, ' ').trim()).toBe(
    '1,412 files · 5.8 GB snap #a41f · 2h ago',
  );

  // The verified promise is stated at the shelf (spec §4.2).
  expect(screen.getByText('● verified — hash-checked as it streams')).toBeTruthy();
});

test('the whole card clicks into browse', async () => {
  installFetch({ views: [shelf] });
  render(Shelves);
  await screen.findByText('gba-everdrive');

  await fireEvent.click(document.querySelector('.card')!);
  expect(window.location.pathname).toBe('/shelf/gba-everdrive');
});

test('no grants renders the warm empty state, not a broken grid', async () => {
  installFetch({ views: [] });
  render(Shelves);

  expect(
    await screen.findByText('nothing on your shelves yet — ask the owner for a grant'),
  ).toBeTruthy();
});
