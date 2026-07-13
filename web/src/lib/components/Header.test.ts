import { render, screen } from '@testing-library/svelte';
import { loadLocale } from 'wuchale/load-utils';
import { afterEach, beforeEach, expect, test, vi } from 'vitest';
import '../../locales/main.loader.svelte.js';
import type { Job } from '../api/types';
import { registry } from '../activity.svelte';
import { jobsSignal } from '../jobs.svelte';
import { calledPath, installFetch, type MockUniverse } from '../../test/mock-api';
import Header from './Header.svelte';

await loadLocale('en');

// The Header owns the ONE registry poll loop (D82, activity.svelte.ts)
// — these tests pin the cadence rules the old tray established.

const nowSecs = Math.floor(Date.now() / 1000);

const running: Job = {
  id: 391,
  name: 'ingest — 2 files',
  progress: 61,
  kind: 'ingest',
  state: 'running',
  started_at: nowSecs - 60,
  finished_at: null,
};

beforeEach(() => {
  registry.jobs = [];
  registry.unreachable = false;
});

afterEach(() => {
  vi.unstubAllGlobals();
  vi.useRealTimers();
});

function jobsCalls(handler: ReturnType<typeof installFetch>): number {
  return handler.mock.calls.filter(([input]) => calledPath(input) === '/v1/jobs').length;
}

test('idle: one request, a quiet activity link into the history', async () => {
  vi.useFakeTimers();
  const handler = installFetch({ jobs: [] });
  render(Header);
  await vi.advanceTimersByTimeAsync(20_000);
  expect(jobsCalls(handler)).toBe(1); // no polling theater
  const link = screen.getByText('activity').closest('a');
  expect(link?.getAttribute('href')).toBe('/activity');
  expect(link?.classList.contains('activity-live')).toBe(false);
});

test('a running job turns the link live with a count, polling until done', async () => {
  vi.useFakeTimers();
  const universe: MockUniverse = { jobs: [running] };
  const handler = installFetch(universe);
  render(Header);

  await vi.advanceTimersByTimeAsync(0);
  expect(jobsCalls(handler)).toBe(1);
  const live = screen.getByText('1 job').closest('a');
  expect(live?.classList.contains('activity-live')).toBe(true);

  // Still running → the 2 s cadence fires again.
  await vi.advanceTimersByTimeAsync(2000);
  expect(jobsCalls(handler)).toBe(2);

  // The job finishes: the next poll sees done and the loop stops.
  universe.jobs = [{ ...running, progress: 100, state: 'done', finished_at: nowSecs }];
  await vi.advanceTimersByTimeAsync(2000);
  expect(jobsCalls(handler)).toBe(3);
  await vi.advanceTimersByTimeAsync(20_000);
  expect(jobsCalls(handler)).toBe(3);
  expect(screen.getByText('activity')).toBeTruthy();
});

test('a failed poll mid-job keeps the last-known count and heals', async () => {
  vi.useFakeTimers();
  const universe: MockUniverse = { jobs: [running] };
  installFetch(universe);
  render(Header);

  await vi.advanceTimersByTimeAsync(0);
  expect(screen.getByText('1 job')).toBeTruthy();

  // The daemon blips: last-known job stays, the ? marks the doubt.
  universe.jobsFail = true;
  await vi.advanceTimersByTimeAsync(2000);
  expect(screen.getByText('1 job')).toBeTruthy();
  expect(screen.getByText('?')).toBeTruthy();

  // ...and heals on the next successful poll.
  universe.jobsFail = false;
  await vi.advanceTimersByTimeAsync(2000);
  expect(screen.queryByText('?')).toBeNull();
});

test('a jobsSignal bump refetches immediately', async () => {
  vi.useFakeTimers();
  const handler = installFetch({ jobs: [] });
  render(Header);
  await vi.advanceTimersByTimeAsync(0);
  expect(jobsCalls(handler)).toBe(1);
  jobsSignal.bump();
  await vi.advanceTimersByTimeAsync(0);
  expect(jobsCalls(handler)).toBe(2);
});
