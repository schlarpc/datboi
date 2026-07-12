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
   * still disabled: the D74 ledger now persists history (and CLI jobs
   * appear here via the poll-time merge), but the feed UI itself is
   * undesigned — the data is ready when it is.
   */
  import { jobDetail, jobs as fetchJobs } from '../api/client';
  import type { Job, JobDetailBody } from '../api/types';
  import { assertNever } from '../exhaustive';
  import { jobsSignal } from '../jobs.svelte';
  import JobRow from './JobRow.svelte';
  import { plural } from '../plural';

  let jobs = $state<Job[]>([]);
  /** The last poll failed: the list below is last-known, not live. */
  let unreachable = $state(false);
  let open = $state(false);
  /** Finished job whose detail line was toggled open by a row click. */
  let selected = $state<number | null>(null);
  /** Detail bodies by job id, fetched lazily while the panel is open. */
  let details = $state<Record<number, JobDetailBody>>({});

  // Lowercase title copy — forced into the catalog at statement level.
  // @wc-include
  const activityTitle = 'recent activity log — history is recorded (D74); the feed UI is not designed yet';

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
          unreachable = false;
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
          // A failed poll means "can't reach the daemon", not "the
          // daemon is idle" — keep the last-known jobs, say so, and
          // re-poll on the running cadence iff the last snapshot had a
          // job running (mirror of the success arm), so a blip mid-job
          // heals itself when the daemon comes back. An idle tray
          // still costs exactly one request.
          if (cancelled) return;
          unreachable = true;
          if (jobs.some((job) => job.state === 'running')) {
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
</script>

<footer class="tray">
  <div class="strip">
    <button class="expander" aria-expanded={open} onclick={() => (open = !open)}>
      {#if open}▾ jobs ({jobs.length}){:else}▸ jobs ({jobs.length}){/if}
    </button>
    {#if unreachable}
      <span class="chip bad">can't reach the daemon</span>
    {/if}
    {#each jobs as job (job.id)}
      <JobRow {job} />
    {/each}
    <span class="activity" title={activityTitle}>activity ▾</span>
  </div>

  {#if open}
    <div class="panel">
      {#if jobs.length === 0}
        {#if unreachable}
          <p class="empty">can't reach the daemon</p>
        {:else}
          <p class="empty">no jobs yet — ingest something</p>
        {/if}
      {:else}
        <ul>
          {#each jobs as job (job.id)}
            {@const detail = details[job.id]}
            <li>
              <button
                class="row"
                aria-expanded={job.state === 'running' || selected === job.id}
                onclick={() => (selected = selected === job.id ? null : job.id)}
              >
                {#if job.state === 'running'}
                  <JobRow {job} />
                {:else if job.state === 'done'}
                  <span class="name">{job.name}</span>
                  <span class="chip">done ✓</span>
                {:else if job.state === 'failed'}
                  <span class="name">{job.name}</span>
                  <span class="chip bad"><!-- @wc-context: job state -->failed</span>
                  {#if detail?.error}
                    <span class="chip bad">{detail.error}</span>
                  {/if}
                {:else}
                  <!-- A new JobRunState fails check here — never a
                       confident red "failed" chip for an unknown state. -->
                  {assertNever(job.state)}
                {/if}
              </button>
              {#if detail !== undefined && (job.state === 'running' || selected === job.id)}
                {#if job.kind !== 'ingest'}
                  <!-- Refine/gc/scrub count ITEMS, and their story is
                       the closing note, not matches/refusals. -->
                  <p class="detail">
                    {detail.files_done}/{plural(detail.files_total, ['# blob', '# blobs'])}
                    {#if detail.current != null}
                      · {detail.current.slice(0, 10)}…
                    {/if}
                    {#if detail.report.notes.length > 0}
                      · {detail.report.notes[detail.report.notes.length - 1]}
                    {:else if detail.report.errors.length > 0}
                      · {plural(detail.report.errors.length, ['# error', '# errors'])}
                    {/if}
                  </p>
                {:else}
                  <p class="detail">
                    {detail.files_done}/{plural(detail.files_total, ['# file', '# files'])}
                    {#if detail.current != null}
                      · {detail.current}
                    {/if}
                    · {detail.matched_total} matched · {refused(detail)} refused
                  </p>
                {/if}
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
    /* The expand-up panel anchors here — a SIBLING of the inline
       strip, never a child, so the strip's sideways scroll (mobile)
       can never clip the panel out of reach. */
    position: relative;
    border-top: 2px solid var(--ink);
    background: var(--tray);
    font: 500 12px var(--font-data);
    color: var(--mut);
  }

  .strip {
    display: flex;
    align-items: center;
    gap: 16px;
    padding: 8px var(--pad-x);
  }

  .expander {
    all: unset;
    cursor: pointer;
  }

  .activity {
    margin-left: auto;
    color: var(--faint);
  }

  .panel {
    position: absolute;
    left: var(--pad-x);
    bottom: calc(100% + 10px);
    width: min(560px, calc(100vw - 2 * var(--pad-x)));
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

  /* The strip carries the expander, any running job bars, and the
     activity stub inline; on a narrow screen it scrolls sideways rather
     than clipping a job mid-bar. The expander stays put as the anchor. */
  @media (max-width: 720px) {
    .strip {
      gap: 12px;
      overflow-x: auto;
      scrollbar-width: none;
    }

    .strip::-webkit-scrollbar {
      display: none;
    }

    .expander {
      flex: none;
    }
  }
</style>
