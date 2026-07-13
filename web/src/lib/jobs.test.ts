import { afterEach, expect, test, vi } from 'vitest';
import type { Job, JobDetailBody } from './api/types';
import { followJob, LostContact } from './jobs.svelte';
import { calledPath, installFetch, type MockUniverse } from '../test/mock-api';

afterEach(() => {
  vi.unstubAllGlobals();
  vi.useRealTimers();
});

const running: Job = {
  id: 7,
  name: 'ingest — 1 file',
  progress: 50,
  kind: 'ingest',
  state: 'running',
  started_at: 1000,
  finished_at: null,
};

function detailOf(over: Partial<JobDetailBody> = {}): JobDetailBody {
  return {
    ...running,
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

test('followJob rides out a transient blip — lost contact is not a job failure', async () => {
  vi.useFakeTimers();
  const universe: MockUniverse = {
    jobTimeline: [detailOf(), detailOf({ state: 'done', progress: 100 })],
  };
  installFetch(universe);

  const followed = followJob(7);
  await vi.advanceTimersByTimeAsync(0); // poll 1: running

  universe.jobDetailFail = true;
  await vi.advanceTimersByTimeAsync(3000); // three failed polls — inside grace
  universe.jobDetailFail = false;
  await vi.advanceTimersByTimeAsync(1000); // recovery poll: terminal

  expect((await followed)?.state).toBe('done');
});

test('followJob gives up with LostContact only after sustained failure', async () => {
  vi.useFakeTimers();
  installFetch({ jobTimeline: [detailOf()], jobDetailFail: true });

  const followed = followJob(7);
  const outcome = followed.then(
    () => 'resolved',
    (e: unknown) => e,
  );
  await vi.advanceTimersByTimeAsync(10_000);
  expect(await outcome).toBeInstanceOf(LostContact);
});

test('followJob stops polling the moment alive() goes false', async () => {
  vi.useFakeTimers();
  const handler = installFetch({ jobTimeline: [detailOf()] }); // running forever
  const polls = () =>
    handler.mock.calls.filter(([input]) => calledPath(input).startsWith('/v1/jobs/')).length;

  let alive = true;
  const followed = followJob(7, { alive: () => alive });
  await vi.advanceTimersByTimeAsync(0); // poll 1: running
  const before = polls();

  alive = false; // the screen unmounted
  await vi.advanceTimersByTimeAsync(10_000);
  expect(await followed).toBeNull();
  expect(polls()).toBe(before); // not a single poll after cancellation
});
