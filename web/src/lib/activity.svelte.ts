/**
 * Shared job-registry snapshot (D82: the jobs tray died; the header
 * indicator + /activity page replaced it).
 *
 * The Header runs the ONE poll loop via `trackRegistry()` — it is
 * mounted on every owner screen, including /activity, so the snapshot
 * is live anywhere it can be read. The activity page consumes this
 * state and fetches only per-job detail on top; it deliberately does
 * NOT run a second list poll. Cadence keeps the old "no theater"
 * rule: one fetch on subscribe, then a 2 s re-poll ONLY while a job
 * is actually running — an idle daemon costs exactly one request per
 * mount. jobsSignal (a screen just started a job) restarts the loop
 * immediately; hidden tabs don't poll.
 */
import { jobs as fetchJobs } from './api/client';
import type { Job } from './api/types';
import { jobsSignal } from './jobs.svelte';

export const POLL_MS = 2000;

export const registry = $state({
  jobs: [] as Job[],
  /** The last poll failed: `jobs` is last-known, not live. */
  unreachable: false,
});

export const runningCount = (): number =>
  registry.jobs.filter((job) => job.state === 'running').length;

/**
 * Call during component init (the Header does). Owns the poll loop
 * described above for as long as the caller is mounted.
 */
export function trackRegistry(): void {
  let visible = $state(!document.hidden);
  $effect(() => {
    const onVis = () => (visible = !document.hidden);
    document.addEventListener('visibilitychange', onVis);
    return () => document.removeEventListener('visibilitychange', onVis);
  });

  $effect(() => {
    void jobsSignal.version; // a bump re-runs the effect: immediate refetch
    if (!visible) return; // hidden: no poll burns while nobody watches
    let timer: ReturnType<typeof setTimeout> | undefined;
    let cancelled = false;
    const poll = () => {
      fetchJobs().then(
        (body) => {
          if (cancelled) return;
          registry.unreachable = false;
          registry.jobs = body.jobs;
          if (body.jobs.some((job) => job.state === 'running')) {
            timer = setTimeout(poll, POLL_MS);
          }
        },
        () => {
          // A failed poll means "can't reach the daemon", not "the
          // daemon is idle" — keep the last-known jobs, say so, and
          // re-poll on the running cadence iff the last snapshot had
          // a job running, so a blip mid-job heals itself.
          if (cancelled) return;
          registry.unreachable = true;
          if (registry.jobs.some((job) => job.state === 'running')) {
            timer = setTimeout(poll, POLL_MS);
          }
        },
      );
    };
    poll();
    return () => {
      cancelled = true;
      clearTimeout(timer);
    };
  });
}
