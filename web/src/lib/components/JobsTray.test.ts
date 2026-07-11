import { render, screen } from '@testing-library/svelte';
import { loadLocale } from 'wuchale/load-utils';
import { afterEach, expect, test, vi } from 'vitest';
import '../../locales/main.loader.svelte.js';
import type { Job } from '../api/types';
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
