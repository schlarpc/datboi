import { fireEvent, render, screen } from '@testing-library/svelte';
import { loadLocale } from 'wuchale/load-utils';
import { afterEach, expect, test, vi } from 'vitest';
import '../locales/main.loader.svelte.js';
import type { System } from '../lib/api/types';
import { installFetch } from '../test/mock-api';
import Library from './Library.svelte';

await loadLocale('en');

const gba: System = {
  id: 1,
  provider: 'no-intro',
  system: 'gba',
  source: 'no-intro/gba',
  revision: { id: 1, version: 'r2026-07', date: null, imported_at: 1000 },
  counts: { verified: 2730, claimed: 160, missing: 240, nodump: 12 },
  total: 3142,
  views: ['gba-everdrive', 'gba-flash'],
};

const snes: System = {
  id: 2,
  provider: 'no-intro',
  system: 'snes',
  source: 'no-intro/snes',
  revision: { id: 2, version: 'r2026-06', date: null, imported_at: 1000 },
  counts: { verified: 2190, claimed: 548, missing: 912, nodump: 0 },
  total: 3650,
  views: [],
};

afterEach(() => vi.unstubAllGlobals());

test('cards render counts, revision, and the subtitle totals', async () => {
  installFetch({ systems: [gba, snes] });
  render(Library);

  expect(await screen.findByText('gba')).toBeTruthy();
  expect(screen.getByText('snes')).toBeTruthy();
  expect(screen.getByText('2 systems · 6,792 entries')).toBeTruthy();
  expect(screen.getByText('2,730 verified')).toBeTruthy();
  expect(screen.getByText('160 claimed')).toBeTruthy();
  expect(screen.getByText('240 missing')).toBeTruthy();
  expect(screen.getByText('no-intro r2026-07')).toBeTruthy();
});

test('completeness color thresholds: green ≥90, amber below', async () => {
  const green: System = {
    ...gba,
    counts: { verified: 95, claimed: 3, missing: 2, nodump: 0 },
    total: 100,
  };
  const amber: System = {
    ...snes,
    counts: { verified: 75, claimed: 10, missing: 15, nodump: 0 },
    total: 100,
  };
  installFetch({ systems: [green, amber] });
  render(Library);

  const greenPct = await screen.findByText('95%');
  expect(greenPct.classList.contains('pct-ok')).toBe(true);
  const amberPct = screen.getByText('75%');
  expect(amberPct.classList.contains('pct-warn')).toBe(true);
});

test('bands: known systems get spec tokens deterministically', async () => {
  installFetch({ systems: [gba, snes] });
  render(Library);
  await screen.findByText('gba');

  const bands = [...document.querySelectorAll<HTMLElement>('.band')];
  expect(bands.map((b) => b.style.background)).toEqual([
    'var(--band-gba)',
    'var(--band-snes)',
  ]);
});

test('views chips render; empty card reveals the CLI hint on click', async () => {
  installFetch({ systems: [gba, snes] });
  render(Library);
  await screen.findByText('gba');

  expect(screen.getByText('gba-everdrive')).toBeTruthy();
  expect(screen.getByText('gba-flash')).toBeTruthy();

  // Dat import is CLI-only in M5: the dashed card reveals the command.
  expect(screen.queryByText(/datboi dat import/)).toBeNull();
  await fireEvent.click(screen.getByText('+ import a dat to start a new system'));
  expect(screen.getByText(/datboi dat import/)).toBeTruthy();
});
