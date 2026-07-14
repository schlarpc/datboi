import { fireEvent, render, screen } from '@testing-library/svelte';
import { loadLocale } from 'wuchale/load-utils';
import { afterEach, beforeEach, expect, test, vi } from 'vitest';
import '../locales/main.loader.svelte.js';
import type { EntryDetail, EntryRow, System } from '../lib/api/types';
import { calledPath, installFetch, type MockUniverse } from '../test/mock-api';
import Audit from './Audit.svelte';

await loadLocale('en');

const MB = 1024 * 1024;
const ALPHA_HASH = 'aabbccddeeff00112233445566778899aabbccdd';
const BLOB_HASH = 'deadbeef'.repeat(8);

const corpus: EntryRow[] = [
  { name: 'Alpha (USA)', state: 'verified', size: 4 * MB, wanted_hash: ALPHA_HASH, wanted_hash_algo: 'sha1' },
  { name: 'Alpha II (USA)', state: 'claimed', size: 8 * MB, wanted_hash: ALPHA_HASH, wanted_hash_algo: 'sha1' },
  { name: 'Beta (Japan)', state: 'missing', size: null, wanted_hash: ALPHA_HASH, wanted_hash_algo: 'sha1' },
  { name: 'Gamma (USA)', state: 'nodump', size: null, wanted_hash: null, wanted_hash_algo: null },
];

const system: System = {
  id: 3,
  provider: 'no-intro',
  system: 'gba',
  source: 'no-intro/gba',
  revision: { id: 1, version: 'r1', date: null, imported_at: 1000 },
  counts: { verified: 1, claimed: 1, missing: 1, nodump: 1 },
  total: 4,
  views: ['gba-everdrive'],
};

const alphaDetail: EntryDetail = {
  name: 'Alpha (USA)',
  state: 'verified',
  size: 4 * MB,
  wanted_hash: ALPHA_HASH,
  wanted_hash_algo: 'sha1',
  revision: { id: 1, version: 'r1', date: null, imported_at: 1000 },
  roms: [
    {
      name: 'Alpha (USA).gba',
      size: 4 * MB,
      state: 'verified',
      optional: false,
      hashes: { sha1: ALPHA_HASH },
      blob: { hash: BLOB_HASH, residency: 'resident', verified_at: 1_780_000_000 },
      routes: [{ route: 'deflate ← roms-gba.zip', source_present: true, verify: 'verified' }],
      pins: ['gba-everdrive'],
    },
  ],
};

const universe: MockUniverse = {
  systems: [system],
  entries: corpus,
  detail: (name) => (name === 'Alpha (USA)' ? alphaDetail : undefined),
};

beforeEach(() => {
  installFetch(universe);
});

afterEach(() => {
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
});

test('rows render with state words and sizes; rail counts are unfiltered', async () => {
  render(Audit, { systemId: '3' });

  expect(await screen.findByText('Alpha (USA)')).toBeTruthy();
  expect(screen.getByText('Beta (Japan)')).toBeTruthy();
  expect(screen.getByText('4 MB')).toBeTruthy();
  // missing + nodump rows show the em-dash size
  expect(screen.getAllByText('—').length).toBe(2);
  // header: completeness excludes nodump → 1 / (4-1) = 33%
  expect(screen.getByText('33%')).toBeTruthy();
  // rail items carry unfiltered totals
  expect(screen.getByText('Verified')).toBeTruthy();
  expect(screen.getByText('No dump')).toBeTruthy();
  expect(screen.getByText('All')).toBeTruthy();
});

test('filter and search compose (both applied together, spec §5.1)', async () => {
  render(Audit, { systemId: '3' });
  await screen.findByText('Alpha (USA)');

  // state filter alone
  await fireEvent.click(screen.getByText('Missing'));
  expect(await screen.findByText('Beta (Japan)')).toBeTruthy();
  expect(screen.queryByText('Alpha (USA)')).toBeNull();

  // search composes on top: no missing entry matches "alpha"
  const input = screen.getByPlaceholderText('filter names…');
  await fireEvent.input(input, { target: { value: 'alpha' } });
  expect(await screen.findByText('nothing matches — clear the filter or search')).toBeTruthy();

  // swap the state to claimed: search still applied
  await fireEvent.click(screen.getByText('Claimed'));
  expect(await screen.findByText('Alpha II (USA)')).toBeTruthy();
  expect(screen.queryByText('Alpha (USA)')).toBeNull();
});

test('a typing burst issues one query per pause — not one per keystroke', async () => {
  vi.useFakeTimers();
  const handler = installFetch(universe);
  const entriesCalls = () =>
    handler.mock.calls.filter(([input]) => calledPath(input).endsWith('/entries')).length;

  render(Audit, { systemId: '3' });
  await vi.advanceTimersByTimeAsync(0);
  const before = entriesCalls();

  // Five keystrokes, each inside the trailing window of the last.
  const input = screen.getByPlaceholderText('filter names…');
  for (const value of ['a', 'al', 'alp', 'alph', 'alpha']) {
    await fireEvent.input(input, { target: { value } });
    await vi.advanceTimersByTimeAsync(100);
  }
  expect(entriesCalls()).toBe(before); // nothing fired mid-burst

  await vi.advanceTimersByTimeAsync(200); // the pause
  expect(entriesCalls()).toBe(before + 1); // exactly one recompose
  expect(screen.getByText('Alpha (USA)')).toBeTruthy();

  vi.useRealTimers();
});

test('row click opens the drawer; Escape, ✕, and re-click close it', async () => {
  render(Audit, { systemId: '3' });
  await screen.findByText('Alpha (USA)');
  // Once the drawer is open the name appears twice; target the row cell.
  const row = () =>
    screen.getAllByText('Alpha (USA)').find((el) => el.classList.contains('row-name'))!;

  await fireEvent.click(row());
  expect(await screen.findByText('ENTRY')).toBeTruthy();
  // sub line: region from the name parenthetical + size + short hash
  // (composed from several text nodes, so match the element's text)
  const sub = document.querySelector('.drawer .sub');
  expect(sub?.textContent?.replace(/\s+/g, ' ').trim()).toBe('USA · 4 MB · aabbccdd');

  // The drawer summarizes and links out — the blob line is a link into
  // the inspector, which owns the internals (web-ui.md; the old
  // storage-internals fold is dead).
  const blobLink = screen.getByText('deadbeef').closest('a');
  expect(blobLink?.getAttribute('href')).toBe(`/storage/blob/${BLOB_HASH}`);
  expect(screen.getByText(/on disk/)).toBeTruthy();
  expect(screen.getByText(/verified 2026-05-28/)).toBeTruthy();

  // Escape closes (spec §5.2)
  await fireEvent.keyDown(window, { key: 'Escape' });
  expect(screen.queryByText('ENTRY')).toBeNull();

  // re-click toggles: open, then ✕ closes
  await fireEvent.click(row());
  expect(await screen.findByText('ENTRY')).toBeTruthy();
  await fireEvent.click(screen.getByLabelText('close'));
  expect(screen.queryByText('ENTRY')).toBeNull();

  // select + deselect by clicking the same row twice
  await fireEvent.click(row());
  expect(await screen.findByText('ENTRY')).toBeTruthy();
  await fireEvent.click(row());
  expect(screen.queryByText('ENTRY')).toBeNull();
});

test('⬇ missing-list generates the plaintext export client-side (§5.5)', async () => {
  const blobs: Blob[] = [];
  URL.createObjectURL = vi.fn((blob: Blob) => {
    blobs.push(blob);
    return 'blob:mock';
  });
  URL.revokeObjectURL = vi.fn();
  const click = vi
    .spyOn(HTMLAnchorElement.prototype, 'click')
    .mockImplementation(() => {});

  render(Audit, { systemId: '3' });
  await screen.findByText('Alpha (USA)');
  await fireEvent.click(screen.getByText('⬇ missing-list'));

  await vi.waitFor(() => expect(blobs.length).toBe(1));
  const text = await blobs[0].text();
  expect(text).toBe('# datboi missing-list · no-intro gba r1\nBeta (Japan)\n');
  expect(click).toHaveBeenCalledOnce();
});

test('a failed entries fetch errors only the rows — the filter rail stays usable', async () => {
  installFetch({ ...universe, entriesFailFromOffset: 0 });
  render(Audit, { systemId: '3' });

  // The rows area carries the failure…
  expect(await screen.findByText(/induced entries failure/)).toBeTruthy();
  // …while the header and the recovery controls (rail + search) stand.
  expect(screen.getByText('33%')).toBeTruthy();
  expect(screen.getByText('Verified')).toBeTruthy();
  expect(screen.getByPlaceholderText('filter names…')).toBeTruthy();
});

const bigCorpus = (n: number): EntryRow[] =>
  Array.from({ length: n }, (_, i) => ({
    name: `Entry ${String(i).padStart(4, '0')}`,
    state: 'verified',
    size: MB,
    wanted_hash: ALPHA_HASH,
    wanted_hash_algo: 'sha1',
  }));

test('the list virtualizes: honest scrollbar, pages fetch as the window moves', async () => {
  installFetch({ systems: [system], entries: bigCorpus(2500) });
  render(Audit, { systemId: '3' });

  // First page in, the spacer is sized for ALL 2500 rows — the
  // scrollbar tells the truth about collection size from paint one.
  expect(await screen.findByText('Entry 0000')).toBeTruthy();
  const spacer = document.querySelector<HTMLElement>('.spacer');
  expect(spacer?.style.height).toBe(`${2500 * 38}px`);
  // Row 1200 is on page 1 — not fetched, not rendered.
  expect(screen.queryByText('Entry 1200')).toBeNull();

  // Scroll the window into page 1: the covering page fetches and the
  // rows land in place.
  const rows = document.querySelector<HTMLElement>('.rows')!;
  rows.scrollTop = 1200 * 38;
  await fireEvent.scroll(rows);
  expect(await screen.findByText('Entry 1200')).toBeTruthy();
  expect(screen.queryByText('Entry 0000')).toBeNull(); // out the window
});

test('a failed page fetch keeps everything loaded and says why', async () => {
  installFetch({ systems: [system], entries: bigCorpus(2500), entriesFailFromOffset: 1000 });
  render(Audit, { systemId: '3' });
  expect(await screen.findByText('Entry 0000')).toBeTruthy();

  const rows = document.querySelector<HTMLElement>('.rows')!;
  rows.scrollTop = 1200 * 38;
  await fireEvent.scroll(rows);
  expect(await screen.findByText(/couldn't load rows — induced entries failure/)).toBeTruthy();
  // The unloaded window renders skeletons, not a blank or an unmount.
  expect(document.querySelector('.row.skeleton')).toBeTruthy();
});

// Density pref deleted per D78 — rows ship at one fixed height.
