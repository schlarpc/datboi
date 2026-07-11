<script lang="ts">
  /**
   * Persistent jobs tray (spec §2.2), bottom of every owner screen,
   * fed by the in-memory job registry (/v1/jobs). Cadence keeps the
   * old "no theater" rule: one fetch on mount, then a 2 s re-poll
   * ONLY while a job is actually running — an idle daemon still costs
   * exactly one request per mount. jobsSignal (a screen just started
   * a job) restarts the loop immediately.
   *
   * `activity ▾` (recent activity log) is reserved per the spec but
   * disabled: finished jobs vanish with the in-memory registry, so a
   * real feed wants the durable job table (open-questions).
   */
  import { jobs as fetchJobs } from '../api/client';
  import type { Job } from '../api/types';
  import { jobsSignal } from '../jobs.svelte';
  import JobRow from './JobRow.svelte';

  let jobs = $state<Job[]>([]);

  // Lowercase title copy — forced into the catalog at statement level.
  // @wc-include
  const activityTitle = 'recent activity log — needs a durable job table';

  const POLL_MS = 2000;

  $effect(() => {
    void jobsSignal.version; // a bump re-runs the effect: immediate refetch
    let timer: ReturnType<typeof setTimeout> | undefined;
    let cancelled = false;
    const poll = () => {
      fetchJobs().then(
        (body) => {
          if (cancelled) return;
          jobs = body.jobs;
          if (body.jobs.some((job) => job.state === 'running')) {
            timer = setTimeout(poll, POLL_MS);
          }
        },
        () => {
          // Degrade to idle on any error (a friend's 403 included)
          // and stop polling.
          if (!cancelled) jobs = [];
        },
      );
    };
    poll();
    return () => {
      cancelled = true;
      clearTimeout(timer);
    };
  });
</script>

<footer class="tray">
  <span class="expander">▸ jobs ({jobs.length})</span>
  {#each jobs as job (job.id)}
    <JobRow {job} />
  {/each}
  <span class="activity" title={activityTitle}>activity ▾</span>
</footer>

<style>
  .tray {
    display: flex;
    align-items: center;
    gap: 16px;
    padding: 8px 28px;
    border-top: 2px solid var(--ink);
    background: var(--tray);
    font: 500 12px var(--font-data);
    color: var(--mut);
  }

  .activity {
    margin-left: auto;
    color: var(--faint);
  }
</style>
