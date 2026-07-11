import { fireEvent, render, screen } from '@testing-library/svelte';
import { loadLocale } from 'wuchale/load-utils';
import { afterEach, expect, test, vi } from 'vitest';
import '../../locales/main.loader.svelte.js';
import type { Job, JobDetailBody } from '../api/types';
import { jobsSignal } from '../jobs.svelte';
import { installFetch, type MockUniverse } from '../../test/mock-api';
import JobRow from './JobRow.svelte';
import JobsTray from './JobsTray.svelte';

await loadLocale('en');

afterEach(() => {
  vi.unstubAllGlobals();
  vi.useRealTimers();
});

const running: Job = {
  id: 391,
  name: 'ingest — 2 files',
  progress: 61,
  kind: 'ingest',
  state: 'running',
};

/** A JobDetailBody over a tray row — counters default to zero. */
function detailOf(job: Job, over: Partial<JobDetailBody> = {}): JobDetailBody {
  return {
    ...job,
    files_total: 1,
    files_done: 0,
    bytes_total: 8,
    bytes_done: 0,
    started_at: 1000,
    finished_at: null,
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

function jobsCalls(handler: ReturnType<typeof installFetch>): number {
  return handler.mock.calls.filter(([input]) => String(input) === '/v1/jobs').length;
}

test('idle tray renders the truthful collapsed state', async () => {
  installFetch({ jobs: [] });
  render(JobsTray);
  expect(await screen.findByText('▸ jobs (0)')).toBeTruthy();
  expect(screen.getByText('activity ▾')).toBeTruthy();
});

test('a running job gets a row with name, bar, and percent', async () => {
  installFetch({ jobs: [running] });
  render(JobsTray);
  expect(await screen.findByText('▸ jobs (1)')).toBeTruthy();
  expect(screen.getByText('ingest — 2 files')).toBeTruthy();
  expect(screen.getByText('61%')).toBeTruthy();
  const fill = document.querySelector<HTMLElement>('.fill');
  expect(fill?.style.width).toBe('61%');
});

test('a finished job row flips its label to done ✓', () => {
  render(JobRow, { job: { id: 392, name: 'ingest — 1 file', progress: 100 } });
  expect(screen.getByText('done ✓')).toBeTruthy();
  expect(screen.queryByText('100%')).toBeNull();
});

test('tray polls while a job runs and stops when it finishes', async () => {
  vi.useFakeTimers();
  const universe: MockUniverse = { jobs: [running] };
  const handler = installFetch(universe);
  render(JobsTray);

  await vi.advanceTimersByTimeAsync(0);
  expect(jobsCalls(handler)).toBe(1);

  // Still running → the 2 s cadence fires again.
  await vi.advanceTimersByTimeAsync(2000);
  expect(jobsCalls(handler)).toBe(2);

  // The job finishes: the next poll sees done and the loop stops.
  universe.jobs = [{ ...running, progress: 100, state: 'done' }];
  await vi.advanceTimersByTimeAsync(2000);
  expect(jobsCalls(handler)).toBe(3);
  await vi.advanceTimersByTimeAsync(20_000);
  expect(jobsCalls(handler)).toBe(3);
});

test('an idle tray costs exactly one request — no polling theater', async () => {
  vi.useFakeTimers();
  const handler = installFetch({ jobs: [] });
  render(JobsTray);
  await vi.advanceTimersByTimeAsync(20_000);
  expect(jobsCalls(handler)).toBe(1);
});

test('expanding lists registry jobs with a live detail line', async () => {
  installFetch({
    jobs: [running],
    jobTimeline: [
      detailOf(running, {
        files_total: 2,
        files_done: 1,
        current: 'roms/pack.zip',
        matched_total: 3,
        report: {
          ...detailOf(running).report,
          errors: [{ path: 'bad.zip', error: 'central directory lies' }],
        },
      }),
    ],
  });
  render(JobsTray);
  await fireEvent.click(await screen.findByText('▸ jobs (1)'));
  expect(screen.getByText('▾ jobs (1)')).toBeTruthy();
  // The detail line: files · current · matched · refused.
  expect(await screen.findByText(/1\/2 files/)).toBeTruthy();
  expect(screen.getByText(/roms\/pack\.zip/)).toBeTruthy();
  expect(screen.getByText(/3 matched/)).toBeTruthy();
  expect(screen.getByText(/1 refused/)).toBeTruthy();
});

test('a failed job shows its error in the panel', async () => {
  const failed: Job = {
    id: 7,
    name: 'ingest — 1 file',
    progress: 100,
    kind: 'ingest',
    state: 'failed',
  };
  installFetch({
    jobs: [failed],
    jobTimeline: [detailOf(failed, { finished_at: 1002, error: 'catalog refresh: boom' })],
  });
  render(JobsTray);
  await fireEvent.click(await screen.findByText('▸ jobs (1)'));
  expect(await screen.findByText('failed')).toBeTruthy();
  expect(await screen.findByText('catalog refresh: boom')).toBeTruthy();
});

test('collapse hides the panel; an empty registry says so', async () => {
  installFetch({ jobs: [] });
  render(JobsTray);
  await fireEvent.click(await screen.findByText('▸ jobs (0)'));
  expect(await screen.findByText('no jobs yet — ingest something')).toBeTruthy();
  await fireEvent.click(screen.getByText('▾ jobs (0)'));
  expect(screen.queryByText('no jobs yet — ingest something')).toBeNull();
  expect(screen.getByText('▸ jobs (0)')).toBeTruthy();
});

test('a jobsSignal bump refetches immediately', async () => {
  vi.useFakeTimers();
  const handler = installFetch({ jobs: [] });
  render(JobsTray);
  await vi.advanceTimersByTimeAsync(0);
  expect(jobsCalls(handler)).toBe(1);
  jobsSignal.bump();
  await vi.advanceTimersByTimeAsync(0);
  expect(jobsCalls(handler)).toBe(2);
});
