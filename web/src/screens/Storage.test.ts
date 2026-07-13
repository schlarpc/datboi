import { fireEvent, render, screen } from '@testing-library/svelte';
import { loadLocale } from 'wuchale/load-utils';
import { afterEach, expect, test, vi } from 'vitest';
import '../locales/main.loader.svelte.js';
import type { StorageBody, StorageBreakdownBody } from '../lib/api/types';
import { installFetch } from '../test/mock-api';
import Storage from './Storage.svelte';

await loadLocale('en');

const GB = 1024 ** 3;
const MB = 1024 ** 2;

const stats: StorageBody = {
  blob_count: 48212,
  on_disk_bytes: 1.2 * 1024 * GB,
  represented_bytes: 3.8 * 1024 * GB,
  literal_only_bytes: 214 * GB,
  quarantine: { count: 0, items: [] },
  last_scrub: null,
};

const breakdown: StorageBreakdownBody = {
  by_class: [
    { namespace: 'data', residency: 'resident', blobs: 48210, bytes: 500 * GB, sizeless: 0 },
    { namespace: 'data', residency: 'evicted_covered', blobs: 900, bytes: 200 * GB, sizeless: 3 },
    { namespace: 'meta', residency: 'resident', blobs: 120, bytes: 3 * MB, sizeless: 0 },
  ],
  by_source: [
    { source: 'no-intro/gba', blobs: 1200, bytes: 90 * GB },
    { source: '(unattached)', blobs: 4, bytes: 2 * GB },
  ],
  largest: [
    {
      hash: 'ab'.repeat(32),
      size: 4 * GB,
      namespace: 'data',
      residency: 'resident',
      verified_at: 1_780_000_000,
      sources: 1,
      routes_in: 0,
      routes_out: 1,
    },
  ],
};

afterEach(() => vi.unstubAllGlobals());

test('tiles render the four stats; savings % is computed client-side', async () => {
  installFetch({ storage: stats });
  render(Storage);

  expect(await screen.findByText('BLOBS')).toBeTruthy();
  expect(screen.getByText('48,212')).toBeTruthy();
  expect(screen.getByText('1228.8 GB')).toBeTruthy();
  expect(screen.getByText('3891.2 GB')).toBeTruthy();
  // 100 × (1 − 1.2/3.8) = 68.4… → 68
  expect(screen.getByText('−68% via recipes')).toBeTruthy();
  // "LITERAL-ONLY … shrinkable" became a name that needs no footnote.
  expect(screen.getByText('NOT YET OPTIMIZED')).toBeTruthy();
});

test('zero represented bytes: no savings claim', async () => {
  installFetch({}); // emptyStorage default
  render(Storage);
  await screen.findByText('BLOBS');
  expect(screen.queryByText(/via recipes/)).toBeNull();
});

test('quarantine empty state: one quiet maintenance line, no card', async () => {
  installFetch({ storage: stats });
  render(Storage);
  await screen.findByText('BLOBS');

  // Management by exception (87-web-ui.md): healthy quarantine is a
  // status line in the maintenance strip, not a card.
  const label = screen.getByText('Quarantine');
  expect(label.closest('.maint-row')).toBeTruthy();
  expect(label.closest('.action-card')).toBeNull();
  expect(screen.getByText('empty')).toBeTruthy();
});

test('quarantine items render inline (that IS the M5 review) with danger tint', async () => {
  installFetch({
    storage: {
      ...stats,
      quarantine: {
        count: 2,
        items: [
          {
            component: 'deadbeef'.repeat(8),
            quarantined_at: 1_780_000_000,
            reason: 'seek path produced bad bytes',
          },
          {
            component: 'cafebabe'.repeat(8),
            quarantined_at: 1_780_000_000,
            reason: 'crc mismatch in archive',
          },
        ],
      },
    },
  });
  render(Storage);

  const title = await screen.findByText(/Quarantine · 2/);
  expect(title.closest('.action-card')?.classList.contains('danger')).toBe(true);
  expect(screen.getByText('seek path produced bad bytes')).toBeTruthy();
  expect(screen.getByText('crc mismatch in archive')).toBeTruthy();
  expect(screen.getByText('deadbeef')).toBeTruthy();
});

test('breakdown renders class bars, the source table, and largest blobs', async () => {
  installFetch({ storage: stats, breakdown });
  render(Storage);

  expect(await screen.findByText('WHERE THE BYTES LIVE')).toBeTruthy();
  // by_class rows: namespace · residency in product words (87-web-ui.md
  // vocabulary), bytes + sizeless surfaced.
  expect(screen.getByText('data · on disk')).toBeTruthy();
  expect(screen.getByText('data · rebuildable')).toBeTruthy();
  expect(screen.getByText('meta · on disk')).toBeTruthy();
  expect(screen.getByText('500.0 GB')).toBeTruthy();
  expect(screen.getByText(/3 sizeless/)).toBeTruthy();

  // by_source table — attribution is viral through the recipe DAG
  // (D79); the residual bucket is truly UNATTACHED blobs.
  expect(screen.getByText('no-intro/gba')).toBeTruthy();
  expect(screen.getByText('90.0 GB')).toBeTruthy();
  expect(screen.getByText('(unattached)')).toBeTruthy();

  // largest blobs link into the inspector
  const link = screen.getByText('abababab').closest('a');
  expect(link?.getAttribute('href')).toBe(`/storage/blob/${'ab'.repeat(32)}`);
  expect(screen.getByText('4.0 GB')).toBeTruthy();
});

test('empty breakdown renders no tables but keeps the tiles', async () => {
  installFetch({ storage: stats }); // emptyBreakdown default
  render(Storage);
  await screen.findByText('WHERE THE BYTES LIVE');
  expect(screen.getByText('BLOBS')).toBeTruthy();
  expect(screen.queryByText('no-intro/gba')).toBeNull();
});

test('scrub and eviction maintenance rows reveal verified CLI hints', async () => {
  installFetch({ storage: stats });
  render(Storage);
  await screen.findByText('BLOBS');

  await fireEvent.click(screen.getByText('run via CLI'));
  expect(screen.getByText(/datboi scrub/)).toBeTruthy();
  expect(screen.getByText('never run')).toBeTruthy();

  // D72: eviction is automatic and reversible; the row tunes, not plans.
  expect(screen.getByText(/automatic at the watermark/)).toBeTruthy();
  await fireEvent.click(screen.getByText('tune via CLI'));
  expect(screen.getByText(/datboi gc config --high-water/)).toBeTruthy();
});

test('orphans empty state: one quiet maintenance line, no card', async () => {
  installFetch({ storage: stats });
  render(Storage);
  await screen.findByText('BLOBS');
  // Management by exception: healthy orphans is a status line.
  const label = await screen.findByText('Orphans');
  expect(label.closest('.maint-row')).toBeTruthy();
  expect(screen.getByText('none')).toBeTruthy();
});

test('orphan review card lists candidates with provenance and arms the delete', async () => {
  installFetch({
    storage: stats,
    orphans: {
      grace_secs: 86_400,
      reclaimable_bytes: 3 * MB,
      orphans: [
        {
          hash: 'cd'.repeat(32),
          size: 3 * MB,
          marked_at: 1_780_000_000,
          sources: ['roms/mystery.bin'],
          kept: false,
        },
        {
          hash: 'ef'.repeat(32),
          size: 1 * MB,
          marked_at: 1_780_000_000,
          sources: [],
          kept: true,
        },
      ],
    },
  });
  render(Storage);
  await screen.findByText(/Orphans · 2/);

  // Provenance is the review context; the kept row shows its state.
  expect(screen.getByText(/roms\/mystery\.bin/)).toBeTruthy();
  expect(screen.getByText('kept ✓')).toBeTruthy();

  // Two-click delete: first arms, second (mock) applies and refreshes.
  const applyButton = screen.getByText('delete non-kept…');
  await fireEvent.click(applyButton);
  expect(screen.getByText('confirm: delete non-kept')).toBeTruthy();
});


test('a failed orphans fetch errors ONLY its card — stats and breakdown stand', async () => {
  installFetch({ storage: stats, breakdown, orphansFail: true });
  render(Storage);

  // The orphans card carries the failure…
  expect(await screen.findByText(/induced orphans failure/)).toBeTruthy();
  // …while the tiles and breakdown render untouched.
  expect(screen.getByText('BLOBS')).toBeTruthy();
  expect(screen.getByText('48,212')).toBeTruthy();
  expect(screen.getByText('WHERE THE BYTES LIVE')).toBeTruthy();
});

test('scrub card reads the D74 run ledger when a row exists', async () => {
  installFetch({
    storage: {
      ...stats,
      last_scrub: { finished_at: 1_780_000_000, name: 'cli: scrub — 100% sample' },
    },
  });
  render(Storage);
  await screen.findByText('BLOBS');
  expect(screen.getByText(/cli: scrub — 100% sample/)).toBeTruthy();
});
