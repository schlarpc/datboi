<script lang="ts">
  /**
   * Storage subpage (spec §3.7 — wireframe 2b, restyled with tokens).
   * The stat tiles are REAL from /v1/storage; the savings % is computed
   * client-side (the API ships raw byte counts only).
   *
   * Action cards:
   * - Scrub: last-run line reads the D74 job ledger (CLI runs stamp
   *   terminal rows); the CLI hint stays — scrub itself is CLI-only.
   * - Quarantine: REAL count + reason lines. The `review →` flow was
   *   never designed (open-questions § "Quarantine review screen") —
   *   the inline items list IS the review for M5.
   * - Eviction: automatic at the watermark since D72 (reversible by
   *   construction); the card tunes rather than plans.
   * - Orphans: the D73 review gate — list, keep, two-click apply.
   */
  import {
    gcApply,
    gcKeep,
    gcOrphans,
    storage as fetchStorage,
    storageBreakdown,
  } from '../lib/api/client';
  import type { OrphansBody, StorageBody, StorageBreakdownBody } from '../lib/api/types';
  import CliHint from '../lib/components/CliHint.svelte';
  import Link from '../lib/components/Link.svelte';
  import { fmtDate, fmtSize, shortHash } from '../lib/format';
  import { residencyLabel } from '../lib/residency.svelte';
  import { errorText, loading, settle, type Remote } from '../lib/remote';

  // Three independent resources, three Remotes: a failed orphan refresh
  // (say, after a keep toggle) must never blank fully-rendered stats.
  let stats = $state<Remote<StorageBody>>(loading());
  let breakdown = $state<Remote<StorageBreakdownBody>>(loading());
  let orphans = $state<Remote<OrphansBody>>(loading());
  let scrubHint = $state(false);
  let evictHint = $state(false);
  /** Two-click delete: first click arms, second applies (D73's human
   * gate deserves more than one tap, less than a modal). */
  let applyArmed = $state(false);
  let applying = $state(false);
  /** A failed apply keeps the reviewed list; this line says why. */
  let applyError = $state<string | null>(null);

  const refreshOrphans = () => {
    settle(gcOrphans(), (value) => (orphans = value));
  };

  const toggleKeep = (hash: string, keep: boolean) => {
    gcKeep(hash, keep).then(refreshOrphans, refreshOrphans);
  };

  const applyAll = () => {
    if (!applyArmed) {
      applyArmed = true;
      return;
    }
    applying = true;
    applyError = null;
    gcApply().then(
      () => {
        applying = false;
        applyArmed = false;
        refreshOrphans();
      },
      (e: unknown) => {
        applying = false;
        applyArmed = false;
        applyError = errorText(e);
      },
    );
  };

  $effect(() => {
    settle(fetchStorage(), (value) => (stats = value));
  });

  $effect(() => {
    settle(storageBreakdown(), (value) => (breakdown = value));
  });

  $effect(refreshOrphans);

  const reviewable = $derived(orphans.st !== 'ready' ? [] : orphans.data.orphans);

  /** `−68% via recipes`: how much smaller disk is than what it represents. */
  const savingsPct = $derived(
    stats.st !== 'ready' || stats.data.represented_bytes === 0
      ? null
      : Math.round(100 * (1 - stats.data.on_disk_bytes / stats.data.represented_bytes)),
  );

  /** Class bars are proportional to the LARGEST cell, not the sum —
   * meta/ is invisible next to data/ either way; against the max the
   * big cell reads full-scale. */
  const maxClassBytes = $derived(
    breakdown.st !== 'ready'
      ? 1
      : Math.max(1, ...breakdown.data.by_class.map((cell) => cell.bytes)),
  );
</script>

<main>
  <div class="title-row">
    <h2>Storage</h2>
    <span class="sub">what the store holds vs what it represents</span>
  </div>

  {#if stats.st === 'error'}
    <!-- Undesigned loading/error states: plain mono in --faint. -->
    <p class="undesigned">something went wrong — {stats.msg}</p>
  {:else if stats.st === 'loading'}
    <p class="undesigned">loading…</p>
  {:else}
    {@const s = stats.data}
    <div class="tiles">
      <div class="tile">
        <span class="label">BLOBS</span>
        <span class="num">{s.blob_count.toLocaleString()}</span>
      </div>
      <div class="tile">
        <span class="label">ON DISK</span>
        <span class="num">{fmtSize(s.on_disk_bytes)}</span>
      </div>
      <div class="tile brag">
        <span class="label">REPRESENTED</span>
        <span class="num">{fmtSize(s.represented_bytes)}</span>
        {#if savingsPct !== null && savingsPct > 0}
          <span class="tile-sub ok">−{savingsPct}% via recipes</span>
        {/if}
      </div>
      <div class="tile dashed">
        <span class="label">LITERAL-ONLY</span>
        <span class="num">{fmtSize(s.literal_only_bytes)}</span>
        <span class="tile-sub">shrinkable</span>
      </div>
    </div>

    <div class="cards">
      <div class="action-card">
        <div class="card-title">Scrub</div>
        {#if s.last_scrub !== null}
          <p class="copy">last: {fmtDate(s.last_scrub.finished_at)} · {s.last_scrub.name}</p>
        {:else}
          <p class="copy">no scrub recorded yet — runs land in the job ledger</p>
        {/if}
        <button class="pill" aria-expanded={scrubHint} onclick={() => (scrubHint = !scrubHint)}>run via CLI</button>
        {#if scrubHint}
          <CliHint command={'datboi scrub [--sample <pct>]'}>
            re-hash stored blobs and report corruption:
          </CliHint>
        {/if}
      </div>

      <div class="action-card" class:danger={s.quarantine.count > 0}>
        <div class="card-title">
          <!-- @wc-context: bad components, not people -->
          Quarantine · {s.quarantine.count.toLocaleString()}
        </div>
        {#if s.quarantine.count === 0}
          <p class="copy">nothing quarantined</p>
        {:else}
          <!-- The `review →` flow was never designed; this inline list
               IS the M5 review (open-questions § "Quarantine review
               screen"; M5 scope ruling, open-questions 2026-07-11). -->
          <div class="q-items">
            {#each s.quarantine.items as item (item.component)}
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
        <!-- D72: watermark eviction is automatic and REVERSIBLE by
             construction — every drop has a locally-replayed rebuild
             route. Tune or disarm via `datboi gc config`. -->
        <p class="copy">rebuildable literals evict automatically at the watermark — nothing unrecoverable is ever dropped</p>
        <button class="pill" aria-expanded={evictHint} onclick={() => (evictHint = !evictHint)}>tune via CLI</button>
        {#if evictHint}
          <CliHint command={'datboi gc config --high-water 90% --low-water 85%'}>
            watermarks ("off" disarms); manual pass: datboi evict --dry-run:
          </CliHint>
        {/if}
      </div>

      <div class="action-card" class:danger={reviewable.some((o) => !o.kept)}>
        <div class="card-title">
          <!-- @wc-context: unreferenced blobs, not people -->
          Orphans · {reviewable.length.toLocaleString()}
        </div>
        {#if orphans.st === 'loading'}
          <p class="copy">loading…</p>
        {:else if orphans.st === 'error'}
          <!-- Card-local failure: stats and breakdown stay rendered. -->
          <p class="copy">something went wrong — {orphans.msg}</p>
        {:else if reviewable.length === 0}
          <p class="copy">nothing unreferenced — every blob is rooted or still under grace</p>
        {:else}
          <!-- D73 review gate: deletion is THIS surface, never ambient.
               Each row shows ingest provenance; keep excludes it. -->
          <div class="q-items">
            {#each reviewable as orphan (orphan.hash)}
              <div class="q-item">
                <span class="q-hash">{shortHash(orphan.hash)}</span>
                <span class="q-reason">
                  {fmtSize(orphan.size)}{orphan.sources.length > 0
                    ? ` · ${orphan.sources.join(', ')}`
                    : ''}
                </span>
                <button class="pill" onclick={() => toggleKeep(orphan.hash, !orphan.kept)}>
                  {orphan.kept ? 'kept ✓' : 'keep'}
                </button>
              </div>
            {/each}
          </div>
          <p class="copy">
            {fmtSize(orphans.data.reclaimable_bytes)} reclaimable · every delete re-verifies
            unreferenced at delete time
          </p>
          <button class="pill" class:bad-pill={applyArmed} disabled={applying} onclick={applyAll}>
            {#if applying}deleting…{:else if applyArmed}confirm: delete non-kept{:else}delete non-kept…{/if}
          </button>
          {#if applyError !== null}
            <p class="copy apply-error">delete failed — {applyError}</p>
          {/if}
        {/if}
      </div>
    </div>

    <!-- Debug-grade introspection (open-questions 2026-07-11): the
         /v1/storage/breakdown aggregates, with the largest blobs
         linking into the /storage/blob/{hash} inspector. -->
    {#if breakdown.st === 'error'}
      <!-- Breakdown-only failure: the tiles and cards above stand. -->
      <p class="undesigned">something went wrong — {breakdown.msg}</p>
    {:else if breakdown.st === 'ready'}
      {@const b = breakdown.data}
      <div class="bytes-card">
        <div class="bytes-title">WHERE THE BYTES LIVE</div>

        <div class="classes">
          {#each b.by_class as cell (cell.namespace + cell.residency)}
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
              {#each b.by_source as row (row.source)}
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
              {#each b.largest as blob (blob.hash)}
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
    padding: 24px var(--pad-x) 30px;
  }

  .title-row {
    display: flex;
    align-items: baseline;
    gap: 14px;
    margin-bottom: 22px;
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

  .undesigned {
    font: 400 0.78125rem var(--font-data);
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
    font: 800 0.8125rem var(--font-display);
    letter-spacing: 0.02em;
    color: var(--mut);
  }

  .num {
    font: 800 1.25rem var(--font-display);
    letter-spacing: -0.02em;
  }

  .brag .num {
    color: var(--okT);
  }

  .tile-sub {
    font: 400 0.71875rem var(--font-data);
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
    font: 800 0.875rem var(--font-display);
    letter-spacing: 0.02em;
    margin-bottom: 8px;
  }

  .danger .card-title {
    color: var(--bad);
  }

  .copy {
    margin: 0 0 12px;
    font: 400 0.78125rem var(--font-data);
    color: var(--mut);
    line-height: 1.6;
  }

  .pill {
    all: unset;
    border: 2px solid var(--ink);
    border-radius: var(--r-pill);
    padding: 4px 14px;
    background: var(--panel);
    font: 600 0.75rem var(--font-data);
    cursor: pointer;
  }

  /* The armed state of the two-click delete: unmistakably destructive. */
  .pill.bad-pill {
    border-color: var(--bad);
    color: var(--bad);
  }

  .pill:disabled {
    cursor: default;
    color: var(--faint);
  }

  .apply-error {
    margin: 10px 0 0;
    color: var(--bad);
  }

  .q-items {
    font: 400 0.75rem var(--font-data);
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
    font: 800 0.875rem var(--font-display);
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
    font: 400 0.75rem var(--font-data);
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
    font: 800 0.78125rem var(--font-display);
    letter-spacing: 0.02em;
    color: var(--mut);
    margin-bottom: 6px;
  }

  .grid-table {
    display: grid;
    font: 400 0.75rem var(--font-data);
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
    font: 600 0.6875rem var(--font-data);
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

  /* Tablet: four stat tiles pair up, the three action cards and the
     by-source / largest-blobs split each go single column. */
  @media (max-width: 720px) {
    .title-row {
      flex-wrap: wrap;
      gap: 4px 14px;
    }

    .tiles {
      grid-template-columns: repeat(2, 1fr);
    }

    .cards {
      grid-template-columns: 1fr;
    }

    .split {
      grid-template-columns: 1fr;
      gap: 20px;
    }
  }

  @media (max-width: 640px) {
    /* The class row's three fixed columns (170 + 72 + 160px) blow past a
       phone width, so the label takes its own line and the track, bytes,
       and blob count share the line below it. */
    .class-row {
      flex-wrap: wrap;
      row-gap: 4px;
    }

    .class-label {
      flex-basis: 100%;
      width: auto;
    }

    .class-blobs {
      width: auto;
      margin-left: auto;
    }
  }
</style>
