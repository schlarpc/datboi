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
  import { storage as fetchStorage } from '../lib/api/client';
  import type { StorageBody } from '../lib/api/types';
  import CliHint from '../lib/components/CliHint.svelte';
  import { fmtDate, fmtSize, shortHash } from '../lib/format';

  let stats = $state<StorageBody | null>(null);
  let error = $state<string | null>(null);
  let scrubHint = $state(false);
  let evictHint = $state(false);

  $effect(() => {
    fetchStorage().then(
      (body) => (stats = body),
      (e: unknown) => (error = e instanceof Error ? e.message : String(e)),
    );
  });

  /** `−68% via recipes`: how much smaller disk is than what it represents. */
  const savingsPct = $derived(
    stats === null || stats.represented_bytes === 0
      ? null
      : Math.round(100 * (1 - stats.on_disk_bytes / stats.represented_bytes)),
  );
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
</style>
