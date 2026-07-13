import { render, screen, fireEvent, waitFor } from '@testing-library/svelte';
import { loadLocale } from 'wuchale/load-utils';
import { afterEach, beforeEach, expect, test, vi } from 'vitest';
import '../locales/main.loader.svelte.js';
import type { ViewDetail, ViewFileRow } from '../lib/api/types';
import { installFetch } from '../test/mock-api';
import Browse from './Browse.svelte';

await loadLocale('en');

const MB = 1024 * 1024;
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
  rows: 2,
  bytes: 12 * MB,
  created_at: nowSecs - 2 * 3600,
  endpoints: { http: '/view/gba-everdrive/', dav: '/dav/gba-everdrive/' },
  image: { minted: true, hash: 'ab'.repeat(32), bytes: 14.2 * 1024 ** 3 },
};

const rows: ViewFileRow[] = [
  { path: 'Games/Alpha (USA).gba', size: 4 * MB, hash: 'aa11bb22'.repeat(8) },
  { path: 'Games/Beta (Japan).gba', size: 8 * MB, hash: 'cc33dd44'.repeat(8) },
];

beforeEach(() => {
  installFetch({ views: [shelf], files: { 'gba-everdrive': rows } });
});

afterEach(() => {
  vi.unstubAllGlobals();
});

test('rows render verified with sizes; search goes through the server q', async () => {
  render(Browse, { view: 'gba-everdrive' });

  expect(await screen.findByText('Games/Alpha (USA).gba')).toBeTruthy();
  expect(screen.getByText('Games/Beta (Japan).gba')).toBeTruthy();
  expect(screen.getByText('4 MB')).toBeTruthy();
  // toolbar info line: snap short + age + total under the filter
  expect((await screen.findByText(/snap #a41f/)).textContent).toContain('2 files');

  const input = screen.getByPlaceholderText('find a game…');
  await fireEvent.input(input, { target: { value: 'beta' } });
  // The recompose trails the keystroke (debounced): wait for it to land.
  await waitFor(() => expect(screen.queryByText('Games/Alpha (USA).gba')).toBeNull());
  expect(screen.getByText('Games/Beta (Japan).gba')).toBeTruthy();

  await fireEvent.input(input, { target: { value: 'zelda' } });
  expect(await screen.findByText('nothing matches “zelda”')).toBeTruthy();
});

test('quick-download is a real anchor, flashes ✓, and does not select the row', async () => {
  render(Browse, { view: 'gba-everdrive' });
  await screen.findByText('Games/Alpha (USA).gba');

  const pill = screen.getAllByLabelText('download')[0] as HTMLAnchorElement;
  expect(pill.getAttribute('href')).toBe('/view/gba-everdrive/Games/Alpha%20(USA).gba');
  expect(pill.getAttribute('download')).toBe('Alpha (USA).gba');
  expect(pill.textContent?.trim()).toBe('⬇');

  await fireEvent.click(pill);
  expect(pill.textContent?.trim()).toBe('✓');
  // stopPropagation: the entry panel did NOT open (§5.12)
  expect(screen.queryByText('ENTRY')).toBeNull();
});

test('row select opens the entry panel: sub-line, trust line, real download', async () => {
  render(Browse, { view: 'gba-everdrive' });
  await screen.findByText('Games/Alpha (USA).gba');

  await fireEvent.click(screen.getByText('Games/Alpha (USA).gba'));
  expect(await screen.findByText('ENTRY')).toBeTruthy();

  // region parsed from the parenthetical · size · 8-char hash
  const sub = document.querySelector('.panel .sub');
  expect(sub?.textContent?.replace(/\s+/g, ' ').trim()).toBe('USA · 4 MB · aa11bb22');
  expect(screen.getByText('● verified')).toBeTruthy();
  expect(screen.getByText('hash-checked as it streams')).toBeTruthy();

  const download = screen.getByText('⬇ Download') as HTMLAnchorElement;
  expect(download.getAttribute('href')).toBe('/view/gba-everdrive/Games/Alpha%20(USA).gba');

  // No disabled future-feature buttons (87-web-ui.md): ▶ Play ships
  // when playing ships.
  expect(screen.queryByText('▶ Play')).toBeNull();

  // re-click deselects
  await fireEvent.click(screen.getByText('Games/Alpha (USA).gba'));
  expect(screen.queryByText('ENTRY')).toBeNull();
});

test('SD image modal: pill only when minted, warning + real download, close semantics', async () => {
  render(Browse, { view: 'gba-everdrive' });
  await screen.findByText('Games/Alpha (USA).gba');

  // trust bar: promise + the minted-image pill with its size
  expect(screen.getByText('● every download is hash-verified as it streams')).toBeTruthy();
  const pill = await screen.findByText(/⬇ whole SD image/);
  expect(pill.textContent).toContain('14.2 GB');

  await fireEvent.click(pill);
  expect(await screen.findByText('SD image · gba-everdrive')).toBeTruthy();
  expect(screen.getByText(/minted from snap #a41f/)).toBeTruthy();
  expect(
    screen.getByText('⚠ Re-flashing a card overwrites on-device saves. Back up your saves first.'),
  ).toBeTruthy();

  // the staged confirm is a REAL download anchor into the image route
  const confirm = screen.getByText('I understand — download') as HTMLAnchorElement;
  expect(confirm.getAttribute('href')).toBe('/v1/views/gba-everdrive/image');
  expect(confirm.getAttribute('download')).toBe('gba-everdrive.img');

  // cancel closes; backdrop click closes; card swallows clicks
  await fireEvent.click(screen.getByText('cancel'));
  expect(screen.queryByText('SD image · gba-everdrive')).toBeNull();
  await fireEvent.click(screen.getByText(/⬇ whole SD image/));
  await fireEvent.click(document.querySelector('.modal')!);
  expect(screen.getByText('SD image · gba-everdrive')).toBeTruthy();
  await fireEvent.click(document.querySelector('.backdrop')!);
  expect(screen.queryByText('SD image · gba-everdrive')).toBeNull();
});

test('Escape closes the modal first, then the entry panel (§5.14)', async () => {
  render(Browse, { view: 'gba-everdrive' });
  await screen.findByText('Games/Alpha (USA).gba');

  await fireEvent.click(screen.getByText('Games/Alpha (USA).gba'));
  await screen.findByText('ENTRY');
  await fireEvent.click(screen.getByText(/⬇ whole SD image/));
  await screen.findByText('SD image · gba-everdrive');

  await fireEvent.keyDown(window, { key: 'Escape' });
  expect(screen.queryByText('SD image · gba-everdrive')).toBeNull();
  expect(screen.getByText('ENTRY')).toBeTruthy();

  await fireEvent.keyDown(window, { key: 'Escape' });
  expect(screen.queryByText('ENTRY')).toBeNull();
});

test('rows activate on Space as well as Enter — the role=button contract', async () => {
  render(Browse, { view: 'gba-everdrive' });
  const row = (await screen.findByText('Games/Alpha (USA).gba')).closest('[role="button"]');
  if (row === null) throw new Error('row not found');
  await fireEvent.keyDown(row, { key: ' ' });
  expect(row.classList.contains('sel')).toBe(true);
  await fireEvent.keyDown(row, { key: 'Enter' });
  expect(row.classList.contains('sel')).toBe(false);
});
