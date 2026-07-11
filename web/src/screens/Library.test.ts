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

test('views chips render; the empty card is the dat drop zone', async () => {
  installFetch({ systems: [gba, snes] });
  render(Library);
  await screen.findByText('gba');

  expect(screen.getByText('gba-everdrive')).toBeTruthy();
  expect(screen.getByText('gba-flash')).toBeTruthy();
  expect(screen.getByText(/drop files here or click to pick/)).toBeTruthy();
});

test('picking a dat imports it, logs the receipt, and refreshes the shelf', async () => {
  const handler = installFetch({
    systems: [gba],
    datImport: {
      source_id: 3,
      revision_id: 9,
      dat_blob: 'a'.repeat(64),
      provider: 'redump',
      system: 'psx',
      entries: 1300,
      claims: 1450,
      demoted_revisions: [],
    },
  });
  render(Library);
  await screen.findByText('gba');

  const input = document.querySelector<HTMLInputElement>('input[type="file"]');
  expect(input).toBeTruthy();
  const file = new File(['<datafile/>'], 'psx.dat');
  await fireEvent.change(input!, { target: { files: [file] } });

  expect(await screen.findByText('psx.dat')).toBeTruthy();
  expect(screen.getByText('redump/psx — 1,300 entries')).toBeTruthy();
  // The import mutated the shelf, so the screen re-fetched it.
  const systemFetches = handler.mock.calls.filter(([input_]) => String(input_) === '/v1/systems');
  expect(systemFetches.length).toBe(2);
});

test('a refused dat logs the server error against the file name', async () => {
  installFetch({ systems: [gba], datImportFail: true });
  render(Library);
  await screen.findByText('gba');

  const input = document.querySelector<HTMLInputElement>('input[type="file"]');
  await fireEvent.change(input!, { target: { files: [new File(['junk'], 'junk.dat')] } });

  expect(await screen.findByText('junk.dat')).toBeTruthy();
  expect(screen.getByText('refused')).toBeTruthy();
  expect(screen.getByText('unknown dat format')).toBeTruthy();
});
