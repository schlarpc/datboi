import { fireEvent, render, screen } from '@testing-library/svelte';
import { loadLocale } from 'wuchale/load-utils';
import { afterEach, beforeEach, expect, test, vi } from 'vitest';
import '../locales/main.loader.svelte.js';
import type { Job, JobDetailBody } from '../lib/api/types';
import { registry } from '../lib/activity.svelte';
import { installFetch } from '../test/mock-api';
import Activity from './Activity.svelte';

await loadLocale('en');

const nowSecs = Math.floor(Date.now() / 1000);

/** The list rides the Header's shared registry poll (D82) — the page
 * itself only fetches detail, so tests seed the snapshot directly. */
beforeEach(() => {
  registry.jobs = [];
  registry.unreachable = false;
});

afterEach(() => {
  vi.unstubAllGlobals();
});

const running: Job = {
  id: 391,
  name: 'ingest — 2 files',
  progress: 61,
  kind: 'ingest',
  state: 'running',
  started_at: nowSecs - 5 * 60,
  finished_at: null,
};

const finished: Job = {
  id: 388,
  name: 'refine — preflate',
  progress: 100,
  kind: 'refine',
  state: 'done',
  started_at: nowSecs - 2 * 3600 - 40,
  finished_at: nowSecs - 2 * 3600,
};

const failed: Job = {
  id: 380,
  name: 'ingest — 1 file',
  progress: 100,
  kind: 'ingest',
  state: 'failed',
  started_at: nowSecs - 3 * 3600,
  finished_at: nowSecs - 3 * 3600 + 5,
};

/** A JobDetailBody over a registry row — counters default to zero. */
function detailOf(job: Job, over: Partial<JobDetailBody> = {}): JobDetailBody {
  return {
    ...job,
    files_total: 1,
    files_done: 0,
    bytes_total: 8,
    bytes_done: 0,
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
    ...over,
  };
}

test('rows render newest first with kind, state, and relative timestamps', async () => {
  installFetch({ jobTimeline: [detailOf(running)] });
  registry.jobs = [finished, running, failed];
  render(Activity);

  // Newest first regardless of snapshot order.
  const names = [...document.querySelectorAll('.name')].map((el) => el.textContent);
  expect(names).toEqual(['ingest — 2 files', 'refine — preflate', 'ingest — 1 file']);

  // Kinds, states, timestamps — no detail fetch needed for any of it.
  expect(screen.getByText('done ✓')).toBeTruthy();
  // 'failed' also names a filter button; the row chip is the .chip.bad.
  expect(document.querySelector('.chip.bad')?.textContent).toBe('failed');
  expect(screen.getByText('61%')).toBeTruthy();
  expect(screen.getByText(/started 5m ago/)).toBeTruthy();
  expect(screen.getByText(/2h ago · took 40s/)).toBeTruthy();
});

test('kind and state filters narrow the list; a dry filter says so', async () => {
  installFetch({ jobTimeline: [detailOf(running)] });
  registry.jobs = [running, finished];
  render(Activity);

  await fireEvent.click(screen.getByRole('button', { name: 'refine' }));
  expect(screen.queryByText('ingest — 2 files')).toBeNull();
  expect(screen.getByText('refine — preflate')).toBeTruthy();

  await fireEvent.click(screen.getByRole('button', { name: 'running' }));
  expect(await screen.findByText('nothing matches the filter')).toBeTruthy();
});

test('a running job carries a live detail line without a click', async () => {
  installFetch({
    jobTimeline: [
      detailOf(running, {
        files_total: 2,
        files_done: 1,
        current: 'roms/pack.zip',
        matched_total: 3,
      }),
    ],
  });
  registry.jobs = [running];
  render(Activity);

  expect(await screen.findByText(/1\/2 files/)).toBeTruthy();
  expect(screen.getByText(/roms\/pack\.zip/)).toBeTruthy();
  expect(screen.getByText(/3 matched/)).toBeTruthy();
});

test('expanding a finished job surfaces the report errors — stderr, retired', async () => {
  installFetch({
    jobTimeline: [
      detailOf(finished, {
        report: {
          ...detailOf(finished).report,
          errors: [{ path: 'bad.zip', error: 'no end-of-central-directory record' }],
        },
      }),
    ],
  });
  registry.jobs = [finished];
  render(Activity);

  await fireEvent.click(screen.getByText('refine — preflate'));
  expect(await screen.findByText(/bad\.zip — no end-of-central-directory record/)).toBeTruthy();
  expect(screen.getByText('1 error')).toBeTruthy();
});

test('a failed job shows its infrastructure error on expand', async () => {
  installFetch({
    jobTimeline: [detailOf(failed, { error: 'catalog refresh: boom' })],
  });
  registry.jobs = [failed];
  render(Activity);

  await fireEvent.click(screen.getByText('ingest — 1 file'));
  expect(await screen.findByText('catalog refresh: boom')).toBeTruthy();
});

test('an empty registry says so; an unreachable daemon wears the chip', async () => {
  installFetch({});
  render(Activity);
  expect(screen.getByText('no jobs yet — ingest something')).toBeTruthy();

  registry.unreachable = true;
  expect(await screen.findByText("can't reach the daemon")).toBeTruthy();
});
