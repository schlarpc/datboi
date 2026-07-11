<script lang="ts">
  /**
   * Persistent jobs tray (spec §2.2), bottom of every owner screen,
   * fed by the in-memory job registry (/v1/jobs). Cadence keeps the
   * old "no theater" rule: one fetch on mount, then a 2 s re-poll
   * ONLY while a job is actually running — an idle daemon still costs
   * exactly one request per mount. jobsSignal (a screen just started
   * a job) restarts the loop immediately.
   *
   * `▸ jobs` expands an over-tray panel of the whole registry (running
   * + finished history, newest first) with per-job detail lines from
   * /v1/jobs/{id}. Details ride the SAME poll loop — opening the panel
   * re-runs the effect, so there is no second timer system.
   *
   * `activity ▾` (recent activity log) is reserved per the spec but
   * disabled: finished jobs vanish with the in-memory registry, so a
   * real feed wants the durable job table (open-questions).
   */
  import { jobDetail, jobs as fetchJobs } from '../api/client';
  import type { Job, JobDetailBody } from '../api/types';
  import { jobsSignal } from '../jobs.svelte';
  import JobRow from './JobRow.svelte';

  let jobs = $state<Job[]>([]);
  let open = $state(false);
  /** Finished job whose detail line was toggled open by a row click. */
  let selected = $state<number | null>(null);
  /** Detail bodies by job id, fetched lazily while the panel is open. */
  let details = $state<Record<number, JobDetailBody>>({});

  // Lowercase title copy — forced into the catalog at statement level.
  // @wc-include
  const activityTitle = 'recent activity log — needs a durable job table';

  const POLL_MS = 2000;

  /** The report card's refused arithmetic (Ingest.svelte). */
  const refused = (d: JobDetailBody): number =>
    d.report.errors.length + d.report.member_skips.length + Number(d.report.skipper_skipped_large);

  $effect(() => {
    void jobsSignal.version; // a bump re-runs the effect: immediate refetch
    const detailsWanted = open; // panel toggles also re-run: details join the loop
    let timer: ReturnType<typeof setTimeout> | undefined;
    let cancelled = false;
    const poll = () => {
      fetchJobs().then(
        async (body) => {
          if (cancelled) return;
          jobs = body.jobs;
          if (detailsWanted) {
            // Lazy, on the same cadence: running jobs refresh every
            // cycle; finished ones fetch once (their reports are
            // final) — plus the cycle that sees the state flip, so
            // the cached running snapshot gets its final counters.
            const want = body.jobs.filter(
              (job) =>
                job.state === 'running' ||
                details[job.id] === undefined ||
                details[job.id].state !== job.state,
            );
            const fetched = await Promise.all(
              want.map((job) => jobDetail(job.id).catch(() => undefined)),
            );
            if (cancelled) return;
            for (const d of fetched) {
              if (d !== undefined) details[d.id] = d;
            }
          }
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
  <button class="expander" aria-expanded={open} onclick={() => (open = !open)}>
    {#if open}▾ jobs ({jobs.length}){:else}▸ jobs ({jobs.length}){/if}
  </button>
  {#each jobs as job (job.id)}
    <JobRow {job} />
  {/each}
  <span class="activity" title={activityTitle}>activity ▾</span>

  {#if open}
    <div class="panel">
      {#if jobs.length === 0}
        <p class="empty">no jobs yet — ingest something</p>
      {:else}
        <ul>
          {#each jobs as job (job.id)}
            {@const detail = details[job.id]}
            <li>
              <button
                class="row"
                onclick={() => (selected = selected === job.id ? null : job.id)}
              >
                {#if job.state === 'running'}
                  <JobRow {job} />
                {:else if job.state === 'done'}
                  <span class="name">{job.name}</span>
                  <span class="chip">done ✓</span>
                {:else}
                  <span class="name">{job.name}</span>
                  <span class="chip bad"><!-- @wc-context: job state -->failed</span>
                  {#if detail?.error}
                    <span class="chip bad">{detail.error}</span>
                  {/if}
                {/if}
              </button>
              {#if detail !== undefined && (job.state === 'running' || selected === job.id)}
                <p class="detail">
                  {detail.files_done}/{detail.files_total} files
                  {#if detail.current != null}
                    · {detail.current}
                  {/if}
                  · {detail.matched_total} matched · {refused(detail)} refused
                </p>
              {/if}
            </li>
          {/each}
        </ul>
      {/if}
    </div>
  {/if}
</footer>

<style>
  .tray {
    position: relative; /* the expand-up panel anchors to the strip */
    display: flex;
    align-items: center;
    gap: 16px;
    padding: 8px 28px;
    border-top: 2px solid var(--ink);
    background: var(--tray);
    font: 500 12px var(--font-data);
    color: var(--mut);
  }

  .expander {
    all: unset;
    cursor: pointer;
    outline: revert; /* keep the focus ring `all: unset` would eat */
  }

  .activity {
    margin-left: auto;
    color: var(--faint);
  }

  .panel {
    position: absolute;
    left: 28px;
    bottom: calc(100% + 10px);
    width: min(560px, calc(100vw - 56px));
    max-height: 320px;
    overflow-y: auto;
    background: var(--panel);
    border: 2px solid var(--ink);
    border-radius: var(--r-card);
    box-shadow: var(--shadow-card);
    padding: 12px 16px;
  }

  .empty {
    margin: 0;
    color: var(--faint);
  }

  .panel ul {
    list-style: none;
    margin: 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: 8px;
  }

  .row {
    all: unset;
    outline: revert;
    display: flex;
    align-items: center;
    gap: 10px;
    width: 100%;
    cursor: pointer;
    font: 500 12px var(--font-data);
    color: var(--mut);
  }

  .name {
    font-weight: 600;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .chip {
    color: var(--faint);
  }

  .chip.bad {
    color: var(--bad);
  }

  .detail {
    margin: 2px 0 0;
    font: 400 11.5px var(--font-data);
    color: var(--faint);
  }
</style>
