<script lang="ts">
  /**
   * Activity — job history over the D74 ledger (D82). The one home for
   * "what is/was the daemon doing": kind + state filters, relative
   * timestamps, live progress for running jobs, and expandable per-job
   * detail including the report's per-item errors — the stuff that
   * used to be visible only on the daemon's stderr.
   *
   * Data flow: the jobs LIST rides the Header's shared registry poll
   * (activity.svelte.ts — one loop for the whole app); this screen
   * fetches only per-job DETAIL, on the tray's old rules: running jobs
   * refresh with every registry change, finished ones fetch once
   * (their reports are final) plus the change that sees the state
   * flip, so a cached running snapshot gets its final counters.
   */
  import { jobDetail } from '../lib/api/client';
  import type { JobDetailBody, JobKind, JobRunState } from '../lib/api/types';
  import { registry } from '../lib/activity.svelte';
  import { assertNever } from '../lib/exhaustive';
  import { fmtAge, fmtDuration } from '../lib/format';
  import { plural } from '../lib/plural';

  const KINDS: ('all' | JobKind)[] = [
    'all',
    'ingest',
    'refine',
    'gc',
    'scrub',
    'eval',
    'mint',
    'sync',
  ];
  const STATES: ('all' | JobRunState)[] = ['all', 'running', 'done', 'failed'];

  let kindFilter = $state<'all' | JobKind>('all');
  let stateFilter = $state<'all' | JobRunState>('all');
  /** Rows toggled open by click; running rows show their line anyway. */
  let expanded = $state<Record<number, boolean>>({});
  /** Detail bodies by job id, fetched lazily per the header comment. */
  let details = $state<Record<number, JobDetailBody>>({});

  /** Ticks so "5m ago" doesn't fossilize while the page sits open. */
  let now = $state(Date.now());
  $effect(() => {
    const timer = setInterval(() => (now = Date.now()), 30_000);
    return () => clearInterval(timer);
  });

  const rows = $derived(
    registry.jobs
      .filter(
        (job) =>
          (kindFilter === 'all' || job.kind === kindFilter) &&
          (stateFilter === 'all' || job.state === stateFilter),
      )
      .slice()
      .sort((a, b) => b.id - a.id),
  );

  $effect(() => {
    const want = registry.jobs.filter(
      (job) =>
        (job.state === 'running' || expanded[job.id] === true) &&
        (job.state === 'running' ||
          details[job.id] === undefined ||
          details[job.id].state !== job.state),
    );
    if (want.length === 0) return;
    let cancelled = false;
    void Promise.all(want.map((job) => jobDetail(job.id).catch(() => undefined))).then(
      (fetched) => {
        if (cancelled) return;
        for (const d of fetched) {
          if (d !== undefined) details[d.id] = d;
        }
      },
    );
    return () => {
      cancelled = true;
    };
  });

  /** The report card's refused arithmetic (Ingest.svelte). */
  const refused = (d: JobDetailBody): number =>
    d.report.errors.length + d.report.member_skips.length + Number(d.report.skipper_skipped_large);
</script>

<main>
  <div class="title-row">
    <h2>Activity</h2>
    <span class="sub">every job the daemon ran — live and ledgered (D74)</span>
    {#if registry.unreachable}
      <span class="chip bad">can't reach the daemon</span>
    {/if}
  </div>

  <div class="filters">
    <div class="seg-group">
      {#each KINDS as kind (kind)}
        <button
          class="seg"
          class:active={kindFilter === kind}
          aria-pressed={kindFilter === kind}
          onclick={() => (kindFilter = kind)}
        >
          {#if kind === 'all'}
            <!-- @wc-context: job kind filter -->all
          {:else if kind === 'ingest'}
            <!-- @wc-context: job kind -->ingest
          {:else if kind === 'refine'}
            <!-- @wc-context: job kind -->refine
          {:else if kind === 'gc'}
            <!-- @wc-context: job kind -->gc
          {:else if kind === 'scrub'}
            <!-- @wc-context: job kind -->scrub
          {:else if kind === 'eval'}
            <!-- @wc-context: job kind -->eval
          {:else if kind === 'mint'}
            <!-- @wc-context: job kind -->mint
          {:else if kind === 'sync'}
            <!-- @wc-context: job kind -->sync
          {:else}
            {assertNever(kind)}
          {/if}
        </button>
      {/each}
    </div>
    <div class="seg-group">
      {#each STATES as st (st)}
        <button
          class="seg"
          class:active={stateFilter === st}
          aria-pressed={stateFilter === st}
          onclick={() => (stateFilter = st)}
        >
          {#if st === 'all'}
            <!-- @wc-context: job state filter -->all
          {:else if st === 'running'}
            <!-- @wc-context: job state -->running
          {:else if st === 'done'}
            <!-- @wc-context: job state -->done
          {:else if st === 'failed'}
            <!-- @wc-context: job state -->failed
          {:else}
            {assertNever(st)}
          {/if}
        </button>
      {/each}
    </div>
  </div>

  {#if rows.length === 0}
    {#if registry.jobs.length === 0}
      <p class="empty">no jobs yet — ingest something</p>
    {:else}
      <p class="empty">nothing matches the filter</p>
    {/if}
  {:else}
    <ul class="jobs">
      {#each rows as job (job.id)}
        {@const d = details[job.id]}
        <li class="job">
          <button
            class="job-row"
            aria-expanded={job.state === 'running' || expanded[job.id] === true}
            onclick={() => (expanded[job.id] = expanded[job.id] !== true)}
          >
            <span class="kind">{job.kind}</span>
            <span class="name">{job.name}</span>
            {#if job.state === 'running'}
              <span class="track"><span class="fill" style:width="{job.progress}%"></span></span>
              <span class="pct">{job.progress}%</span>
            {:else if job.state === 'done'}
              <span class="chip ok">done ✓</span>
            {:else if job.state === 'failed'}
              <span class="chip bad"><!-- @wc-context: job state -->failed</span>
            {:else}
              <!-- A new JobRunState fails check here — never a confident
                   red "failed" chip for an unknown state. -->
              {assertNever(job.state)}
            {/if}
            <span class="when">
              {#if job.finished_at === null}
                started {fmtAge(job.started_at, now)} ago
              {:else}
                {fmtAge(job.finished_at, now)} ago · took {fmtDuration(
                  job.finished_at - job.started_at,
                )}
              {/if}
            </span>
          </button>

          {#if d !== undefined && (job.state === 'running' || expanded[job.id] === true)}
            <div class="detail">
              {#if job.kind !== 'ingest'}
                <!-- Refine/gc/scrub count ITEMS; their story is the
                     closing note, not matches/refusals. -->
                <p class="line">
                  {d.files_done}/{plural(d.files_total, ['# blob', '# blobs'])}
                  {#if d.current != null}
                    · {d.current.slice(0, 10)}
                  {/if}
                  {#if d.report.notes.length > 0}
                    · {d.report.notes[d.report.notes.length - 1]}
                  {/if}
                </p>
              {:else}
                <p class="line">
                  {d.files_done}/{plural(d.files_total, ['# file', '# files'])}
                  {#if d.current != null}
                    · {d.current}
                  {/if}
                  · {d.matched_total} matched · {refused(d)} refused
                </p>
              {/if}
              {#if d.error}
                <p class="line bad">{d.error}</p>
              {/if}
              {#if d.report.errors.length > 0}
                <div class="errors">
                  <span class="errors-label">
                    {plural(d.report.errors.length, ['# error', '# errors'])}
                  </span>
                  {#each d.report.errors as err (err.path + err.error)}
                    <p class="line bad">{err.path} — {err.error}</p>
                  {/each}
                </div>
              {/if}
            </div>
          {/if}
        </li>
      {/each}
    </ul>
  {/if}
</main>

<style>
  main {
    flex: 1;
    overflow-y: auto;
    padding: 24px var(--pad-x) 30px;
  }

  .title-row {
    display: flex;
    align-items: baseline;
    gap: 14px;
    margin-bottom: 18px;
  }

  h2 {
    margin: 0;
    font: 800 1.5rem var(--font-display);
    letter-spacing: -0.03em;
  }

  .sub {
    font: 400 0.8125rem var(--font-data);
    color: var(--faint);
  }

  .filters {
    display: flex;
    gap: 14px;
    flex-wrap: wrap;
    margin-bottom: 16px;
  }

  .seg-group {
    display: flex;
    border: 1.5px solid var(--hair);
    border-radius: var(--r-pill);
    overflow: hidden;
  }

  .seg {
    all: unset;
    padding: 3px 12px;
    font: 500 0.71875rem var(--font-data);
    color: var(--faint);
    cursor: pointer;
  }

  .seg.active {
    background: var(--ink);
    color: var(--bg);
    font-weight: 600;
  }

  .empty {
    font: 400 0.78125rem var(--font-data);
    color: var(--faint);
  }

  .jobs {
    list-style: none;
    margin: 0;
    padding: 0;
    background: var(--panel);
    border: 1px solid var(--hair);
    border-radius: var(--r-sub);
  }

  .job + .job {
    border-top: 1px solid var(--rule);
  }

  .job-row {
    all: unset;
    outline: revert;
    display: flex;
    align-items: center;
    gap: 12px;
    width: 100%;
    box-sizing: border-box;
    padding: 9px 16px;
    cursor: pointer;
    font: 500 0.75rem var(--font-data);
    color: var(--mut);
  }

  .kind {
    flex: none;
    width: 46px;
    color: var(--faint);
  }

  .name {
    font-weight: 600;
    color: var(--text);
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .track {
    width: 120px;
    height: 5px;
    border-radius: var(--r-fill);
    background: var(--panel2);
    overflow: hidden;
    flex: none;
  }

  .fill {
    display: block;
    height: 100%;
    background: var(--ok);
    transition: width 0.5s linear;
  }

  .pct {
    color: var(--faint);
  }

  .chip {
    color: var(--faint);
  }

  .chip.ok {
    color: var(--okT);
  }

  .chip.bad {
    color: var(--bad);
  }

  .when {
    margin-left: auto;
    flex: none;
    font: 400 0.71875rem var(--font-data);
    color: var(--faint);
  }

  .detail {
    padding: 0 16px 10px 74px;
  }

  .line {
    margin: 2px 0 0;
    font: 400 0.71875rem var(--font-data);
    color: var(--faint);
    overflow-wrap: anywhere;
  }

  .line.bad {
    color: var(--bad);
  }

  .errors {
    margin-top: 4px;
  }

  .errors-label {
    font: 600 0.65625rem var(--font-data);
    color: var(--bad);
    letter-spacing: 0.04em;
  }
</style>
