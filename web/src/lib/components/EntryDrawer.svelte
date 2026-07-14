<script lang="ts">
  /**
   * The ENTRY drawer (spec §3.2, wireframe 2c: "same panel works for
   * any blob/file row anywhere — one component, used everywhere").
   * 300px, 2px ink left edge. The drawer SUMMARIZES one entry and
   * links out; the blob page owns storage internals (web-ui.md:
   * one canonical home per concept — the old fold here was a second,
   * worse rendering of the same truth).
   *
   * Rulings baked in:
   * - No `⬇ Download` action: catalog entries have no serving route
   *   outside views in the M5 API (only /view/... serves bytes), so a
   *   download button would 404. The slot returns when an
   *   entry-serving route exists.
   * - `▶ Play` (D85): one per rom claim that a local blob satisfies
   *   AND a shipped core claims by extension — blob-sourced play over
   *   the owner-only /v1 bytes surface. No placeholder ever rendered
   *   for the rest (dev-facing scaffolding is not UI, web-ui.md);
   *   the artwork slot stays visual-only for the same reason.
   */
  import type { EntryDetail } from '../api/types';
  import { coreForPath, playBlobUrl } from '../emu/registry';
  import { residencyLabel } from '../residency.svelte';
  import { fmtDate, fmtSize, parseRegion, shortHash } from '../format';
  import Link from './Link.svelte';

  let {
    detail,
    onclose,
    stale = false,
  }: {
    detail: EntryDetail;
    onclose: () => void;
    /** The NEXT selection is still loading: this content is the
     * previous entry's, kept up (dimmed) so the pane never flickers
     * closed between rows. */
    stale?: boolean;
  } = $props();

  const region = $derived(parseRegion(detail.name));

  // The playable roms (D85): satisfied by a local blob and claimed by
  // a core. Flattened here so the template needs no non-null dances.
  const playable = $derived(
    detail.roms.flatMap((rom) =>
      rom.blob != null && coreForPath(rom.name) !== null
        ? [{ name: rom.name, hash: rom.blob.hash }]
        : [],
    ),
  );

  // Lowercase attribute copy, forced at statement level (an element
  // directive would sweep class names into the catalog too).
  // @wc-include
  const closeLabel = 'close';

  // Focus contract: the drawer takes focus when it opens (it mounts on
  // row selection) and hands it back to the opener when it closes —
  // otherwise a keyboard user is still parked on the row while the
  // drawer's controls sit unreachable behind a dozen Tab stops.
  let drawerEl = $state<HTMLElement | null>(null);
  $effect(() => {
    if (drawerEl === null) return;
    const opener = document.activeElement;
    drawerEl.focus();
    return () => {
      if (opener instanceof HTMLElement && opener.isConnected) opener.focus();
    };
  });
</script>

<aside class="drawer" class:stale bind:this={drawerEl} tabindex="-1">
  <div class="head">
    <span class="caps">ENTRY</span>
    <button class="close" onclick={onclose} aria-label={closeLabel}>✕</button>
  </div>

  <!-- Artwork slot: box-art metadata provider is explicitly future
       (§8). The slot is visual; an empty dashed box reads as an empty
       art slot without a caption saying so. -->
  <div class="art"></div>

  <div class="name">{detail.name}</div>
  <!-- The ` · ` separators are literal expressions so they survive
       whitespace collapsing at block boundaries. -->
  <div class="sub">
    {#if region !== null}{region}{' · '}{/if}{#if detail.size !== null}{fmtSize(
        detail.size,
      )}{:else}not in library{/if}{#if detail.wanted_hash !== null}{' · '}{shortHash(
        detail.wanted_hash,
      )}{/if}
  </div>

  <div class="state-line state-text--{detail.state}">
    <!-- CSS-drawn mark (web-ui.md: structure over glyph). -->
    <span class="dot dot--{detail.state}"></span>
    {#if detail.state === 'verified'}
      <!-- @wc-context: storage state -->verified
    {:else if detail.state === 'claimed'}
      <!-- @wc-context: storage state -->claimed
    {:else if detail.state === 'missing'}
      <!-- @wc-context: storage state -->missing
    {:else}
      <!-- @wc-context: storage state -->no dump
    {/if}
  </div>

  {#if playable.length > 0}
    <div class="actions">
      {#each playable as rom (rom.name)}
        <Link class="play" href={playBlobUrl(rom.hash, rom.name)}>
          <!-- The rom name disambiguates only when the entry has more
               than one — the single-rom case stays a clean verb. -->
          ▶ Play{#if detail.roms.length > 1}{' · '}{rom.name}{/if}
        </Link>
      {/each}
    </div>
  {/if}

  {#if detail.roms.some((rom) => rom.blob)}
    <div class="section stor">
      {#each detail.roms as rom (rom.name)}
        {#if detail.roms.length > 1}
          <div class="rom-name">{rom.name}</div>
        {/if}
        {#if rom.blob}
          {@const blob = rom.blob}
          <div>
            blob <Link class="blob-link" href={`/storage/blob/${blob.hash}`}
              >{shortHash(blob.hash)}</Link
            >
            · {residencyLabel(blob.residency)}
          </div>
          {#if blob.verified_at !== null}
            <div>verified {fmtDate(blob.verified_at)}</div>
          {/if}
        {/if}
      {/each}
    </div>
  {/if}
</aside>

<style>
  .drawer {
    width: 300px;
    flex: none;
    border-left: 2px solid var(--ink);
    background: var(--bg);
    padding: 16px 18px 20px;
    overflow-y: auto;
    box-sizing: border-box;
    transition: opacity 0.15s;
  }

  .drawer.stale {
    opacity: 0.55;
  }

  .head {
    display: flex;
    align-items: center;
    justify-content: space-between;
  }

  .caps {
    font: 800 0.8125rem var(--font-display);
    letter-spacing: 0.02em;
  }

  .close {
    all: unset;
    cursor: pointer;
    color: var(--faint);
    font-size: 0.8125rem;
    padding: 2px 4px;
  }

  .art {
    height: 132px;
    border: 1.5px dashed var(--hair);
    border-radius: var(--r-sub);
    background: var(--hatch-placeholder);
    margin: 12px 0 14px;
  }

  .name {
    font-size: 0.875rem;
    font-weight: 600;
    line-height: 1.35;
  }

  .sub {
    font: 400 0.71875rem var(--font-data);
    color: var(--faint);
    margin: 4px 0 12px;
  }

  .state-line {
    font: 600 0.75rem var(--font-data);
    display: flex;
    align-items: center;
    gap: 7px;
  }

  .actions {
    margin-top: 14px;
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 10px;
  }

  /* :global — the pill lives on the anchor inside <Link>, outside
     this component's scope hash. Same shape as Browse's actions. */
  .actions :global(.play) {
    background: var(--ink);
    color: var(--bg);
    border-radius: var(--r-pill);
    padding: 7px 16px;
    font: 600 0.8125rem var(--font-display);
    text-decoration: none;
  }

  .section {
    border-top: 1px dashed var(--hair);
    margin-top: 14px;
    padding-top: 12px;
  }

  .stor {
    font: 400 0.75rem var(--font-data);
    color: var(--mut);
    line-height: 1.8;
    overflow-wrap: anywhere;
  }

  .stor :global(.blob-link) {
    color: var(--text);
    text-decoration: underline;
    text-underline-offset: 2px;
  }

  .rom-name {
    color: var(--faint);
  }

  /* On a phone there's no room for a 300px side rail, so the drawer
     detaches into a bottom sheet: full width, pinned to the bottom of
     the viewport, scrollable, and lifted above the list. The ✕ and the
     Escape handler already dismiss it. */
  @media (max-width: 720px) {
    .drawer {
      position: fixed;
      left: 0;
      right: 0;
      bottom: 0;
      top: auto;
      width: auto;
      max-height: 78dvh;
      border-left: none;
      border-top: 2px solid var(--ink);
      border-radius: var(--r-card) var(--r-card) 0 0;
      box-shadow: 0 -6px 24px color-mix(in srgb, var(--ink) 22%, transparent);
      z-index: 20;
      /* Clear the iOS home indicator. */
      padding-bottom: max(20px, env(safe-area-inset-bottom));
    }
  }
</style>
