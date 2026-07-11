import { fireEvent, render, screen } from '@testing-library/svelte';
import { loadLocale } from 'wuchale/load-utils';
import { afterEach, expect, test, vi } from 'vitest';
import '../locales/main.loader.svelte.js';
import type { ViewDetail } from '../lib/api/types';
import { installClipboard, installFetch } from '../test/mock-api';
import Views from './Views.svelte';

await loadLocale('en');

const nowSecs = Math.floor(Date.now() / 1000);

/** Image-profile view (EverDrive-style): primary verb = SD export. */
const gbaView: ViewDetail = {
  name: 'gba-everdrive',
  snapshot: `a41f${'0'.repeat(60)}`,
  definition: {
    provider: 'no-intro',
    system: 'gba',
    template: 'Games/{name}.gba',
    one_g_one_r: { mode: 'held_first', regions: ['USA', 'Europe', 'Japan'], langs: [] },
    profile: 'everdrive',
    image: { cluster_size: 32 * 1024, partition: true, label: null },
    mame_mode: null,
  },
  rows: 1412,
  bytes: 5_800_000_000,
  created_at: nowSecs - 2 * 3600,
  endpoints: { http: '/view/gba-everdrive/', dav: '/dav/gba-everdrive/' },
  image: { minted: false },
};

/** Mount-style view (no image params): primary verb = copy link. */
const snesView: ViewDetail = {
  name: 'snes-mister',
  snapshot: `9e02${'0'.repeat(60)}`,
  definition: {
    provider: 'no-intro',
    system: 'snes',
    template: '{name}',
    one_g_one_r: { mode: 'strict', regions: ['USA', 'Europe'], langs: [] },
    profile: 'mister',
    image: null,
    mame_mode: null,
  },
  rows: 900,
  bytes: 3_000_000_000,
  created_at: nowSecs - 50 * 3600,
  endpoints: { http: '/view/snes-mister/', dav: '/dav/snes-mister/' },
  image: null,
};

afterEach(() => vi.unstubAllGlobals());

test('cards compose sub-line, stats, and per-profile verb from the detail', async () => {
  installFetch({ views: [gbaView, snesView] });
  render(Views);

  expect(await screen.findByText('gba-everdrive')).toBeTruthy();
  expect(screen.getByText('snes-mister')).toBeTruthy();

  // Sub-line: system · 1G1R mode (regions) · profile.
  const subs = [...document.querySelectorAll('.sub-line')].map((el) =>
    el.textContent?.replace(/\s+/g, ' ').trim(),
  );
  expect(subs).toEqual([
    'gba · 1G1R held-first (USA›Europe›Japan) · everdrive profile',
    'snes · 1G1R strict (USA›Europe) · mister profile',
  ]);

  // Stats: rows + snap short-hash + age (no missing/clean claim — the
  // API stores no eval report).
  expect(screen.getByText('1,412 files')).toBeTruthy();
  const stats = [...document.querySelectorAll('.stats')].map((el) =>
    el.textContent?.replace(/\s+/g, ' ').trim(),
  );
  expect(stats[0]).toContain('snap #a41f · 2h');
  expect(stats[1]).toContain('snap #9e02 · 2d');

  // Verb selection: image params → SD export; otherwise copy link.
  expect(screen.getByText('⬇ Export SD image')).toBeTruthy();
  expect(screen.getByText('⎘ copy link')).toBeTruthy();

  // Footnotes: saves warning on the image card, endpoint on the mount card.
  expect(screen.getByText('⚠ flashing overwrites on-device saves')).toBeTruthy();
  expect(screen.getByText(`${location.origin}/view/snes-mister/ · read-only`)).toBeTruthy();
});

test('SD export: unminted reveals the mint CLI hint; minted is a real download', async () => {
  // Unminted image profile: minting is CLI-only, so the verb reveals it.
  installFetch({ views: [gbaView] });
  const { unmount } = render(Views);
  await screen.findByText('gba-everdrive');

  expect(screen.queryByText(/datboi view image/)).toBeNull();
  await fireEvent.click(screen.getByText('⬇ Export SD image'));
  expect(screen.getByText('datboi view image gba-everdrive')).toBeTruthy();
  unmount();

  // Minted: the verb is an anchor into the verified image route.
  installFetch({
    views: [{ ...gbaView, image: { minted: true, hash: 'ab'.repeat(32), bytes: 1024 } }],
  });
  render(Views);
  await screen.findByText('gba-everdrive');
  const anchor = screen.getByText('⬇ Export SD image') as HTMLAnchorElement;
  expect(anchor.getAttribute('href')).toBe('/v1/views/gba-everdrive/image');
  expect(anchor.getAttribute('download')).toBe('gba-everdrive.img');
});

test('copy link puts the absolute HTTP endpoint on the clipboard', async () => {
  const writeText = installClipboard();
  installFetch({ views: [snesView] });
  render(Views);
  await screen.findByText('snes-mister');

  await fireEvent.click(screen.getByText('⎘ copy link'));
  expect(writeText).toHaveBeenCalledWith(`${location.origin}/view/snes-mister/`);
  expect(await screen.findByText('copied ✓')).toBeTruthy();
});

test('⋯ menu: webdav copy is real; definition fold shows read-only fields', async () => {
  const writeText = installClipboard();
  installFetch({ views: [gbaView] });
  render(Views);
  await screen.findByText('gba-everdrive');

  await fireEvent.click(screen.getByLabelText('view actions'));
  await fireEvent.click(screen.getByText('⎘ webdav url'));
  expect(writeText).toHaveBeenCalledWith(`${location.origin}/dav/gba-everdrive/`);

  await fireEvent.click(screen.getByText('definition'));
  expect(screen.getByText('no-intro/gba')).toBeTruthy();
  expect(screen.getByText('Games/{name}.gba')).toBeTruthy();
  expect(screen.getByText('not minted yet')).toBeTruthy();
  expect(screen.getByText(/datboi view define gba-everdrive/)).toBeTruthy();
});

test('re-eval and + new view are CLI hints; browse links into the served tree', async () => {
  installFetch({ views: [gbaView] });
  render(Views);
  await screen.findByText('gba-everdrive');

  await fireEvent.click(screen.getByText('re-eval'));
  expect(screen.getByText('datboi view eval gba-everdrive')).toBeTruthy();

  const browse = screen.getByText('browse') as HTMLAnchorElement;
  expect(browse.getAttribute('href')).toBe('/view/gba-everdrive/');
  expect(browse.getAttribute('target')).toBe('_blank');

  await fireEvent.click(screen.getByText('+ new view'));
  expect(screen.getByText(/datboi view define <name>/)).toBeTruthy();
});
