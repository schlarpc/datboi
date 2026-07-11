import { fireEvent, render, screen } from '@testing-library/svelte';
import { loadLocale } from 'wuchale/load-utils';
import { afterEach, expect, test, vi } from 'vitest';
import '../locales/main.loader.svelte.js';
import type { BlobDetail } from '../lib/api/types';
import { installClipboard, installFetch } from '../test/mock-api';
import Blob from './Blob.svelte';

await loadLocale('en');

const MEMBER = 'aa'.repeat(32);
const CONTAINER = 'bb'.repeat(32);

/** A zip member: made from its container, claimed, pinned. */
const detail: BlobDetail = {
  hash: MEMBER,
  size: 18,
  namespace: 'data',
  residency: 'resident',
  verified_at: 1_780_000_000,
  digests: { blake3: MEMBER, crc32: '11f95a1c', sha1: '0f'.repeat(20) },
  provenance: [{ path: 'packs/pack.zip', ingested_at: 1_780_000_000 }],
  routes_in: [
    {
      op: 'assemble@1',
      verify: 'verified',
      inputs: [{ hash: CONTAINER, size: 134, name: null }],
      outputs: [{ hash: MEMBER, size: 18, name: 'inner.gba' }],
    },
  ],
  routes_out: [],
  claims: [{ entry: 'Alpha (USA)', source: 'no-intro/gba' }],
  claims_total: 1,
  pins: ['gba'],
};

afterEach(() => vi.unstubAllGlobals());

test('the inspector renders identity, digests, provenance, claims, and pins', async () => {
  installFetch({ blobDetails: { [MEMBER]: detail } });
  render(Blob, { hash: MEMBER });

  // Full hash in the header AND the digests card (blake3 row).
  const hashes = await screen.findAllByText(MEMBER);
  expect(hashes.length).toBe(2);
  // Badges: namespace, residency, size, verified date.
  expect(screen.getByText('data')).toBeTruthy();
  expect(screen.getByText('resident')).toBeTruthy();
  // '18 B' renders twice: the size badge and the self output ref.
  expect(screen.getAllByText('18 B').length).toBe(2);
  expect(screen.getByText('verified 2026-05-28')).toBeTruthy();
  // Digests list only the recorded algos.
  expect(screen.getByText('11f95a1c')).toBeTruthy();
  expect(screen.getByText('0f'.repeat(20))).toBeTruthy();
  expect(screen.queryByText('md5')).toBeNull();
  // Provenance, claims, pins.
  expect(screen.getByText('packs/pack.zip')).toBeTruthy();
  expect(screen.getByText('Alpha (USA)')).toBeTruthy();
  expect(screen.getByText('no-intro/gba')).toBeTruthy();
  expect(screen.getByText('gba')).toBeTruthy();
  // No consumers: the empty line, not an empty card.
  expect(screen.getByText('no recipe consumes this blob')).toBeTruthy();
});

test('route edges link every neighbor hash to its own inspector', async () => {
  installFetch({ blobDetails: { [MEMBER]: detail } });
  render(Blob, { hash: MEMBER });
  await screen.findAllByText(MEMBER);

  expect(screen.getByText('assemble@1')).toBeTruthy();
  // The input ref walks to the container's card…
  const container = screen.getByText('bbbbb…bb').closest('a');
  expect(container?.getAttribute('href')).toBe(`/storage/blob/${CONTAINER}`);
  // …and the output ref (self) carries the recorded member name.
  const self = screen.getByText('aaaaa…aa').closest('a');
  expect(self?.getAttribute('href')).toBe(`/storage/blob/${MEMBER}`);
  expect(screen.getByText('inner.gba')).toBeTruthy();
});

test('the copy affordance puts the full hash on the clipboard', async () => {
  const writeText = installClipboard();
  installFetch({ blobDetails: { [MEMBER]: detail } });
  render(Blob, { hash: MEMBER });
  await screen.findAllByText(MEMBER);

  await fireEvent.click(screen.getByText('⎘ copy'));
  expect(writeText).toHaveBeenCalledWith(MEMBER);
  expect(await screen.findByText('copied ✓')).toBeTruthy();
});

test('an unknown hash shows the undesigned error line, with the way back', async () => {
  installFetch({});
  render(Blob, { hash: 'cc'.repeat(32) });

  expect(await screen.findByText(/something went wrong — no such blob/)).toBeTruthy();
  const back = screen.getByText('← storage').closest('a');
  expect(back?.getAttribute('href')).toBe('/storage');
});
