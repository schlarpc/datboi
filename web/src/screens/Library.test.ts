import { fireEvent, render, screen } from '@testing-library/svelte';
import { loadLocale } from 'wuchale/load-utils';
import { afterEach, expect, test, vi } from 'vitest';
import '../locales/main.loader.svelte.js';
import type { IngestReport, JobDetailBody, System } from '../lib/api/types';
import { installFetch, installUploadXhr } from '../test/mock-api';
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

/** A finished one-file ingest job whose report carries the given lanes
 * — the Library card reads only `dats_imported` and `errors`. */
function doneJob(report: Partial<IngestReport>): JobDetailBody {
  return {
    id: 1,
    name: 'ingest — 1 file',
    progress: 100,
    kind: 'ingest',
    state: 'done',
    files_total: 1,
    files_done: 1,
    bytes_total: 4,
    bytes_done: 4,
    started_at: 1000,
    finished_at: 1001,
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
      ...report,
    },
    matched: [],
    matched_total: 0,
    error: null,
  };
}

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

test('picking a dat stages it through the ingest job, logs the receipt, and refreshes the shelf', async () => {
  const handler = installFetch({
    systems: [gba],
    jobTimeline: [
      doneJob({
        dats_imported: [{ path: 'psx.dat', provider: 'redump', system: 'psx', entries: 1300 }],
      }),
    ],
  });
  const sent = installUploadXhr();
  render(Library);
  await screen.findByText('gba');

  const input = document.querySelector<HTMLInputElement>('input[type="file"]');
  expect(input).toBeTruthy();
  const file = new File(['<datafile/>'], 'psx.dat');
  await fireEvent.change(input!, { target: { files: [file] } });

  // The receipt comes from the job report's dats lane.
  expect(await screen.findByText('psx.dat')).toBeTruthy();
  expect(screen.getByText('redump/psx — 1,300 entries')).toBeTruthy();
  // The file rode the unified flow: staged upload, then one job.
  expect(sent.map((s) => s.name)).toEqual(['psx.dat']);
  const starts = handler.mock.calls.filter(([input_]) => String(input_) === '/v1/ingest');
  expect(starts.length).toBe(1);
  // The import mutated the shelf, so the screen re-fetched it.
  const systemFetches = handler.mock.calls.filter(([input_]) => String(input_) === '/v1/systems');
  expect(systemFetches.length).toBe(2);
});

test('a refused dat logs the job-report error against the file name', async () => {
  installFetch({
    systems: [gba],
    jobTimeline: [
      doneJob({ errors: [{ path: 'junk.dat', error: 'unknown dat format' }] }),
    ],
  });
  installUploadXhr();
  render(Library);
  await screen.findByText('gba');

  const input = document.querySelector<HTMLInputElement>('input[type="file"]');
  await fireEvent.change(input!, { target: { files: [new File(['junk'], 'junk.dat')] } });

  expect(await screen.findByText('junk.dat')).toBeTruthy();
  expect(screen.getByText('refused')).toBeTruthy();
  expect(screen.getByText('unknown dat format')).toBeTruthy();
});
