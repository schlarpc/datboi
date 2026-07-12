<script lang="ts">
  /**
   * Friend browse (spec §4.3 + §5.11–15): flat manifest rows of the
   * view's current snapshot, server-side search, quick-download pills,
   * the entry panel (the owner drawer minus storage internals/verify —
   * a snapshot only serves verified content, so there is nothing to
   * verify or fold), the trust bar, and the SD-image modal.
   *
   * Rulings baked in:
   * - Rows are the FLAT full-path listing. Folder rows are a §4.3
   *   wireframe sketch (3c); the interactive friend prototype is flat,
   *   and flat is its canon — nesting can arrive later without an API
   *   change (paths already carry `/`).
   * - Rows PAGINATE like Audit: server q + offset/limit with a "load
   *   more" row (same rationale — the server already filters).
   * - Downloads are real anchors into /view/{name}/{path}: the browser
   *   streams from the verified range path (D49). No progress theater —
   *   the browser's own download UI is the truth.
   */
  import { viewDetail, viewFiles, viewFileUrl, viewImageUrl } from '../lib/api/client';
  import type { ViewDetail, ViewFileRow } from '../lib/api/types';
  import { fmtAge, fmtSize, parseRegion, shortHash, snapShort } from '../lib/format';

  let { view }: { view: string } = $props();

  const PAGE = 500;

  let detail = $state<ViewDetail | null>(null);
  let error = $state<string | null>(null);
  let files = $state<ViewFileRow[]>([]);
  let total = $state(0);
  let q = $state('');
  let selected = $state<ViewFileRow | null>(null);
  let modal = $state(false);
  /** Paths whose quick-download pill is flashing `✓` (§5.12, ~1.4s). */
  let flashed = $state<Record<string, boolean>>({});

  $effect(() => {
    viewDetail(view).then(
      (body) => (detail = body),
      (e: unknown) => (error = e instanceof Error ? e.message : String(e)),
    );
  });

  // Search recomposes server-side; stale answers are dropped by
  // generation counter so a slow page can't overwrite a newer one.
  let generation = 0;
  $effect(() => {
    const params = { q: q || undefined };
    const gen = ++generation;
    viewFiles(view, { ...params, offset: 0, limit: PAGE }).then(
      (body) => {
        if (gen === generation) {
          files = body.files;
          total = body.total;
        }
      },
      (e: unknown) => (error = e instanceof Error ? e.message : String(e)),
    );
  });

  async function loadMore() {
    const gen = generation;
    const body = await viewFiles(view, {
      q: q || undefined,
      offset: files.length,
      limit: PAGE,
    });
    if (gen === generation) {
      files = [...files, ...body.files];
      total = body.total;
    }
  }

  function select(row: ViewFileRow) {
    selected = selected?.path === row.path ? null : row;
  }

  /** §5.12: the pill is a real download anchor; the click must not
   * select the row, and the ✓ flash confirms without a dialog. */
  function quickDownload(event: MouseEvent, row: ViewFileRow) {
    event.stopPropagation();
    flashed[row.path] = true;
    setTimeout(() => delete flashed[row.path], 1400);
  }

  /** §5.14 Escape ordering: modal first, then the entry panel. */
  function onkeydown(event: KeyboardEvent) {
    // @wc-ignore
    if (event.key !== 'Escape') {
      return;
    }
    if (modal) {
      modal = false;
    } else if (selected !== null) {
      selected = null;
    }
  }

  const basename = (path: string) => path.split('/').at(-1) ?? path;

  const region = $derived(selected === null ? null : parseRegion(basename(selected.path)));
  const image = $derived(detail?.image?.minted === true ? detail.image : null);

  // Lowercase attribute copy, forced at statement level (the codebase
  // pattern — an element directive would sweep class names too).
  // @wc-include
  const searchPlaceholder = 'find a game…';
  // @wc-include
  const playTitle = 'in-browser emulator — future';
  // @wc-include
  const closeLabel = 'close';
  // @wc-include
  const quickDownloadLabel = 'download';
</script>

<svelte:window {onkeydown} />

<main>
  {#if error !== null}
    <!-- Undesigned loading/error states: plain mono in --faint. -->
    <p class="undesigned">something went wrong — {error}</p>
  {:else}
    <div class="toolbar">
      <input type="search" placeholder={searchPlaceholder} bind:value={q} />
      {#if detail !== null && detail.snapshot !== null}
        <!-- Snapshot facts, not search results: the file count here is
             the whole manifest (the prototype's static info line); the
             filtered count lives on the load-more row. -->
        <span class="info">
          snap {snapShort(detail.snapshot)}{#if detail.created_at != null}{' · '}{fmtAge(
              detail.created_at,
            )} ago{/if}{#if detail.rows != null}{' · '}{detail.rows.toLocaleString()} files{/if}
        </span>
      {/if}
    </div>

    <div class="table">
      <div class="rows">
        {#if files.length === 0}
          {#if q === ''}
            <!-- An evaluated-but-empty snapshot; not a search miss. -->
            <p class="empty">this shelf is empty</p>
          {:else}
            <p class="empty">nothing matches “{q}”</p>
          {/if}
        {:else}
          {#each files as row (row.path)}
            {@const isSel = selected?.path === row.path}
            <!-- div, not button: the quick-download pill inside is a
                 real anchor and anchors can't nest in buttons. -->
            <div
              class="row"
              class:sel={isSel}
              onclick={() => select(row)}
              onkeydown={(e) => {
                // @wc-ignore
                if (e.key === 'Enter') select(row);
              }}
              role="button"
              tabindex="0"
            >
              <!-- Every row is verified: a snapshot only serves verified
                   content (spec §4.3). -->
              <span class="dot dot--verified"></span>
              <span class="row-name" class:bold={isSel}>{row.path}</span>
              <span class="size">{fmtSize(row.size)}</span>
              <a
                class="quick"
                href={viewFileUrl(view, row.path)}
                download={basename(row.path)}
                aria-label={quickDownloadLabel}
                onclick={(e) => quickDownload(e, row)}
              >
                {flashed[row.path] ? '✓' : '⬇'}
              </a>
            </div>
          {/each}
          {#if files.length < total}
            <button class="load-more" onclick={loadMore}>
              load more ({files.length.toLocaleString()} / {total.toLocaleString()})
            </button>
          {/if}
        {/if}
      </div>

      {#if selected !== null}
        <aside class="panel">
          <div class="head">
            <span class="caps">ENTRY</span>
            <button class="close" onclick={() => (selected = null)} aria-label={closeLabel}>
              ✕
            </button>
          </div>
          <!-- Box-art metadata provider is explicitly future (spec §8). -->
          <div class="art">box art — metadata provider, later</div>
          <div class="name">{basename(selected.path)}</div>
          <div class="sub">
            {#if region !== null}{region}{' · '}{/if}{fmtSize(selected.size)}{' · '}{shortHash(
              selected.hash,
            )}
          </div>
          <div class="trust-line">
            <!-- @wc-context: storage state -->
            <span class="word">● verified</span>
            <span class="meaning">hash-checked as it streams</span>
          </div>
          <div class="actions">
            <a
              class="download"
              href={viewFileUrl(view, selected.path)}
              download={basename(selected.path)}
            >
              ⬇ Download
            </a>
            <button class="play" disabled title={playTitle}>▶ Play</button>
          </div>
          <div class="hint">play: in-browser core over verified ranges — future</div>
          <div class="section">
            <!-- Footnote only: no metadata provider yet, so no fake
                 released/publisher/genre rows (the owner-drawer ruling). -->
            <div class="hint">
              metadata provider, later — the dat name stays the source of truth
            </div>
          </div>
        </aside>
      {/if}
    </div>

    <footer class="trust-bar">
      <span class="promise">● every download is hash-verified as it streams</span>
      {#if image !== null}
        <!-- Only when an image is actually minted (D62 tag) — the pill
             never advertises a download that would 404. -->
        <button class="image-pill" onclick={() => (modal = true)}>
          ⬇ whole SD image{#if image.bytes != null}{' · '}{fmtSize(image.bytes)}{/if}
        </button>
      {/if}
    </footer>

    {#if modal && image !== null}
      <!-- Backdrop click closes; the card swallows clicks (§5.14). -->
      <div
        class="backdrop"
        onclick={() => (modal = false)}
        onkeydown={() => {}}
        role="presentation"
      >
        <div
          class="modal"
          onclick={(e) => e.stopPropagation()}
          onkeydown={() => {}}
          role="dialog"
          aria-modal="true"
          tabindex="-1"
        >
          <div class="modal-title">SD image · {view}</div>
          {#if detail !== null && detail.snapshot !== null}
            <div class="modal-sub">
              FAT32{#if image.bytes != null}{' · '}{fmtSize(image.bytes)}{/if}{' · '}minted from
              snap {snapShort(detail.snapshot)}
            </div>
          {/if}
          <div class="warning">
            ⚠ Re-flashing a card overwrites on-device saves. Back up your saves first.
          </div>
          <!-- No progress bar: this is a plain anchor download, so the
               browser owns (and truthfully shows) the progress. The
               prototype's client-side "streaming + verifying · 47%"
               bar only makes sense if we ever stream in-page — a fake
               one here would be theater. -->
          <div class="modal-actions">
            <button class="cancel" onclick={() => (modal = false)}>cancel</button>
            <a class="confirm" href={viewImageUrl(view)} download={`${view}.img`}>
              I understand — download
            </a>
          </div>
        </div>
      </div>
    {/if}
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
    padding: 26px var(--pad-x);
  }

  .toolbar {
    display: flex;
    align-items: center;
    gap: 16px;
    padding: 14px var(--pad-x);
  }

  .toolbar input {
    flex: 1;
    max-width: 420px;
    box-sizing: border-box;
    font: 400 13px var(--font-data);
    padding: 8px 14px;
    border: 2px solid var(--ink);
    border-radius: var(--r-pill);
    background: var(--panel);
    color: var(--text);
  }

  .info {
    margin-left: auto;
    font: 500 12px var(--font-data);
    color: var(--faint);
    white-space: nowrap;
  }

  .table {
    flex: 1;
    display: flex;
    min-height: 0;
    border-top: 1px solid var(--hair);
    background: var(--panel);
  }

  .rows {
    flex: 1;
    overflow-y: auto;
    min-width: 0;
  }

  .row {
    display: flex;
    align-items: center;
    gap: 12px;
    padding: 9px 24px;
    border-bottom: 1px solid var(--rule);
    cursor: pointer;
  }

  .row:hover {
    background: var(--hover-row);
  }

  .row.sel {
    background: color-mix(in srgb, var(--ok) 10%, var(--panel));
    box-shadow: inset 3px 0 0 var(--ok);
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

  .size {
    font: 400 11.5px var(--font-data);
    color: var(--dim);
    width: 46px;
    text-align: right;
    flex: none;
  }

  .quick {
    border: 1.5px solid var(--hair);
    border-radius: var(--r-pill);
    padding: 2px 12px;
    font: 500 12px var(--font-data);
    color: var(--mut);
    text-decoration: none;
    flex: none;
  }

  .quick:hover {
    border-color: var(--ink);
    color: var(--text);
  }

  .empty {
    font: 400 12.5px var(--font-data);
    color: var(--faint);
    padding: 28px 24px;
    margin: 0;
  }

  .load-more {
    all: unset;
    display: block;
    width: 100%;
    box-sizing: border-box;
    padding: 10px 24px;
    text-align: center;
    font: 500 12px var(--font-data);
    color: var(--faint);
    cursor: pointer;
  }

  .load-more:hover {
    color: var(--text);
  }

  /* ---- entry panel (owner drawer register, spec §4.3) ---- */

  .panel {
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
    overflow-wrap: anywhere;
  }

  .sub {
    font: 400 11.5px var(--font-data);
    color: var(--faint);
    margin: 4px 0 12px;
  }

  .trust-line .word {
    font: 600 12px var(--font-data);
    color: var(--okT);
  }

  .trust-line .meaning {
    font: 400 11px var(--font-data);
    color: var(--faint);
  }

  .actions {
    margin: 14px 0 6px;
    display: flex;
    align-items: center;
    gap: 10px;
  }

  .download {
    background: var(--ink);
    color: var(--bg);
    border-radius: var(--r-pill);
    padding: 7px 16px;
    font: 600 13px var(--font-display);
    text-decoration: none;
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

  /* ---- trust bar (replaces the jobs tray, spec §4.3) ---- */

  .trust-bar {
    display: flex;
    align-items: center;
    gap: 16px;
    padding: 9px var(--pad-x);
    border-top: 2px solid var(--ink);
    background: var(--tray);
    font: 500 12px var(--font-data);
  }

  .promise {
    color: var(--okT);
  }

  .image-pill {
    all: unset;
    margin-left: auto;
    border: 2px solid var(--ink);
    border-radius: var(--r-pill);
    padding: 4px 14px;
    background: var(--panel);
    font: 600 12px var(--font-data);
    color: var(--text);
    cursor: pointer;
  }

  /* ---- SD image modal (spec §4.4) ---- */

  .backdrop {
    position: fixed;
    inset: 0;
    background: var(--backdrop-modal);
    display: flex;
    align-items: center;
    justify-content: center;
    z-index: 10;
  }

  .modal {
    width: min(420px, calc(100vw - 32px));
    box-sizing: border-box;
    background: var(--panel);
    border: 2px solid var(--ink);
    border-radius: var(--r-modal);
    padding: 24px 26px;
    box-shadow: var(--shadow-modal);
  }

  .modal-title {
    font: 800 17px var(--font-display);
    letter-spacing: -0.02em;
  }

  .modal-sub {
    margin-top: 4px;
    font: 400 12px var(--font-data);
    color: var(--faint);
  }

  .warning {
    margin-top: 14px;
    padding: 12px 14px;
    border: 1.5px solid var(--bad);
    border-radius: var(--r-sub);
    background: color-mix(in srgb, var(--bad) 8%, var(--panel));
    font: 500 12.5px var(--font-data);
    color: var(--bad);
    line-height: 1.6;
  }

  .modal-actions {
    margin-top: 18px;
    display: flex;
    justify-content: flex-end;
    gap: 10px;
  }

  .cancel {
    all: unset;
    border: 2px solid var(--hair);
    color: var(--mut);
    border-radius: var(--r-pill);
    padding: 6px 16px;
    font: 600 12px var(--font-data);
    cursor: pointer;
  }

  .confirm {
    background: var(--ink);
    color: var(--bg);
    border-radius: var(--r-pill);
    padding: 7px 18px;
    font: 600 13px var(--font-display);
    text-decoration: none;
  }

  /* Mobile: the entry panel becomes a bottom sheet (matching the owner
     drawer), the toolbar's snapshot info drops under the search box, and
     the row gutter follows the shell. */
  @media (max-width: 720px) {
    .toolbar {
      flex-wrap: wrap;
      gap: 8px 16px;
    }

    .toolbar input {
      max-width: none;
    }

    .info {
      margin-left: 0;
      white-space: normal;
    }

    .row {
      padding: 9px var(--pad-x);
    }

    .empty,
    .load-more {
      padding-left: var(--pad-x);
      padding-right: var(--pad-x);
    }

    .panel {
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
      padding-bottom: max(20px, env(safe-area-inset-bottom));
    }
  }
</style>
