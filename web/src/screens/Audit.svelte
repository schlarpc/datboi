<script lang="ts">
  /**
   * Audit / system drill-down (spec §3.2 + §5 — the canonical screen).
   * Bench register: thin rules, mono state words, 9px dots.
   *
   * Data flow rulings:
   * - System header data comes from GET /v1/systems (there is no
   *   single-system endpoint; the list is tiny). Rail counts are the
   *   system's UNFILTERED totals per spec §5.1.
   * - Rows PAGINATE against the API's offset/limit with a "load more"
   *   row (ruling: the server caps pages at 1000 and already does
   *   filter+search, so client-side windowing over a full mirror would
   *   duplicate the query engine; a page of 500 keeps the DOM shallow
   *   without virtualization machinery. Filter and search go to the
   *   server; both compose there exactly like the prototype, §5.1).
   */
  import { entryDetail, systemEntries, systems as fetchSystems } from '../lib/api/client';
  import type { EntryDetail, EntryRow, System } from '../lib/api/types';
  import { bandFor } from '../lib/bands';
  import EntryDrawer from '../lib/components/EntryDrawer.svelte';
  import StackedBar from '../lib/components/StackedBar.svelte';
  import { fmtSize } from '../lib/format';
  import { prefs } from '../lib/prefs.svelte';
  import { completenessPct, ENTRY_STATES, type EntryState } from '../lib/state';

  let { systemId }: { systemId: string } = $props();

  const PAGE = 500;

  let system = $state<System | null>(null);
  let error = $state<string | null>(null);
  let entries = $state<EntryRow[]>([]);
  let total = $state(0);
  let filter = $state<'all' | EntryState>('all');
  let q = $state('');
  let selected = $state<string | null>(null);
  let detail = $state<EntryDetail | null>(null);
  let exporting = $state(false);

  $effect(() => {
    fetchSystems().then(
      (body) => {
        system = body.systems.find((sys) => String(sys.id) === systemId) ?? null;
        if (system === null) {
          error = 'no such system';
        }
      },
      (e: unknown) => (error = e instanceof Error ? e.message : String(e)),
    );
  });

  // Filter/search recompose server-side; stale answers are dropped by
  // generation counter so a slow page can't overwrite a newer one.
  let generation = 0;
  $effect(() => {
    const params = {
      q: q || undefined,
      state: filter === 'all' ? undefined : filter,
    };
    const gen = ++generation;
    systemEntries(systemId, { ...params, offset: 0, limit: PAGE }).then(
      (body) => {
        if (gen === generation) {
          entries = body.entries;
          total = body.total;
        }
      },
      (e: unknown) => (error = e instanceof Error ? e.message : String(e)),
    );
  });

  async function loadMore() {
    const gen = generation;
    const body = await systemEntries(systemId, {
      q: q || undefined,
      state: filter === 'all' ? undefined : filter,
      offset: entries.length,
      limit: PAGE,
    });
    if (gen === generation) {
      entries = [...entries, ...body.entries];
      total = body.total;
    }
  }

  function select(name: string) {
    if (selected === name) {
      selected = null;
      detail = null;
      return;
    }
    selected = name;
    detail = null;
    entryDetail(systemId, name).then(
      (body) => {
        if (selected === name) {
          detail = body;
        }
      },
      () => (detail = null),
    );
  }

  function close() {
    selected = null;
    detail = null;
  }

  function onkeydown(event: KeyboardEvent) {
    // @wc-ignore
    if (event.key === 'Escape' && selected !== null) {
      close();
    }
  }

  // Lowercase attribute copy fails wuchale's attribute heuristic, so it
  // lives here with statement-level force-includes (an element-level
  // directive would also sweep class/type attributes into the catalog).
  // @wc-include
  const historyTitle = 'dat revision history — future';
  // @wc-include
  const diffTitle = 'dat revision diff — future';
  // @wc-include
  const searchPlaceholder = 'filter names…';

  /**
   * §5.5: client-generated missing-list export. Fetches every missing
   * entry (API pages cap at 1000) and downloads a plaintext file.
   */
  async function exportMissing() {
    if (exporting || system === null) return;
    exporting = true;
    try {
      const names: string[] = [];
      for (;;) {
        const page = await systemEntries(systemId, {
          state: 'missing',
          offset: names.length,
          limit: 1000,
        });
        names.push(...page.entries.map((entry) => entry.name));
        if (names.length >= page.total || page.entries.length === 0) break;
      }
      // User-visible file content goes through the catalog too; the
      // directive forces extraction ('#' fails the script heuristic).
      // @wc-include
      const header = `# datboi missing-list · ${system.provider} ${system.system} ${system.revision?.version ?? ''}`;
      const blob = new Blob([`${header.trimEnd()}\n${names.join('\n')}\n`], {
        type: 'text/plain',
      });
      const url = URL.createObjectURL(blob);
      const a = document.createElement('a');
      a.href = url;
      a.download = `${system.system}-missing.txt`;
      a.click();
      URL.revokeObjectURL(url);
    } finally {
      exporting = false;
    }
  }

  const railCounts = $derived(
    system === null
      ? null
      : { all: system.total, ...system.counts },
  );

  /** Selection tint colors per state (spec: ok/warn/bad/dim). */
  const stateColor: Record<EntryState, string> = {
    verified: 'var(--ok)',
    claimed: 'var(--warn)',
    missing: 'var(--bad)',
    nodump: 'var(--dim)',
  };
</script>

<svelte:window {onkeydown} />

<main>
  {#if error !== null}
    <!-- Undesigned loading/error states: plain mono in --faint. -->
    <p class="undesigned">something went wrong — {error}</p>
  {:else if system === null || railCounts === null}
    <p class="undesigned">loading…</p>
  {:else}
    {@const pct = completenessPct(system.counts)}
    <div class="sys-head">
      <div class="row1">
        <span class="accent" style:background={bandFor(system.system)}></span>
        <h2>{system.system}</h2>
        <span class="sub">
          {system.provider}{#if system.revision?.version}
            · {system.revision.version}{/if}
          <!-- Revision picker + history/diff: dat-history screens were
               never designed (spec §8 unresolved) — disabled, future. -->
          <span class="future" title={historyTitle}>▾</span>
          ·
          <span class="future" title={historyTitle}>history</span>
          ·
          <span class="future" title={diffTitle}>diff</span>
        </span>
        <button class="missing-list" onclick={exportMissing} disabled={exporting}>
          ⬇ missing-list
        </button>
      </div>
      <div class="row2">
        <span class="pct">{pct}%</span>
        <div class="bar-wrap"><StackedBar counts={system.counts} register="bench" /></div>
        <span class="counts">
          <span class="c-ok">{system.counts.verified.toLocaleString()}</span> ·
          <span class="c-warn">{system.counts.claimed.toLocaleString()}</span> ·
          <span class="c-bad">{system.counts.missing.toLocaleString()}</span> ·
          <span class="c-faint">{system.counts.nodump.toLocaleString()}</span>
        </span>
      </div>
    </div>

    <div class="table">
      <div class="rail">
        <button class="rail-item" class:sel={filter === 'all'} onclick={() => (filter = 'all')}>
          <span>All</span>
          <span class="count">{railCounts.all.toLocaleString()}</span>
        </button>
        {#each ENTRY_STATES as st (st)}
          <button
            class="rail-item"
            class:sel={filter === st}
            class:nodump={st === 'nodump'}
            onclick={() => (filter = st)}
          >
            <span>
              {#if st === 'verified'}
                <!-- @wc-context: storage state -->● Verified
              {:else if st === 'claimed'}
                <!-- @wc-context: storage state -->◐ Claimed
              {:else if st === 'missing'}
                <!-- @wc-context: storage state -->○ Missing
              {:else}
                <!-- @wc-context: storage state -->– No dump
              {/if}
            </span>
            <span class="count">{railCounts[st].toLocaleString()}</span>
          </button>
        {/each}
        <div class="rail-divider"></div>
        <div class="rail-search">
          <input type="search" placeholder={searchPlaceholder} bind:value={q} />
        </div>
        <!-- Density pref (spec §1.3): specced as a user preference but
             never given a home in the comps — parked at the rail foot. -->
        <div class="rail-density">
          <button
            class="density-seg"
            class:active={prefs.density === 'comfortable'}
            onclick={() => prefs.setDensity('comfortable')}
          >
            comfortable
          </button>
          <button
            class="density-seg"
            class:active={prefs.density === 'compact'}
            onclick={() => prefs.setDensity('compact')}
          >
            compact
          </button>
        </div>
      </div>

      <div class="rows" style:--rowpad={prefs.density === 'compact' ? '4px 20px' : '9px 20px'}>
        {#if entries.length === 0}
          <p class="empty">nothing matches — clear the filter or search</p>
        {:else}
          {#each entries as entry (entry.name)}
            {@const isSel = selected === entry.name}
            <button
              class="row"
              class:sel={isSel}
              style:--state-color={stateColor[entry.state]}
              onclick={() => select(entry.name)}
            >
              <span class="dot dot--{entry.state}"></span>
              <span class="row-name name--{entry.state}" class:bold={isSel}>{entry.name}</span>
              <span class="state-word state-text--{entry.state}">
                {#if entry.state === 'verified'}
                  <!-- @wc-context: storage state -->verified
                {:else if entry.state === 'claimed'}
                  <!-- @wc-context: storage state -->claimed
                {:else if entry.state === 'missing'}
                  <!-- @wc-context: storage state -->missing
                {:else}
                  <!-- @wc-context: storage state -->no dump
                {/if}
              </span>
              <span class="size">{entry.size === null ? '—' : fmtSize(entry.size)}</span>
            </button>
          {/each}
          {#if entries.length < total}
            <button class="load-more" onclick={loadMore}>
              load more ({entries.length.toLocaleString()} / {total.toLocaleString()})
            </button>
          {/if}
        {/if}
      </div>

      {#if selected !== null && detail !== null}
        <EntryDrawer {detail} onclose={close} />
      {/if}
    </div>
  {/if}
</main>

<style>
  main {
    flex: 1;
    display: flex;
    flex-direction: column;
    min-height: 0;
  }

  .undesigned {
    font: 400 12.5px var(--font-data);
    color: var(--faint);
    padding: 26px 28px;
  }

  .sys-head {
    padding: 20px 28px 0;
  }

  .row1 {
    display: flex;
    align-items: center;
    gap: 12px;
  }

  .accent {
    width: 5px;
    height: 34px;
    border-radius: var(--r-fill);
    flex: none;
  }

  h2 {
    margin: 0;
    font: 800 22px var(--font-display);
    letter-spacing: -0.03em;
  }

  .sub {
    font: 400 12.5px var(--font-data);
    color: var(--faint);
  }

  .future {
    cursor: not-allowed;
    opacity: 0.7;
  }

  .missing-list {
    all: unset;
    margin-left: auto;
    border: 2px solid var(--ink);
    border-radius: var(--r-pill);
    padding: 4px 14px;
    background: var(--panel);
    font: 600 12px var(--font-data);
    cursor: pointer;
  }

  .missing-list:disabled {
    color: var(--faint);
    cursor: progress;
  }

  .row2 {
    display: flex;
    align-items: center;
    gap: 16px;
    margin: 14px 0 16px;
  }

  .pct {
    font: 800 20px var(--font-display);
    color: var(--okT);
  }

  .bar-wrap {
    flex: 1;
  }

  .counts {
    font: 500 12.5px var(--font-data);
    color: var(--mut);
    white-space: nowrap;
  }

  .c-ok {
    color: var(--okT);
  }

  .c-warn {
    color: var(--warnT);
  }

  .c-bad {
    color: var(--bad);
  }

  .c-faint {
    color: var(--faint);
  }

  .table {
    flex: 1;
    display: flex;
    min-height: 0;
    border-top: 1px solid var(--hair);
    background: var(--panel);
  }

  .rail {
    width: 180px;
    flex: none;
    border-right: 1px solid var(--rule);
    padding: 14px 0 20px;
    font: 500 12.5px var(--font-data);
    color: var(--mut);
    display: flex;
    flex-direction: column;
  }

  .rail-item {
    all: unset;
    display: flex;
    justify-content: space-between;
    padding: 5px 20px;
    cursor: pointer;
    box-sizing: border-box;
  }

  .rail-item.nodump:not(.sel) {
    color: var(--faint);
  }

  .rail-item.sel {
    background: var(--panel2);
    box-shadow: inset 3px 0 0 var(--ink);
    font-weight: 600;
    color: var(--text);
  }

  .rail-divider {
    border-top: 1px dashed var(--hair);
    margin: 10px 20px;
  }

  .rail-search {
    padding: 0 20px;
  }

  .rail-search input {
    width: 100%;
    box-sizing: border-box;
    font: 400 12px var(--font-data);
    padding: 5px 8px;
    border: 1.5px solid var(--hair);
    border-radius: var(--r-input);
    background: var(--bg);
    color: var(--text);
  }

  .rail-density {
    margin: auto 20px 0;
    padding-top: 14px;
    display: flex;
    border: 1.5px solid var(--hair);
    border-radius: var(--r-pill);
    overflow: hidden;
    align-self: flex-start;
    margin-left: 20px;
  }

  .density-seg {
    all: unset;
    padding: 2px 8px;
    font: 500 10.5px var(--font-data);
    color: var(--faint);
    cursor: pointer;
  }

  .density-seg.active {
    background: var(--ink);
    color: var(--bg);
    font-weight: 600;
  }

  .rows {
    flex: 1;
    overflow-y: auto;
    min-width: 0;
  }

  .row {
    all: unset;
    display: flex;
    align-items: center;
    gap: 12px;
    padding: var(--rowpad);
    border-bottom: 1px solid var(--rule);
    cursor: pointer;
    width: 100%;
    box-sizing: border-box;
  }

  .row:hover {
    background: var(--hover-row);
  }

  .row.sel {
    background: color-mix(in srgb, var(--state-color) 12%, var(--panel));
    box-shadow: inset 3px 0 0 var(--state-color);
  }

  .row-name {
    flex: 1;
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    font-size: 13.5px;
  }

  .row-name.bold {
    font-weight: 600;
  }

  .name--missing {
    color: var(--bad);
  }

  .name--nodump {
    color: var(--faint);
  }

  .state-word {
    font: 400 11.5px var(--font-data);
  }

  .size {
    font: 400 11.5px var(--font-data);
    color: var(--dim);
    width: 46px;
    text-align: right;
    flex: none;
  }

  .empty {
    font: 400 12.5px var(--font-data);
    color: var(--faint);
    padding: 28px 20px;
    margin: 0;
  }

  .load-more {
    all: unset;
    display: block;
    width: 100%;
    box-sizing: border-box;
    padding: 10px 20px;
    text-align: center;
    font: 500 12px var(--font-data);
    color: var(--faint);
    cursor: pointer;
  }

  .load-more:hover {
    color: var(--text);
  }
</style>
