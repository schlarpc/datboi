<script lang="ts">
  /**
   * Persistent jobs tray (spec §2.2), bottom of every owner screen.
   * /v1/jobs truthfully returns an empty list today (no job registry —
   * docs/open-questions.md § "Jobs tray backend"), so the live render
   * is the collapsed idle state `▸ jobs (0)`; the per-job rows exist
   * (JobRow) and light up when a registry lands. One fetch, no polling
   * — polling an endpoint known to answer [] would be theater.
   *
   * `activity ▾` (recent activity log) is reserved per the spec but
   * disabled: there is no activity feed to expand yet.
   */
  import { jobs as fetchJobs } from '../api/client';
  import JobRow, { isTrayJob, type TrayJob } from './JobRow.svelte';

  let jobs = $state<TrayJob[]>([]);

  // Lowercase title copy — forced into the catalog at statement level.
  // @wc-include
  const activityTitle = 'recent activity log — needs a job registry';

  $effect(() => {
    fetchJobs().then(
      // The contract's Job is shapeless (no registry yet, D69) — narrow
      // to the tray's rendering shape; unrenderable items are dropped.
      (body) => (jobs = (body.jobs as unknown[]).filter(isTrayJob)),
      () => (jobs = []), // tray degrades to idle on any error
    );
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
