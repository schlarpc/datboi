<script lang="ts">
  /**
   * Blob inspector (/storage/blob/{hash}) — the storage screen's
   * drill-down: "what IS this blob, specifically?" Identity header
   * with the full hash, digests, source-path provenance, the one-hop
   * recipe DAG (every neighbor hash is a link into its own inspector,
   * so the graph is walkable), the dat claims the blob satisfies, and
   * the views pinning it (D33). Owner-only server-side like the rest
   * of the storage surface; the friend shell never routes here.
   */
  import { blobDetail } from '../lib/api/client';
  import type { BlobDetail, RouteEdge } from '../lib/api/types';
  import { copyText } from '../lib/clipboard';
  import Link from '../lib/components/Link.svelte';
  import { fmtDate, fmtSize, shortHash } from '../lib/format';
  import { residencyLabel } from '../lib/residency.svelte';
  import { loading, settle, type Remote } from '../lib/remote';

  let { hash }: { hash: string } = $props();

  let detail = $state<Remote<BlobDetail>>(loading());
  let copied = $state<'idle' | 'done' | 'failed'>('idle');

  // The recipe DAG links land here with a NEW hash on the same mounted
  // screen: reset to loading and generation-guard both arms so a slow
  // answer for the old hash can't land on the new one.
  let generation = 0;
  $effect(() => {
    const gen = ++generation;
    detail = loading();
    settle(
      blobDetail(hash),
      (value) => (detail = value),
      () => gen === generation,
    );
  });

  /** The Views.svelte copy affordance, single target: the full hash.
   * On a clipboard-less origin (LAN http) the button says so — the
   * full hash is the heading right beside it for a manual copy. */
  async function copyHash() {
    if (detail.st !== 'ready') return;
    copied = (await copyText(detail.data.hash)) ? 'done' : 'failed';
    setTimeout(() => (copied = 'idle'), 1400);
  }

  /** Declared digest rows, in strength order (absent algos omitted —
   * the wire shape). */
  const digestRows = $derived(
    detail.st !== 'ready'
      ? []
      : (
          [
            ['blake3', detail.data.digests.blake3],
            ['sha256', detail.data.digests.sha256],
            ['sha1', detail.data.digests.sha1],
            ['md5', detail.data.digests.md5],
            ['crc32', detail.data.digests.crc32],
          ] as [string, string | null | undefined][]
        ).filter((row): row is [string, string] => typeof row[1] === 'string'),
  );
</script>

{#snippet refList(refs: RouteEdge['inputs'])}
  {#each refs as ref, i (i)}
    <div class="ref">
      <Link class="ref-hash" href={`/storage/blob/${ref.hash}`}>{shortHash(ref.hash)}</Link>
      {#if ref.name !== null}
        <span class="ref-name">{ref.name}</span>
      {/if}
      <span class="ref-size">{ref.size === null ? '' : fmtSize(ref.size)}</span>
    </div>
  {/each}
{/snippet}

{#snippet edgeList(edges: RouteEdge[])}
  {#each edges as edge, i (i)}
    <div class="edge">
      <div class="edge-head">
        <span class="op">{edge.op}</span>
        <span class="verify">{edge.verify}</span>
      </div>
      <div class="edge-cols">
        <div class="refs">
          <span class="refs-label">inputs</span>
          {@render refList(edge.inputs)}
        </div>
        <div class="refs">
          <span class="refs-label">outputs</span>
          {@render refList(edge.outputs)}
        </div>
      </div>
    </div>
  {/each}
{/snippet}

<main>
  <div class="crumbs">
    <Link class="back" href="/storage">← storage</Link>
  </div>

  {#if detail.st === 'error'}
    <!-- Undesigned loading/error states: plain mono in --faint. -->
    <p class="undesigned">something went wrong — {detail.msg}</p>
  {:else if detail.st === 'loading'}
    <p class="undesigned">loading…</p>
  {:else}
    {@const d = detail.data}
    <div class="head">
      <h2 class="hash">{d.hash}</h2>
      <button class="pill" onclick={copyHash}>
        {#if copied === 'done'}copied ✓{:else if copied === 'failed'}couldn't copy{:else}⎘ copy{/if}
      </button>
    </div>
    <div class="badges">
      <span class="badge">{d.namespace}</span>
      <span class="badge">{residencyLabel(d.residency)}</span>
      <span class="badge">{d.size === null ? '—' : fmtSize(d.size)}</span>
      {#if d.verified_at !== null}
        <span class="badge ok">verified {fmtDate(d.verified_at)}</span>
      {:else}
        <span class="badge dim">never verified</span>
      {/if}
    </div>

    <div class="cards">
      <section class="card">
        <div class="card-title">digests</div>
        <div class="kv">
          {#each digestRows as [algo, hex] (algo)}
            <span class="k">{algo}</span>
            <span class="v">{hex}</span>
          {/each}
        </div>
      </section>

      <section class="card">
        <div class="card-title">provenance</div>
        {#if d.provenance.length === 0}
          <p class="none">no recorded source paths</p>
        {:else}
          {#each d.provenance as row (row.path)}
            <div class="prov-row">
              <span class="prov-path">{row.path}</span>
              <span class="prov-when">
                {row.ingested_at === null ? '' : fmtDate(row.ingested_at)}
              </span>
            </div>
          {/each}
        {/if}
      </section>

      <section class="card">
        <div class="card-title">routes in — ways to make it</div>
        {#if d.routes_in.length === 0}
          <p class="none">no recipe produces this blob — a literal</p>
        {:else}
          {@render edgeList(d.routes_in)}
        {/if}
      </section>

      <section class="card">
        <div class="card-title">routes out — things made from it</div>
        {#if d.routes_out.length === 0}
          <p class="none">no recipe consumes this blob</p>
        {:else}
          {@render edgeList(d.routes_out)}
        {/if}
      </section>

      <section class="card">
        <div class="card-title">claims · {d.claims_total.toLocaleString()}</div>
        {#if d.claims.length === 0}
          <p class="none">satisfies no dat claims</p>
        {:else}
          {#each d.claims as claim (claim.source + claim.entry)}
            <div class="claim-row">
              <span class="claim-entry">{claim.entry}</span>
              <span class="claim-source">{claim.source}</span>
            </div>
          {/each}
          {#if d.claims_total > d.claims.length}
            <p class="none">
              …and {(d.claims_total - d.claims.length).toLocaleString()} more
            </p>
          {/if}
        {/if}
      </section>

      <section class="card">
        <div class="card-title">pinned by</div>
        {#if d.pins.length === 0}
          <p class="none">no view pins this blob</p>
        {:else}
          <div class="chips">
            {#each d.pins as pin (pin)}
              <span class="chip">{pin}</span>
            {/each}
          </div>
        {/if}
      </section>
    </div>
  {/if}
</main>

<style>
  main {
    flex: 1;
    overflow-y: auto;
    padding: 20px var(--pad-x) 30px;
  }

  .crumbs {
    margin-bottom: 14px;
    font: 500 12.5px var(--font-data);
  }

  .crumbs :global(a.back) {
    color: var(--mut);
    text-decoration: none;
  }

  .crumbs :global(a.back:hover) {
    color: var(--text);
  }

  .undesigned {
    font: 400 12.5px var(--font-data);
    color: var(--faint);
  }

  .head {
    display: flex;
    align-items: center;
    gap: 14px;
    min-width: 0;
  }

  .hash {
    margin: 0;
    font: 600 15px var(--font-data);
    letter-spacing: 0.01em;
    overflow-wrap: anywhere;
    min-width: 0;
  }

  .pill {
    all: unset;
    flex: none;
    border: 2px solid var(--ink);
    border-radius: var(--r-pill);
    padding: 3px 12px;
    background: var(--panel);
    font: 600 12px var(--font-data);
    cursor: pointer;
  }

  .badges {
    display: flex;
    gap: 8px;
    margin: 10px 0 18px;
    flex-wrap: wrap;
  }

  .badge {
    font: 500 11.5px var(--font-data);
    border: 1.5px solid var(--hair);
    border-radius: var(--r-pill);
    padding: 2px 10px;
    color: var(--mut);
  }

  .badge.ok {
    color: var(--okT);
    border-color: var(--ok);
  }

  .badge.dim {
    color: var(--faint);
    border-style: dashed;
  }

  .cards {
    display: grid;
    grid-template-columns: repeat(2, 1fr);
    gap: 16px;
    align-items: start;
  }

  .card {
    background: var(--panel);
    border: 1px solid var(--hair);
    border-radius: var(--r-sub);
    padding: 14px 18px 16px;
  }

  .card-title {
    font: 800 13px var(--font-display);
    letter-spacing: 0.02em;
    margin-bottom: 8px;
  }

  .none {
    margin: 0;
    font: 400 12px var(--font-data);
    color: var(--faint);
  }

  .kv {
    display: grid;
    grid-template-columns: auto 1fr;
    column-gap: 14px;
    row-gap: 3px;
    font: 400 12px var(--font-data);
  }

  .k {
    color: var(--faint);
  }

  .v {
    color: var(--text);
    overflow-wrap: anywhere;
  }

  .prov-row {
    display: flex;
    gap: 12px;
    align-items: baseline;
    font: 400 12px var(--font-data);
    line-height: 1.8;
  }

  .prov-path {
    flex: 1;
    min-width: 0;
    overflow-wrap: anywhere;
    color: var(--text);
  }

  .prov-when {
    color: var(--faint);
  }

  .edge {
    border: 1px dashed var(--hair);
    border-radius: var(--r-sub);
    padding: 8px 12px 10px;
    margin-bottom: 8px;
  }

  .edge-head {
    display: flex;
    gap: 10px;
    align-items: baseline;
    margin-bottom: 6px;
  }

  .op {
    font: 600 12.5px var(--font-data);
  }

  .verify {
    font: 400 11px var(--font-data);
    color: var(--faint);
  }

  .edge-cols {
    display: grid;
    grid-template-columns: 1fr 1fr;
    gap: 14px;
  }

  .refs {
    display: flex;
    flex-direction: column;
    gap: 2px;
    min-width: 0;
  }

  .refs-label {
    font: 600 10.5px var(--font-data);
    color: var(--faint);
    letter-spacing: 0.04em;
  }

  .ref {
    display: flex;
    gap: 8px;
    align-items: baseline;
    font: 400 12px var(--font-data);
    min-width: 0;
  }

  .ref :global(a.ref-hash) {
    color: var(--text);
    text-decoration: underline;
    text-decoration-color: var(--dim);
    text-underline-offset: 2px;
  }

  .ref-name {
    flex: 1;
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    color: var(--mut);
  }

  .ref-size {
    margin-left: auto;
    flex: none;
    color: var(--dim);
  }

  .claim-row {
    display: flex;
    gap: 12px;
    align-items: baseline;
    font: 400 12.5px var(--font-data);
    line-height: 1.8;
  }

  .claim-entry {
    flex: 1;
    min-width: 0;
    overflow-wrap: anywhere;
  }

  .claim-source {
    color: var(--faint);
  }

  .chips {
    display: flex;
    gap: 8px;
    flex-wrap: wrap;
  }

  .chip {
    font: 500 12px var(--font-data);
    background: color-mix(in srgb, var(--ok) 10%, var(--panel));
    border: 1.5px solid var(--ink);
    border-radius: var(--r-pill);
    padding: 2px 10px;
  }

  @media (max-width: 640px) {
    /* Provenance/recipe/claims cards go single-file, and each recipe
       edge stacks its inputs over its outputs rather than side by side. */
    .cards {
      grid-template-columns: 1fr;
    }

    .edge-cols {
      grid-template-columns: 1fr;
    }
  }
</style>
