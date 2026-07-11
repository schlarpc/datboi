<script lang="ts">
  /**
   * Library home — "The shelf" (spec §3.1, hi-fi 4c). System cards from
   * GET /v1/systems in the full cartridge register: band, 2px ink,
   * offset shadow, stacked bar with ink frame, views chips.
   */
  import { systems as fetchSystems } from '../lib/api/client';
  import type { System } from '../lib/api/types';
  import { bandFor } from '../lib/bands';
  import Link from '../lib/components/Link.svelte';
  import StackedBar from '../lib/components/StackedBar.svelte';
  import { router } from '../lib/router.svelte';
  import { completenessPct } from '../lib/state';

  let systems = $state<System[] | null>(null);
  let error = $state<string | null>(null);
  // Empty-card reveal: dat import is CLI-only in M5 (api.rs scope
  // ruling), so the dashed card can't open an upload flow — clicking it
  // reveals the CLI incantation instead. Not a modal, just a fold.
  let cliHintOpen = $state(false);

  $effect(() => {
    fetchSystems().then(
      (body) => (systems = body.systems),
      (e: unknown) => (error = e instanceof Error ? e.message : String(e)),
    );
  });

  const entryTotal = $derived((systems ?? []).reduce((sum, sys) => sum + sys.total, 0));

  /** Completeness color per the comps: green ≥90, amber below. */
  function pctClass(pct: number): string {
    return pct >= 90 ? 'pct-ok' : 'pct-warn';
  }
</script>

<main>
  <div class="title-row">
    <h2>The shelf</h2>
    {#if systems !== null}
      <span class="sub">
        {systems.length.toLocaleString()} systems · {entryTotal.toLocaleString()} entries
      </span>
    {/if}
  </div>

  {#if error !== null}
    <!-- Loading/error states have no design (spec has none) — plain
         mono text in --faint until they get one. -->
    <p class="undesigned">something went wrong — {error}</p>
  {:else if systems === null}
    <p class="undesigned">loading…</p>
  {:else}
    <div class="grid">
      {#each systems as sys (sys.id)}
        {@const pct = completenessPct(sys.counts)}
        <!-- Whole card opens the audit drill-down (spec §3.1); a div
             with role=link because a real <a> can't nest the view-chip
             links and <article> refuses the interactive role. -->
        <div
          class="card"
          onclick={() => router.navigate(`/library/${sys.id}`)}
          onkeydown={(e) => {
            // @wc-ignore
            if (e.key === 'Enter') router.navigate(`/library/${sys.id}`);
          }}
          role="link"
          tabindex="0"
        >
          <div class="band" style:background={bandFor(sys.system)}></div>
          <div class="body">
            <div class="head">
              <span class="name">{sys.system}</span>
              <span class="rev">
                <!-- Revision is data (provider label + dat version). -->
                {sys.provider}{#if sys.revision?.version}&nbsp;{sys.revision.version}{/if}
              </span>
              <span class="pct {pctClass(pct)}">{pct}%</span>
            </div>
            <StackedBar counts={sys.counts} register="shelf" />
            <div class="counts">
              <!-- @wc-context: storage state -->
              <span>{sys.counts.verified.toLocaleString()} verified</span>
              <!-- @wc-context: storage state -->
              <span>{sys.counts.claimed.toLocaleString()} claimed</span>
              <!-- @wc-context: storage state -->
              <span class="bad">{sys.counts.missing.toLocaleString()} missing</span>
            </div>
            {#if sys.views.length > 0}
              <div class="chips">
                {#each sys.views as view (view)}
                  <!-- Chips open the view — the Views screen for now;
                       per-view pages are a later task. -->
                  <Link
                    href="/views"
                    class="chip"
                    onclick={(e: MouseEvent) => e.stopPropagation()}
                  >
                    {view}
                  </Link>
                {/each}
              </div>
            {/if}
          </div>
        </div>
      {/each}
    </div>

    <button class="empty-card" onclick={() => (cliHintOpen = !cliHintOpen)}>
      <span>+ import a dat to start a new system</span>
      {#if cliHintOpen}
        <span class="cli-hint">
          <span>dat import is CLI-only for now — run:</span>
          <!-- @wc-ignore -->
          <code>datboi dat import &lt;file.dat&gt;</code>
        </span>
      {/if}
    </button>
  {/if}
</main>

<style>
  main {
    flex: 1;
    overflow-y: auto;
    padding: 26px 28px 30px;
  }

  .title-row {
    display: flex;
    align-items: baseline;
    gap: 14px;
    margin-bottom: 22px;
  }

  h2 {
    margin: 0;
    font: 800 24px var(--font-display);
    letter-spacing: -0.03em;
  }

  .sub {
    font: 400 13px var(--font-data);
    color: var(--faint);
  }

  .undesigned {
    font: 400 12.5px var(--font-data);
    color: var(--faint);
  }

  .grid {
    display: grid;
    grid-template-columns: 1fr 1fr;
    gap: 18px;
  }

  .card {
    background: var(--panel);
    border: 2px solid var(--ink);
    border-radius: var(--r-card);
    overflow: hidden;
    box-shadow: var(--shadow-card);
    cursor: pointer;
  }

  .band {
    height: 12px;
  }

  .body {
    padding: 18px 22px 20px;
  }

  .head {
    display: flex;
    align-items: baseline;
    gap: 10px;
    margin-bottom: 14px;
  }

  .name {
    font: 800 17px var(--font-display);
    letter-spacing: -0.02em;
  }

  .rev {
    font: 400 12px var(--font-data);
    color: var(--faint);
  }

  .pct {
    margin-left: auto;
    font: 800 18px var(--font-display);
  }

  .pct-ok {
    color: var(--okT);
  }

  .pct-warn {
    color: var(--warnT);
  }

  .counts {
    display: flex;
    gap: 14px;
    margin-top: 10px;
    font: 500 12px var(--font-data);
    color: var(--mut);
  }

  .counts .bad {
    color: var(--bad);
  }

  .chips {
    display: flex;
    flex-wrap: wrap;
    gap: 8px;
    margin-top: 14px;
    padding-top: 12px;
    border-top: 1px solid var(--rule);
  }

  .chips :global(a.chip) {
    font: 500 12px var(--font-data);
    background: color-mix(in srgb, var(--ok) 10%, var(--panel));
    border: 1.5px solid var(--ink);
    border-radius: var(--r-pill);
    padding: 2px 10px;
    color: var(--text);
    text-decoration: none;
  }

  .empty-card {
    all: unset;
    display: block;
    box-sizing: border-box;
    width: 100%;
    margin-top: 18px;
    border: 2px dashed var(--dim);
    border-radius: var(--r-card);
    padding: 16px;
    text-align: center;
    color: var(--faint);
    font: 600 13px var(--font-display);
    cursor: pointer;
  }

  .cli-hint {
    display: block;
    margin-top: 8px;
    font: 400 11.5px var(--font-data);
    color: var(--mut);
  }

  .cli-hint code {
    font-family: var(--font-data);
    color: var(--text);
  }
</style>
