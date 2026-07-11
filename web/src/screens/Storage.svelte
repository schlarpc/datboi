<script lang="ts">
  /**
   * Storage subpage (spec §3.7 — wireframe 2b, restyled with tokens).
   * The stat tiles are REAL from /v1/storage; the savings % is computed
   * client-side (the API ships raw byte counts only).
   *
   * Action cards (M5 scope ruling, open-questions 2026-07-11 —
   * mutating pipeline actions are CLI-only):
   * - Scrub: no scrub-run ledger exists (open-questions § "Scrub runs
   *   and verify methods aren't recorded"), so the card says so
   *   honestly and hints `datboi scrub` instead of faking `last: 2d`.
   * - Quarantine: REAL count + reason lines. The `review →` flow was
   *   never designed (open-questions § "Quarantine review screen") —
   *   the inline items list IS the review for M5.
   * - Eviction: the planner screen (§3.8) is not built — there is no
   *   plan API; the dry-run CLI is the only entry point, and the
   *   design's promise copy stays.
   */
  import { storage as fetchStorage, storageBreakdown } from '../lib/api/client';
  import type { StorageBody, StorageBreakdownBody } from '../lib/api/types';
  import CliHint from '../lib/components/CliHint.svelte';
  import Link from '../lib/components/Link.svelte';
  import { fmtDate, fmtSize, shortHash } from '../lib/format';

  let stats = $state<StorageBody | null>(null);
  let breakdown = $state<StorageBreakdownBody | null>(null);
  let error = $state<string | null>(null);
  let scrubHint = $state(false);
  let evictHint = $state(false);

  $effect(() => {
    fetchStorage().then(
      (body) => (stats = body),
      (e: unknown) => (error = e instanceof Error ? e.message : String(e)),
    );
  });

  $effect(() => {
    storageBreakdown().then(
      (body) => (breakdown = body),
      (e: unknown) => (error = e instanceof Error ? e.message : String(e)),
    );
  });

  /** `−68% via recipes`: how much smaller disk is than what it represents. */
  const savingsPct = $derived(
    stats === null || stats.represented_bytes === 0
      ? null
      : Math.round(100 * (1 - stats.on_disk_bytes / stats.represented_bytes)),
  );

  /** Class bars are proportional to the LARGEST cell, not the sum —
   * meta/ is invisible next to data/ either way; against the max the
   * big cell reads full-scale. */
  const maxClassBytes = $derived(
    breakdown === null ? 1 : Math.max(1, ...breakdown.by_class.map((cell) => cell.bytes)),
  );

  /** `evicted_covered` → `evicted covered` — residency is data (mono),
   * but the underscore is a wire artifact, not display. */
  const residencyLabel = (residency: string) => residency.replace('_', ' ');
</script>

<main>
  <div class="title-row">
    <h2>Storage</h2>
    <span class="sub">what the store holds vs what it represents</span>
  </div>

  {#if error !== null}
    <!-- Undesigned loading/error states: plain mono in --faint. -->
    <p class="undesigned">something went wrong — {error}</p>
  {:else if stats === null}
    <p class="undesigned">loading…</p>
  {:else}
    <div class="tiles">
      <div class="tile">
        <span class="label">BLOBS</span>
        <span class="num">{stats.blob_count.toLocaleString()}</span>
      </div>
      <div class="tile">
        <span class="label">ON DISK</span>
        <span class="num">{fmtSize(stats.on_disk_bytes)}</span>
      </div>
      <div class="tile brag">
        <span class="label">REPRESENTED</span>
        <span class="num">{fmtSize(stats.represented_bytes)}</span>
        {#if savingsPct !== null && savingsPct > 0}
          <span class="tile-sub ok">−{savingsPct}% via recipes</span>
        {/if}
      </div>
      <div class="tile dashed">
        <span class="label">LITERAL-ONLY</span>
        <span class="num">{fmtSize(stats.literal_only_bytes)}</span>
        <span class="tile-sub">shrinkable</span>
      </div>
    </div>

    <div class="cards">
      <div class="action-card">
        <div class="card-title">Scrub</div>
        <p class="copy">no scrub ledger yet — the index records per-blob verify dates, not runs</p>
        <button class="pill" onclick={() => (scrubHint = !scrubHint)}>run via CLI</button>
        {#if scrubHint}
          <CliHint command={'datboi scrub [--sample <pct>]'}>
            re-hash stored blobs and report corruption:
          </CliHint>
        {/if}
      </div>

      <div class="action-card" class:danger={stats.quarantine.count > 0}>
        <div class="card-title">
          <!-- @wc-context: bad components, not people -->
          Quarantine · {stats.quarantine.count.toLocaleString()}
        </div>
        {#if stats.quarantine.count === 0}
          <p class="copy">nothing quarantined</p>
        {:else}
          <!-- The `review →` flow was never designed; this inline list
               IS the M5 review (open-questions § "Quarantine review
               screen"; M5 scope ruling, open-questions 2026-07-11). -->
          <div class="q-items">
            {#each stats.quarantine.items as item (item.component)}
              <div class="q-item">
                <span class="q-hash">{shortHash(item.component)}</span>
                <span class="q-reason">{item.reason}</span>
                <span class="q-when">{fmtDate(item.quarantined_at)}</span>
              </div>
            {/each}
          </div>
        {/if}
      </div>

      <div class="action-card">
        <div class="card-title"><!-- @wc-context: storage eviction -->Eviction</div>
        <p class="copy bad">nothing is deleted without a plan you approve</p>
        <!-- The eviction planner (§3.8) is not built: no plan API — the
             dry-run CLI is the only entry (M5 scope ruling,
             open-questions 2026-07-11). -->
        <button class="pill" onclick={() => (evictHint = !evictHint)}>plan (dry-run) via CLI</button>
        {#if evictHint}
          <CliHint command={'datboi evict --target-bytes <n> --dry-run'}>
            plan first; the same command without --dry-run executes:
          </CliHint>
        {/if}
      </div>
    </div>

    <!-- Debug-grade introspection (open-questions 2026-07-11): the
         /v1/storage/breakdown aggregates, with the largest blobs
         linking into the /storage/blob/{hash} inspector. -->
    {#if breakdown !== null}
      <div class="bytes-card">
        <div class="bytes-title">WHERE THE BYTES LIVE</div>

        <div class="classes">
          {#each breakdown.by_class as cell (cell.namespace + cell.residency)}
            <div class="class-row">
              <span class="class-label">{cell.namespace} · {residencyLabel(cell.residency)}</span>
              <span class="track">
                <span
                  class="fill"
                  style:width="{Math.max(1, Math.round((100 * cell.bytes) / maxClassBytes))}%"
                ></span>
              </span>
              <span class="class-bytes">{fmtSize(cell.bytes)}</span>
              <span class="class-blobs">
                {cell.blobs.toLocaleString()} blobs{#if cell.sizeless > 0}
                  · {cell.sizeless.toLocaleString()} sizeless{/if}
              </span>
            </div>
          {/each}
        </div>

        <div class="split">
          <div class="col">
            <div class="col-title">by source</div>
            <div class="grid-table sources">
              <span class="th">source</span>
              <span class="th num">blobs</span>
              <span class="th num">bytes</span>
              {#each breakdown.by_source as row (row.source)}
                <span class="td data">{row.source}</span>
                <span class="td num">{row.blobs.toLocaleString()}</span>
                <span class="td num">{fmtSize(row.bytes)}</span>
              {/each}
            </div>
          </div>
          <div class="col">
            <div class="col-title">largest blobs</div>
            <div class="grid-table largest">
              <span class="th">hash</span>
              <span class="th num">size</span>
              <span class="th">residency</span>
              {#each breakdown.largest as blob (blob.hash)}
                <Link class="td data blob-link" href={`/storage/blob/${blob.hash}`}>
                  {shortHash(blob.hash)}
                </Link>
                <span class="td num">{blob.size === null ? '—' : fmtSize(blob.size)}</span>
                <span class="td data">{residencyLabel(blob.residency)}</span>
              {/each}
            </div>
          </div>
        </div>
      </div>
    {/if}
  {/if}
</main>

<style>
  main {
    flex: 1;
    overflow-y: auto;
    padding: 24px 28px 30px;
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

  .tiles {
    display: grid;
    grid-template-columns: repeat(4, 1fr);
    gap: 16px;
  }

  .tile {
    background: var(--panel);
    border: 2px solid var(--ink);
    border-radius: var(--r-card);
    box-shadow: var(--shadow-card);
    padding: 14px 18px 16px;
    display: flex;
    flex-direction: column;
    gap: 6px;
  }

  .tile.dashed {
    border-style: dashed;
    border-color: var(--dim);
    box-shadow: none;
  }

  .label {
    font: 800 13px var(--font-display);
    letter-spacing: 0.02em;
    color: var(--mut);
  }

  .num {
    font: 800 20px var(--font-display);
    letter-spacing: -0.02em;
  }

  .brag .num {
    color: var(--okT);
  }

  .tile-sub {
    font: 400 11.5px var(--font-data);
    color: var(--faint);
  }

  .tile-sub.ok {
    color: var(--okT);
  }

  .cards {
    display: grid;
    grid-template-columns: repeat(3, 1fr);
    gap: 16px;
    margin-top: 18px;
    align-items: start;
  }

  .action-card {
    background: var(--panel);
    border: 1px solid var(--hair);
    border-radius: var(--r-sub);
    padding: 18px 20px;
  }

  .action-card.danger {
    border: 1.5px solid var(--bad);
    background: color-mix(in srgb, var(--bad) 8%, var(--panel));
  }

  .card-title {
    font: 800 14px var(--font-display);
    letter-spacing: 0.02em;
    margin-bottom: 8px;
  }

  .danger .card-title {
    color: var(--bad);
  }

  .copy {
    margin: 0 0 12px;
    font: 400 12.5px var(--font-data);
    color: var(--mut);
    line-height: 1.6;
  }

  .copy.bad {
    color: var(--bad);
  }

  .pill {
    all: unset;
    border: 2px solid var(--ink);
    border-radius: var(--r-pill);
    padding: 4px 14px;
    background: var(--panel);
    font: 600 12px var(--font-data);
    cursor: pointer;
  }

  .q-items {
    font: 400 12px var(--font-data);
    color: var(--mut);
    line-height: 1.9;
  }

  .q-item {
    display: flex;
    gap: 10px;
    align-items: baseline;
  }

  .q-hash {
    color: var(--text);
  }

  .q-reason {
    flex: 1;
    min-width: 0;
    overflow-wrap: anywhere;
  }

  .q-when {
    color: var(--faint);
  }

  .bytes-card {
    margin-top: 18px;
    background: var(--panel);
    border: 2px solid var(--ink);
    border-radius: var(--r-card);
    box-shadow: var(--shadow-card);
    padding: 16px 20px 20px;
  }

  .bytes-title {
    font: 800 14px var(--font-display);
    letter-spacing: 0.02em;
    margin-bottom: 12px;
  }

  .classes {
    display: flex;
    flex-direction: column;
    gap: 6px;
    margin-bottom: 16px;
  }

  .class-row {
    display: flex;
    align-items: center;
    gap: 12px;
    font: 400 12px var(--font-data);
  }

  .class-label {
    width: 170px;
    flex: none;
    color: var(--text);
  }

  /* The JobRow track/fill register — a width-percent fill is enough
     here (StackedBar is the shelf's multi-state register). */
  .track {
    flex: 1;
    height: 8px;
    background: var(--panel2);
    border: 1px solid var(--hair);
    border-radius: var(--r-fill);
    overflow: hidden;
  }

  .fill {
    display: block;
    height: 100%;
    background: var(--ink);
    border-radius: var(--r-fill);
  }

  .class-bytes {
    width: 72px;
    flex: none;
    text-align: right;
    color: var(--text);
  }

  .class-blobs {
    width: 160px;
    flex: none;
    text-align: right;
    color: var(--faint);
  }

  .split {
    display: grid;
    grid-template-columns: 1fr 1fr;
    gap: 24px;
    align-items: start;
  }

  .col-title {
    font: 800 12.5px var(--font-display);
    letter-spacing: 0.02em;
    color: var(--mut);
    margin-bottom: 6px;
  }

  .grid-table {
    display: grid;
    font: 400 12px var(--font-data);
    column-gap: 14px;
    row-gap: 2px;
  }

  .grid-table.sources {
    grid-template-columns: 1fr auto auto;
  }

  .grid-table.largest {
    grid-template-columns: auto auto 1fr;
  }

  .th {
    font: 600 11px var(--font-data);
    color: var(--faint);
    border-bottom: 1px solid var(--rule);
    padding-bottom: 3px;
  }

  .td {
    color: var(--mut);
  }

  .td.data {
    color: var(--text);
  }

  .num {
    text-align: right;
  }

  .grid-table :global(a.blob-link) {
    color: var(--text);
    text-decoration: underline;
    text-decoration-color: var(--dim);
    text-underline-offset: 2px;
  }
</style>
