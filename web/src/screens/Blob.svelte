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
  import { blobDetail, blobVerify } from '../lib/api/client';
  import type { BlobDetail, RouteEdge } from '../lib/api/types';
  import { copyText } from '../lib/clipboard';
  import Link from '../lib/components/Link.svelte';
  import { fmtDate, fmtSize, shortHash } from '../lib/format';
  import { followJob, jobsSignal } from '../lib/jobs.svelte';
  import { residencyLabel } from '../lib/residency.svelte';
  import { errorText, loading, settle, type Remote } from '../lib/remote';
  import LoadError from '../lib/components/LoadError.svelte';

  let { hash }: { hash: string } = $props();

  let detail = $state<Remote<BlobDetail>>(loading());
  let copied = $state<'idle' | 'done' | 'failed'>('idle');
  /** Which digest row just copied — brief ✓ feedback per row. */
  let copiedDigest = $state<string | null>(null);
  /** Per-edge "…and N more" expansion, keyed `${direction}-${index}`. */
  let expanded = $state<Record<string, boolean>>({});

  /** Aggregates before enumerations (87-web-ui.md): a 74-chunk
   * assemble is "74 inputs" first, rows on demand. */
  const REF_CAP = 5;

  // The recipe DAG links land here with a NEW hash on the same mounted
  // screen: reset to loading and generation-guard both arms so a slow
  // answer for the old hash can't land on the new one.
  let generation = 0;
  /** Bumped by the error line's retry — the load effect re-runs. */
  let attempt = $state(0);
  $effect(() => {
    void attempt;
    const gen = ++generation;
    detail = loading();
    expanded = {};
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

  // Verify-now (D80): the "never verified" badge is the button — the
  // moment of doubt is when the user must be able to act in place.
  let verifying = $state(false);
  let verifyError = $state<string | null>(null);
  let destroyed = false;
  $effect(() => () => {
    destroyed = true;
  });

  async function verifyNow() {
    if (verifying || detail.st !== 'ready') return;
    verifying = true;
    verifyError = null;
    try {
      const { job } = await blobVerify(detail.data.hash);
      jobsSignal.bump(); // the header indicator lights immediately
      const done = await followJob(job, { alive: () => !destroyed });
      if (done === null) return; // unmounted mid-poll
      if (done.state === 'failed') {
        verifyError = done.error ?? 'verify failed';
      }
      attempt += 1; // refetch — verified_at (or the demotion) lands
    } catch (e) {
      verifyError = errorText(e);
    } finally {
      verifying = false;
    }
  }

  /** Digest rows copy on click — sha1/md5 are what gets pasted into
   * dat tools, and they're plain text (not nav links like ref hashes,
   * which stay links per 87-web-ui.md). */
  async function copyDigest(algo: string, hex: string) {
    if (await copyText(hex)) {
      copiedDigest = algo;
      setTimeout(() => (copiedDigest = null), 1400);
    }
  }

  // Lowercase attribute copy, forced at statement level.
  // @wc-include
  const copyDigestTitle = 'click to copy';

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

{#snippet refList(refs: RouteEdge['inputs'], key: string)}
  {@const showAll = expanded[key] === true || refs.length <= REF_CAP}
  {#each showAll ? refs : refs.slice(0, REF_CAP) as ref, i (i)}
    <div class="ref">
      {#if ref.hash === hash}
        <!-- A link to the page you're on isn't navigation — walking
             assemble inputs used to LOOK like a loop because only the
             headline hash changed (87-web-ui.md). -->
        <span class="ref-self">this blob</span>
      {:else}
        <Link class="ref-hash" href={`/storage/blob/${ref.hash}`}>{shortHash(ref.hash)}</Link>
      {/if}
      {#if ref.name !== null}
        <span class="ref-name">{ref.name}</span>
      {/if}
      <span class="ref-size">{ref.size === null ? '' : fmtSize(ref.size)}</span>
    </div>
  {/each}
  {#if !showAll}
    <button class="more" onclick={() => (expanded[key] = true)}>
      …and {(refs.length - REF_CAP).toLocaleString()} more
    </button>
  {/if}
{/snippet}

{#snippet edgeList(edges: RouteEdge[], dir: string)}
  {#each edges as edge, i (i)}
    <div class="edge">
      <div class="edge-head">
        <span class="op">{edge.op}</span>
        {#if edge.inputs.length > REF_CAP}
          <span class="edge-count">{edge.inputs.length.toLocaleString()} inputs</span>
        {/if}
        <span class="verify">{edge.verify}</span>
      </div>
      <div class="edge-cols">
        <div class="refs">
          <span class="refs-label">inputs</span>
          {@render refList(edge.inputs, `${dir}-${i}-in`)}
        </div>
        <div class="refs">
          <span class="refs-label">outputs</span>
          {@render refList(edge.outputs, `${dir}-${i}-out`)}
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
    <LoadError msg={detail.msg} onretry={() => (attempt += 1)} />
  {:else if detail.st === 'loading'}
    <p class="undesigned">loading…</p>
  {:else}
    {@const d = detail.data}
    <!-- The headline is the blob's MEANING, computed from the edges
         (D79): claim name, else relation to a claimed root, else a
         byte sniff. The hash demotes to metadata below — a collector
         reads "what is this", not 64 hex chars. -->
    {#if d.claims.length > 0}
      <h2 class="title">{d.claims[0].entry}</h2>
      <div class="title-sub">
        {d.claims[0].source}{#if d.claims_total > 1}
          {' · '}+{(d.claims_total - 1).toLocaleString()} more claims{/if}
      </div>
    {:else if d.roots.length > 0}
      {@const root = d.roots[0]}
      <h2 class="title">
        {#if root.relation === 'makes'}
          helps rebuild
        {:else if root.relation === 'derived_from'}
          derived from
        {:else}
          related to
        {/if}
        <Link class="title-link" href={`/storage/blob/${root.hash}`}>{root.entry}</Link>
      </h2>
      <div class="title-sub">
        {root.source}{#if d.roots.length > 1}
          {' · '}+{(d.roots.length - 1).toLocaleString()} more{/if}
      </div>
    {:else if d.sniff !== null}
      <h2 class="title">{d.sniff}</h2>
      <div class="title-sub">unattached — nothing claimed connects to it</div>
    {:else}
      <h2 class="title">unattached blob</h2>
      <div class="title-sub">nothing claimed connects to it</div>
    {/if}
    <div class="head">
      <span class="hash">{d.hash}</span>
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
      {:else if d.residency === 'resident'}
        <button class="badge dim verify" disabled={verifying} onclick={verifyNow}>
          {#if verifying}verifying…{:else}never verified — verify now{/if}
        </button>
      {:else}
        <!-- Not on disk: verification means replay, which stays CLI. -->
        <span class="badge dim">never verified</span>
      {/if}
      {#if verifyError !== null}
        <span class="badge bad-badge">{verifyError}</span>
      {/if}
    </div>

    <div class="cards">
      <section class="card">
        <div class="card-title">digests</div>
        <div class="kv">
          {#each digestRows as [algo, hex] (algo)}
            <span class="k">{algo}</span>
            <button class="v copyable" title={copyDigestTitle} onclick={() => copyDigest(algo, hex)}>
              {hex}{#if copiedDigest === algo}
                <span class="copied-tick">✓</span>{/if}
            </button>
          {/each}
        </div>
      </section>

      <section class="card">
        <div class="card-title">provenance</div>
        {#if d.provenance.length > 0}
          {#each d.provenance as row (row.path)}
            <div class="prov-row">
              <span class="prov-path">{row.path}</span>
              <span class="prov-when">
                {row.ingested_at === null ? '' : fmtDate(row.ingested_at)}
              </span>
            </div>
          {/each}
        {:else if d.provenance_via.length > 0}
          <!-- Viral display (D79): a derived blob's bytes arrived as
               somebody's file — that path lives on a CONNECTED blob. -->
          {#each d.provenance_via as row (row.path)}
            <div class="prov-row">
              <span class="prov-path">
                {row.path}
                <Link class="prov-via" href={`/storage/blob/${row.via}`}
                  >via {shortHash(row.via)}</Link
                >
              </span>
              <span class="prov-when">
                {row.ingested_at === null ? '' : fmtDate(row.ingested_at)}
              </span>
            </div>
          {/each}
        {:else}
          <p class="none">no recorded source paths</p>
        {/if}
      </section>

      <section class="card">
        <div class="card-title">routes in — ways to make it</div>
        {#if d.routes_in.length === 0}
          <p class="none">no recipe produces this blob — a literal</p>
        {:else}
          {@render edgeList(d.routes_in, 'in')}
        {/if}
      </section>

      <section class="card">
        <div class="card-title">routes out — things made from it</div>
        {#if d.routes_out.length === 0}
          <p class="none">no recipe consumes this blob</p>
        {:else}
          {@render edgeList(d.routes_out, 'out')}
        {/if}
      </section>

      <section class="card">
        <div class="card-title">claims · {d.claims_total.toLocaleString()}</div>
        {#if d.claims.length === 0}
          <p class="none">satisfies no dat claims</p>
        {:else}
          {#each d.claims as claim (claim.source + claim.entry)}
            <div class="claim-row">
              <span class="claim-entry" title={claim.entry}>{claim.entry}</span>
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
    font: 500 0.78125rem var(--font-data);
  }

  .crumbs :global(a.back) {
    color: var(--mut);
    text-decoration: none;
  }

  .crumbs :global(a.back:hover) {
    color: var(--text);
  }

  .undesigned {
    font: 400 0.78125rem var(--font-data);
    color: var(--faint);
  }

  .title {
    margin: 0;
    font: 800 1.25rem var(--font-display);
    letter-spacing: -0.02em;
    overflow-wrap: anywhere;
  }

  .title :global(a.title-link) {
    color: var(--text);
    text-decoration: underline;
    text-decoration-color: var(--dim);
    text-underline-offset: 3px;
  }

  .title-sub {
    font: 400 0.78125rem var(--font-data);
    color: var(--faint);
    margin: 2px 0 10px;
  }

  .head {
    display: flex;
    align-items: center;
    gap: 14px;
    min-width: 0;
  }

  .hash {
    font: 500 0.78125rem var(--font-data);
    color: var(--mut);
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
    font: 600 0.75rem var(--font-data);
    cursor: pointer;
  }

  .badges {
    display: flex;
    gap: 8px;
    margin: 10px 0 18px;
    flex-wrap: wrap;
  }

  .badge {
    font: 500 0.71875rem var(--font-data);
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

  button.badge.verify {
    all: unset;
    font: 500 0.71875rem var(--font-data);
    border: 1.5px dashed var(--hair);
    border-radius: var(--r-pill);
    padding: 2px 10px;
    color: var(--faint);
    cursor: pointer;
  }

  button.badge.verify:hover:not(:disabled) {
    color: var(--text);
    border-color: var(--edge);
  }

  button.badge.verify:disabled {
    cursor: progress;
  }

  .badge.bad-badge {
    color: var(--bad);
    border-color: var(--bad);
  }

  .cards {
    display: grid;
    /* minmax(0, …): a plain 1fr track's MIN size is its content's
       min-content width, so one card holding a wide recipe silently
       broke the 50/50 on exactly the blobs with big assembles. */
    grid-template-columns: repeat(2, minmax(0, 1fr));
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
    font: 800 0.8125rem var(--font-display);
    letter-spacing: 0.02em;
    margin-bottom: 8px;
  }

  .none {
    margin: 0;
    font: 400 0.75rem var(--font-data);
    color: var(--faint);
  }

  .kv {
    display: grid;
    grid-template-columns: auto 1fr;
    column-gap: 14px;
    row-gap: 3px;
    font: 400 0.75rem var(--font-data);
  }

  .k {
    color: var(--faint);
  }

  .v {
    color: var(--text);
    overflow-wrap: anywhere;
  }

  .copyable {
    all: unset;
    color: var(--text);
    overflow-wrap: anywhere;
    min-width: 0;
    cursor: copy;
    text-align: left;
  }

  .copyable:hover {
    text-decoration: underline;
    text-decoration-style: dotted;
    text-underline-offset: 2px;
  }

  .copied-tick {
    color: var(--okT);
  }

  .prov-row {
    display: flex;
    gap: 12px;
    align-items: baseline;
    font: 400 0.75rem var(--font-data);
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

  .prov-row :global(a.prov-via) {
    color: var(--faint);
    text-decoration: underline;
    text-decoration-color: var(--dim);
    text-underline-offset: 2px;
    margin-left: 6px;
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
    font: 600 0.78125rem var(--font-data);
  }

  .verify {
    font: 400 0.6875rem var(--font-data);
    color: var(--faint);
  }

  .edge-cols {
    display: grid;
    grid-template-columns: minmax(0, 1fr) minmax(0, 1fr);
    gap: 14px;
  }

  .edge-count {
    font: 400 0.6875rem var(--font-data);
    color: var(--mut);
  }

  .more {
    all: unset;
    cursor: pointer;
    font: 500 0.71875rem var(--font-data);
    color: var(--faint);
    padding: 2px 0;
  }

  .more:hover {
    color: var(--text);
  }

  .refs {
    display: flex;
    flex-direction: column;
    gap: 2px;
    min-width: 0;
  }

  .refs-label {
    font: 600 0.65625rem var(--font-data);
    color: var(--faint);
    letter-spacing: 0.04em;
  }

  .ref {
    display: flex;
    gap: 8px;
    align-items: baseline;
    font: 400 0.75rem var(--font-data);
    min-width: 0;
  }

  .ref :global(a.ref-hash) {
    color: var(--text);
    text-decoration: underline;
    text-decoration-color: var(--dim);
    text-underline-offset: 2px;
  }

  /* Self-reference: same slot as a ref hash, deliberately not a link. */
  .ref-self {
    color: var(--accent, var(--mut));
    font-style: italic;
    flex: none;
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
    font: 400 0.78125rem var(--font-data);
    line-height: 1.8;
  }

  /* Ellipsis, not `overflow-wrap: anywhere` — anywhere-wrapping a NAME
     squeezed by an unshrinkable source chip degenerates to one char per
     line at narrow widths. Anywhere is for hashes (87-web-ui.md). */
  .claim-entry {
    flex: 1;
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
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
    font: 500 0.75rem var(--font-data);
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
