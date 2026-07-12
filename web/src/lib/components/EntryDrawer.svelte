<script lang="ts">
  /**
   * The ENTRY drawer (spec §3.2, wireframe 2c: "same panel works for
   * any blob/file row anywhere — one component, used everywhere").
   * 300px, 2px ink left edge; artwork placeholder, name + sub, state +
   * meaning, actions, metadata footnote, folded storage internals.
   *
   * M5 rulings baked in:
   * - No `⬇ Download` action: catalog entries have no serving route
   *   outside views in the M5 API (only /view/... serves bytes), so a
   *   download button would 404. Reported to the coordinator; the slot
   *   returns when an entry-serving route exists.
   * - No `verify now`: verification replay is CLI-only in M5 (api.rs
   *   scope ruling — mutating pipeline actions stay CLI-only).
   * - `▶ Play` stays a disabled placeholder (explicitly future, §8).
   * - Metadata section renders its footnote only — no provider exists,
   *   so no fake released/publisher/genre rows.
   */
  import type { EntryDetail } from '../api/types';
  import { residencyLabel } from '../residency.svelte';
  import { fmtDate, fmtSize, parseRegion, shortHash } from '../format';
  import { STATE_GLYPHS } from '../state';

  let {
    detail,
    onclose,
  }: {
    detail: EntryDetail;
    onclose: () => void;
  } = $props();

  const region = $derived(parseRegion(detail.name));
  let storOpen = $state(false);


  // Lowercase attribute copy, forced at statement level (an element
  // directive would sweep class names into the catalog too).
  // @wc-include
  const playTitle = 'in-browser emulator — future';
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

<aside class="drawer" bind:this={drawerEl} tabindex="-1">
  <div class="head">
    <span class="caps">ENTRY</span>
    <button class="close" onclick={onclose} aria-label={closeLabel}>✕</button>
  </div>

  <!-- Artwork slot: box-art metadata provider is explicitly future (§8). -->
  <div class="art">box art — metadata provider, later</div>

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
    {STATE_GLYPHS[detail.state]}
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
  <div class="meaning">
    {#if detail.state === 'verified'}
      bytes on hand, hash checked against the catalog
    {:else if detail.state === 'claimed'}
      bytes rebuildable, not yet re-verified
    {:else if detail.state === 'missing'}
      no blob or claim names this hash
    {:else}
      the catalog marks this entry as never dumped — nothing to have
    {/if}
  </div>

  {#if detail.state === 'verified' || detail.state === 'claimed'}
    <div class="actions">
      <!-- ⬇ Download omitted for M5 (see header comment). -->
      <button class="play" disabled title={playTitle}>▶ Play</button>
    </div>
    <div class="hint">play: in-browser core over verified ranges — future</div>
  {/if}

  <div class="section">
    <!-- Footnote only: no metadata provider yet, so no fake k/v rows. -->
    <div class="hint">metadata provider, later — the dat name stays the source of truth</div>
  </div>

  <div class="section">
    <button class="fold" aria-expanded={storOpen} onclick={() => (storOpen = !storOpen)}>
      {storOpen ? '▾' : '▸'} storage internals
    </button>
    {#if storOpen}
      <div class="stor">
        {#if detail.state === 'missing' || detail.state === 'nodump'}
          {#if detail.state === 'missing'}
            {#if detail.wanted_hash !== null}
              <div>wanted {shortHash(detail.wanted_hash)}</div>
            {/if}
            {#if detail.revision.version !== null}
              <div>added in dat rev {detail.revision.version}</div>
            {/if}
            <div>appears in missing-list export</div>
          {:else}
            <div>excluded from completeness math</div>
          {/if}
        {:else}
          <!-- Real route/pins/residency from entry detail; only fields
               the API returned are rendered (no invented scrub method —
               the index records a date, never a "how"). -->
          {#each detail.roms as rom (rom.name)}
            {#if detail.roms.length > 1}
              <div class="rom-name">{rom.name}</div>
            {/if}
            {#if rom.blob}
              <div>
                <!-- Exhaustive: a fourth ResidencyState fails check
                     here instead of rendering as "evicted (covered)". -->
                blob {shortHash(rom.blob.hash)} ·
                {residencyLabel(rom.blob.residency)}
              </div>
              {#if rom.blob.verified_at !== null}
                <div>verified {fmtDate(rom.blob.verified_at)}</div>
              {/if}
            {/if}
            {#each rom.routes ?? [] as route (route.route)}
              <div>route&nbsp;&nbsp;{route.route} {route.source_present ? '●' : '○'}</div>
              {#if route.verify === 'pending'}
                <div>verify pending replay</div>
              {/if}
            {/each}
            {#if rom.pins != null}
              {#if rom.pins.length > 0}
                <div>pinned {rom.pins.join(', ')}</div>
              {:else}
                <div>pins&nbsp;&nbsp;&nbsp;— none</div>
              {/if}
            {/if}
          {/each}
          <!-- `verify now` intentionally absent: CLI-only in M5. -->
        {/if}
      </div>
    {/if}
  </div>
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
  }

  .head {
    display: flex;
    align-items: center;
    justify-content: space-between;
  }

  .caps {
    font: 800 13px var(--font-display);
    letter-spacing: 0.02em;
  }

  .close {
    all: unset;
    cursor: pointer;
    color: var(--faint);
    font-size: 13px;
    padding: 2px 4px;
  }

  .art {
    height: 132px;
    border: 1.5px dashed var(--hair);
    border-radius: var(--r-sub);
    background: var(--hatch-placeholder);
    display: flex;
    align-items: center;
    justify-content: center;
    text-align: center;
    font: 400 10.5px var(--font-data);
    color: var(--dim);
    margin: 12px 0 14px;
  }

  .name {
    font-size: 14px;
    font-weight: 600;
    line-height: 1.35;
  }

  .sub {
    font: 400 11.5px var(--font-data);
    color: var(--faint);
    margin: 4px 0 12px;
  }

  .state-line {
    font: 600 12px var(--font-data);
  }

  .meaning {
    font: 400 11px var(--font-data);
    color: var(--faint);
    margin: 4px 0 14px;
    line-height: 1.5;
  }

  .actions {
    margin-bottom: 6px;
  }

  .play {
    all: unset;
    border: 2px solid var(--hair);
    color: var(--faint);
    border-radius: var(--r-pill);
    padding: 5px 14px;
    font: 600 12px var(--font-data);
    cursor: not-allowed;
  }

  .hint {
    font: 400 10.5px var(--font-data);
    color: var(--dim);
    line-height: 1.5;
  }

  .section {
    border-top: 1px dashed var(--hair);
    margin-top: 14px;
    padding-top: 12px;
  }

  .fold {
    all: unset;
    cursor: pointer;
    font: 600 11.5px var(--font-data);
    color: var(--mut);
  }

  .stor {
    margin-top: 8px;
    font: 400 12px var(--font-data);
    color: var(--mut);
    line-height: 1.8;
    overflow-wrap: anywhere;
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
