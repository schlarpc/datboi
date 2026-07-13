import { fireEvent, render, screen } from '@testing-library/svelte';
import { loadLocale } from 'wuchale/load-utils';
import { afterEach, expect, test, vi } from 'vitest';
import '../locales/main.loader.svelte.js';
import type { BlobDetail, JobDetailBody } from '../lib/api/types';
import { calledPath, installClipboard, installFetch } from '../test/mock-api';
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
  roots: [],
  provenance_via: [],
  sniff: null,
  pins: ['gba'],
};

afterEach(() => vi.unstubAllGlobals());

test('the inspector renders identity, digests, provenance, claims, and pins', async () => {
  installFetch({ blobDetails: { [MEMBER]: detail } });
  render(Blob, { hash: MEMBER });

  // The claim name is the HEADLINE (D79); the hash demotes to
  // metadata. Full hash renders in the head row AND the digests card.
  const title = await screen.findByRole('heading', { level: 2 });
  expect(title.textContent?.trim()).toBe('Alpha (USA)');
  const hashes = screen.getAllByText(MEMBER);
  expect(hashes.length).toBe(2);
  // Badges: namespace, residency, size, verified date.
  expect(screen.getByText('data')).toBeTruthy();
  expect(screen.getByText('on disk')).toBeTruthy();
  // '18 B' renders twice: the size badge and the self output ref.
  expect(screen.getAllByText('18 B').length).toBe(2);
  expect(screen.getByText('verified 2026-05-28')).toBeTruthy();
  // Digests list only the recorded algos.
  expect(screen.getByText('11f95a1c')).toBeTruthy();
  expect(screen.getByText('0f'.repeat(20))).toBeTruthy();
  expect(screen.queryByText('md5')).toBeNull();
  // Provenance, claims, pins.
  expect(screen.getByText('packs/pack.zip')).toBeTruthy();
  expect(screen.getAllByText('Alpha (USA)').length).toBe(2); // headline + claim row
  expect(screen.getAllByText('no-intro/gba').length).toBeGreaterThan(0);
  expect(screen.getByText('gba')).toBeTruthy();
  // No consumers: the empty line, not an empty card.
  expect(screen.getByText('no recipe consumes this blob')).toBeTruthy();
});

test('an unclaimed blob headlines its root relation; provenance rides via (D79)', async () => {
  const chunk: BlobDetail = {
    ...detail,
    hash: CONTAINER,
    claims: [],
    claims_total: 0,
    provenance: [],
    provenance_via: [{ path: 'roms/pack.zip', ingested_at: 1_780_000_000, via: MEMBER }],
    roots: [{ hash: MEMBER, entry: 'Alpha (USA)', source: 'no-intro/gba', relation: 'makes' }],
    routes_in: [],
    routes_out: [],
    pins: [],
  };
  installFetch({ blobDetails: { [CONTAINER]: chunk } });
  render(Blob, { hash: CONTAINER });

  const title = await screen.findByRole('heading', { level: 2 });
  expect(title.textContent?.replace(/\s+/g, ' ').trim()).toBe('helps rebuild Alpha (USA)');
  // The root name links into its own inspector.
  expect(screen.getByText('Alpha (USA)').closest('a')?.getAttribute('href')).toBe(
    `/storage/blob/${MEMBER}`,
  );
  // Viral provenance: the arrival path, credited to the carrying blob.
  expect(screen.getByText('roms/pack.zip')).toBeTruthy();
  expect(screen.getByText(`via ${MEMBER.slice(0, 8)}`)).toBeTruthy();
});

test('a truly unattached blob falls back to the byte sniff', async () => {
  const stray: BlobDetail = {
    ...detail,
    hash: CONTAINER,
    claims: [],
    claims_total: 0,
    provenance: [],
    roots: [],
    sniff: 'zip archive',
    routes_in: [],
    routes_out: [],
    pins: [],
  };
  installFetch({ blobDetails: { [CONTAINER]: stray } });
  render(Blob, { hash: CONTAINER });

  const title = await screen.findByRole('heading', { level: 2 });
  expect(title.textContent?.trim()).toBe('zip archive');
  expect(screen.getByText('unattached — nothing claimed connects to it')).toBeTruthy();
});

test('route edges link every neighbor hash to its own inspector', async () => {
  installFetch({ blobDetails: { [MEMBER]: detail } });
  render(Blob, { hash: MEMBER });
  await screen.findAllByText(MEMBER);

  expect(screen.getByText('assemble@1')).toBeTruthy();
  // The input ref walks to the container's card…
  const container = screen.getByText('bbbbbbbb').closest('a');
  expect(container?.getAttribute('href')).toBe(`/storage/blob/${CONTAINER}`);
  // …and the output ref (self) is deliberately NOT a link — a link to
  // the page you're on isn't navigation (87-web-ui.md).
  const self = screen.getByText('this blob');
  expect(self.closest('a')).toBeNull();
  expect(screen.getByText('inner.gba')).toBeTruthy();
});

test('long ref lists cap at 5 with an expand-in-place tail', async () => {
  const chunks = Array.from({ length: 9 }, (_, i) => ({
    hash: `${String(i).repeat(2)}`.padEnd(64, 'd'),
    size: 1024,
    name: null,
  }));
  const chunked: BlobDetail = {
    ...detail,
    routes_in: [
      { op: 'assemble@1', verify: 'verified', inputs: chunks, outputs: [] },
    ],
  };
  installFetch({ blobDetails: { [MEMBER]: chunked } });
  render(Blob, { hash: MEMBER });
  await screen.findAllByText(MEMBER);

  // Aggregate first (edge head count), enumeration capped…
  expect(screen.getByText('9 inputs')).toBeTruthy();
  const more = screen.getByText('…and 4 more');
  // …and the tail expands in place.
  await fireEvent.click(more);
  expect(screen.queryByText('…and 4 more')).toBeNull();
  expect(screen.getByText(chunks[8].hash.slice(0, 8))).toBeTruthy();
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

test('the never-verified badge is the verify-now button (D80)', async () => {
  const doneJob: JobDetailBody = {
    id: 9,
    name: 'verify — aaaaaaaa',
    progress: 100,
    kind: 'scrub',
    state: 'done',
    started_at: 1000,
    finished_at: 1001,
    files_total: 1,
    files_done: 1,
    bytes_total: 1,
    bytes_done: 1,
    report: {
      files_scanned: 0,
      files_unchanged: 0,
      files_stored: 0,
      files_already_present: 0,
      chd_v5: 0,
      members_claimed: 0,
      members_extracted: 0,
      detector_hits: 0,
      skipper_skipped_large: 0,
      dats_imported: [],
      errors: [],
      member_skips: [],
      notes: [],
    },
    matched: [],
    matched_total: 0,
    error: null,
  };
  const handler = installFetch({
    blobDetails: { [MEMBER]: { ...detail, verified_at: null } },
    verifyJob: 9,
    jobTimeline: [doneJob],
  });
  render(Blob, { hash: MEMBER });

  const btn = await screen.findByText('never verified — verify now');
  await fireEvent.click(btn);
  // POST fired, the job polled to done, and the detail refetched.
  await vi.waitFor(() => {
    expect(
      handler.mock.calls.some(([input]) => calledPath(input).endsWith('/verify')),
    ).toBe(true);
    expect(
      handler.mock.calls.filter(([input]) => calledPath(input) === `/v1/blobs/${MEMBER}`).length,
    ).toBeGreaterThan(1);
  });
});

test('an unknown hash shows the undesigned error line, with the way back', async () => {
  installFetch({});
  render(Blob, { hash: 'cc'.repeat(32) });

  expect(await screen.findByText(/something went wrong — no such blob/)).toBeTruthy();
  const back = screen.getByText('← storage').closest('a');
  expect(back?.getAttribute('href')).toBe('/storage');
});
