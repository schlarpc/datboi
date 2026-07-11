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
};

const breakdown: StorageBreakdownBody = {
  by_class: [
    { namespace: 'data', residency: 'resident', blobs: 48210, bytes: 500 * GB, sizeless: 0 },
    { namespace: 'data', residency: 'evicted_covered', blobs: 900, bytes: 200 * GB, sizeless: 3 },
    { namespace: 'meta', residency: 'resident', blobs: 120, bytes: 3 * MB, sizeless: 0 },
  ],
  by_source: [
    { source: 'no-intro/gba', blobs: 1200, bytes: 90 * GB },
    { source: '(unattributed)', blobs: 4, bytes: 2 * GB },
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
  expect(screen.getByText('shrinkable')).toBeTruthy();
});

test('zero represented bytes: no savings claim', async () => {
  installFetch({}); // emptyStorage default
  render(Storage);
  await screen.findByText('BLOBS');
  expect(screen.queryByText(/via recipes/)).toBeNull();
});

test('quarantine empty state: zero count, no danger tint', async () => {
  installFetch({ storage: stats });
  render(Storage);
  await screen.findByText('BLOBS');

  const title = screen.getByText(/Quarantine · 0/);
  expect(title.closest('.action-card')?.classList.contains('danger')).toBe(false);
  expect(screen.getByText('nothing quarantined')).toBeTruthy();
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
  expect(screen.getByText('deadb…ef')).toBeTruthy();
});

test('breakdown renders class bars, the source table, and largest blobs', async () => {
  installFetch({ storage: stats, breakdown });
  render(Storage);

  expect(await screen.findByText('WHERE THE BYTES LIVE')).toBeTruthy();
  // by_class rows: namespace · residency labels, wire underscore
  // rendered as a space, bytes + sizeless surfaced.
  expect(screen.getByText('data · resident')).toBeTruthy();
  expect(screen.getByText('data · evicted covered')).toBeTruthy();
  expect(screen.getByText('meta · resident')).toBeTruthy();
  expect(screen.getByText('500.0 GB')).toBeTruthy();
  expect(screen.getByText(/3 sizeless/)).toBeTruthy();

  // by_source table
  expect(screen.getByText('no-intro/gba')).toBeTruthy();
  expect(screen.getByText('90.0 GB')).toBeTruthy();
  expect(screen.getByText('(unattributed)')).toBeTruthy();

  // largest blobs link into the inspector
  const link = screen.getByText('ababa…ab').closest('a');
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

test('scrub and eviction cards reveal verified CLI hints', async () => {
  installFetch({ storage: stats });
  render(Storage);
  await screen.findByText('BLOBS');

  await fireEvent.click(screen.getByText('run via CLI'));
  expect(screen.getByText(/datboi scrub/)).toBeTruthy();

  expect(screen.getByText('nothing is deleted without a plan you approve')).toBeTruthy();
  await fireEvent.click(screen.getByText('plan (dry-run) via CLI'));
  expect(screen.getByText('datboi evict --target-bytes <n> --dry-run')).toBeTruthy();
});
