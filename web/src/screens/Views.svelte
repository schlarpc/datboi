<script lang="ts">
  /**
   * Views list (spec §3.3, hi-fi 5b/5e): compiled shelves as cartridge
   * cards — band from the source system, sub-line composed from the
   * stored definition, one primary verb per device profile.
   *
   * M5 scope ruling (open-questions 2026-07-11): mutating pipeline
   * actions are CLI-only, so re-eval / new-view / edit-definition
   * render the design's action slots as CLI-hint reveals (the pattern
   * Library's "+ import a dat" card established). The REAL actions
   * here are clipboard copies of the serving endpoints (HTTP + WebDAV)
   * and the browse link into the served tree.
   *
   * Minted images ARE HTTP-downloadable (GET /v1/views/{name}/image,
   * same verified-range machinery as /view files), so `⬇ Export SD
   * image` is a real download anchor once an image exists; MINTING
   * stays CLI-only like every other mutating action, so an unminted
   * image profile reveals the `datboi view image <name>` incantation.
   *
   * View editor (§3.4) and eval report/diff (§3.5) are NOT built as
   * screens: definitions are read-only here (the ⋯ "definition" fold)
   * and no eval-history API exists — deferral recorded in
   * docs/open-questions.md (M5 scope ruling, open-questions 2026-07-11).
   */
  import { viewDetail, viewImageUrl, views as fetchViews } from '../lib/api/client';
  import type { ViewDetail } from '../lib/api/types';
  import { bandFor } from '../lib/bands';
  import CliHint from '../lib/components/CliHint.svelte';
  import { fmtAge, fmtSize, shortHash, snapShort } from '../lib/format';

  let views = $state<ViewDetail[] | null>(null);
  let error = $state<string | null>(null);

  $effect(() => {
    // The list body lacks endpoints + image (detail-only fields); view
    // counts are shelf-scale, so N detail fetches stay cheap.
    fetchViews()
      .then((body) => Promise.all(body.views.map((view) => viewDetail(view.name))))
      .then(
        (details) => (views = details),
        (e: unknown) => (error = e instanceof Error ? e.message : String(e)),
      );
  });

  /** One fold open per card: which CLI-hint/definition panel shows. */
  type Panel = 'image' | 'reeval' | 'diff' | 'sync' | 'grants' | 'definition';
  let open = $state<Record<string, Panel | undefined>>({});
  let menu = $state<Record<string, boolean>>({});
  let newViewHint = $state(false);
  /** `${name}:link` / `${name}:dav` — flips that label to `copied ✓`. */
  let copied = $state<string | null>(null);

  function toggle(name: string, panel: Panel) {
    open[name] = open[name] === panel ? undefined : panel;
  }

  /** The real action: absolute endpoint URL onto the clipboard. */
  async function copy(key: string, text: string) {
    await navigator.clipboard.writeText(text);
    copied = key;
    setTimeout(() => {
      if (copied === key) {
        copied = null;
      }
    }, 1400);
  }

  // 1G1R mode words (D57) are user-visible but lowercase — forced into
  // the catalog at statement level (EntryDrawer's residency pattern).
  // @wc-include
  const modeHeldFirst = 'held-first';
  // @wc-include
  const modeStrict = 'strict';
  // @wc-include
  const menuLabel = 'view actions';
</script>

<main>
  <div class="title-row">
    <!-- @wc-context: compiled shelf -->
    <h2>Views</h2>
    <span class="sub">compiled shelves, served read-only</span>
    <button class="new-view" onclick={() => (newViewHint = !newViewHint)}>
      <!-- @wc-context: compiled shelf -->+ new view
    </button>
  </div>
  {#if newViewHint}
    <!-- New-view creation entry was never designed (spec §8) and view
         definition is CLI-only anyway (M5 scope ruling, open-questions
         2026-07-11) — the button reveals the incantation. -->
    <div class="new-view-hint">
      <CliHint command={'datboi view define <name> <provider>/<system> … && datboi view eval <name>'}>
        view definition is CLI-only for now — define, then evaluate:
      </CliHint>
    </div>
  {/if}

  {#if error !== null}
    <!-- Undesigned loading/error states: plain mono in --faint. -->
    <p class="undesigned">something went wrong — {error}</p>
  {:else if views === null}
    <p class="undesigned">loading…</p>
  {:else if views.length === 0}
    <p class="undesigned">no views yet — define one and it lands here</p>
  {:else}
    <div class="grid">
      {#each views as view (view.name)}
        {@const def = view.definition}
        {@const hasImage = def?.image != null}
        {@const httpUrl = location.origin + view.endpoints.http}
        {@const panel = open[view.name]}
        <div class="card">
          <div class="band" style:background={bandFor(def?.system ?? view.name)}></div>
          <div class="body">
            <div class="head">
              <span class="name">{view.name}</span>
              <button
                class="menu-btn"
                aria-label={menuLabel}
                onclick={() => (menu[view.name] = !menu[view.name])}
              >
                ⋯
              </button>
            </div>
            {#if menu[view.name]}
              <div class="menu">
                <button
                  onclick={() => copy(`${view.name}:dav`, location.origin + view.endpoints.dav)}
                >
                  {#if copied === `${view.name}:dav`}copied ✓{:else}⎘ webdav url{/if}
                </button>
                <button onclick={() => toggle(view.name, 'sync')}>view-sync CLI</button>
                <button onclick={() => toggle(view.name, 'definition')}>definition</button>
                <button onclick={() => toggle(view.name, 'grants')}>access grants</button>
                <!-- "pin snapshot" (wireframe 2d) omitted: no CLI exists
                     to pin a snapshot, so there is nothing truthful to
                     hint (M5 scope ruling, open-questions 2026-07-11). -->
              </div>
            {/if}

            <div class="sub-line">
              {#if def !== null}
                {def.system}{#if def.one_g_one_r !== null}{' · '}1G1R
                  {def.one_g_one_r.mode === 'strict' ? modeStrict : modeHeldFirst}
                  ({def.one_g_one_r.regions.join('›')}){/if}{#if def.profile !== null}{' · '}{def.profile} profile{/if}
              {:else}
                served by tag only — no stored definition
              {/if}
            </div>

            <div class="stats">
              {#if view.rows !== undefined}
                <span>{view.rows.toLocaleString()} files</span>
              {/if}
              {#if view.snapshot !== null}
                <span>
                  snap {snapShort(view.snapshot)}{#if view.created_at !== undefined}{' · '}{fmtAge(
                      view.created_at,
                    )}{/if}
                </span>
              {:else}
                <span class="never">not evaluated yet</span>
              {/if}
              <!-- The spec's `4 missing` / `clean ●` status cell is
                   omitted: the API stores no eval report, so neither
                   claim would be truthful (M5 scope ruling,
                   open-questions 2026-07-11). -->
            </div>

            <div class="actions">
              {#if hasImage && view.image?.minted === true}
                <!-- Minted: a real download through the verified image
                     route (/v1/views/{name}/image). -->
                <a class="primary" href={viewImageUrl(view.name)} download={`${view.name}.img`}>
                  ⬇ Export SD image
                </a>
              {:else if hasImage}
                <!-- Image profile but nothing minted yet: minting is
                     CLI-only (mutating action), so reveal the verb. -->
                <button class="primary" onclick={() => toggle(view.name, 'image')}>
                  ⬇ Export SD image
                </button>
              {:else}
                <button class="primary" onclick={() => copy(`${view.name}:link`, httpUrl)}>
                  {#if copied === `${view.name}:link`}copied ✓{:else}⎘ copy link{/if}
                </button>
              {/if}
              <span class="links">
                <button class="link" onclick={() => toggle(view.name, 'reeval')}>re-eval</button>
                <span class="sep">·</span>
                <button class="link" onclick={() => toggle(view.name, 'diff')}>diff</button>
                <span class="sep">·</span>
                <!-- Real link into the served HTML listing, new tab. -->
                <a class="link" href={view.endpoints.http} target="_blank" rel="noopener">browse</a>
              </span>
            </div>

            {#if panel === 'image'}
              <CliHint command={`datboi view image ${view.name}`}>
                no image minted yet — minting is CLI-only, run:
              </CliHint>
            {:else if panel === 'reeval'}
              <CliHint command={`datboi view eval ${view.name}`}>
                evaluation is CLI-only for now — run:
              </CliHint>
            {:else if panel === 'diff'}
              <!-- Snapshot diff (spec §3.5) has no API and no CLI; the
                   dat-revision diff is the real command that exists
                   (M5 scope ruling, open-questions 2026-07-11). -->
              <CliHint
                command={`datboi dat diff ${def !== null ? `${def.provider}/${def.system}` : '<provider>/<system>'}`}
              >
                snapshot diff isn't built yet — the dat-revision diff is:
              </CliHint>
            {:else if panel === 'sync'}
              <CliHint command={`datboi view sync ${view.name} <target-dir>`}>
                SD sync is CLI-only for now — run:
              </CliHint>
            {:else if panel === 'grants'}
              <CliHint command={`datboi user grant <username> ${view.name}`}>
                grants live on the Admin screen, or via CLI:
              </CliHint>
            {:else if panel === 'definition'}
              <!-- The view editor (§3.4) is deferred: definition edits
                   are CLI-only in M5, so this fold shows the stored
                   definition read-only + the redefine incantation
                   (M5 scope ruling, open-questions 2026-07-11). -->
              <div class="def">
                {#if def !== null}
                  <div class="def-row">
                    <span class="k">source</span>
                    <span class="v">{def.provider}/{def.system}</span>
                  </div>
                  <div class="def-row">
                    <span class="k">layout</span>
                    <span class="v">{def.template}</span>
                  </div>
                  {#if def.one_g_one_r !== null}
                    <div class="def-row">
                      <span class="k">1G1R</span>
                      <span class="v">
                        {def.one_g_one_r.mode === 'strict' ? modeStrict : modeHeldFirst}
                        · {def.one_g_one_r.regions.join(' › ')}{#if def.one_g_one_r.langs.length > 0}{' · '}{def.one_g_one_r.langs.join(
                            ' › ',
                          )}{/if}
                      </span>
                    </div>
                  {/if}
                  {#if def.profile !== null}
                    <div class="def-row">
                      <span class="k">profile</span>
                      <span class="v">{def.profile}</span>
                    </div>
                  {/if}
                  {#if def.image !== null}
                    <div class="def-row">
                      <span class="k">image</span>
                      <span class="v">
                        FAT32 · {fmtSize(def.image.cluster_size)} clusters ·
                        {#if def.image.partition}MBR{:else}superfloppy{/if}{#if def.image.label !== null}{' · '}{def.image.label}{/if}
                      </span>
                    </div>
                  {/if}
                  {#if def.mame_mode !== null}
                    <div class="def-row">
                      <span class="k">mame</span>
                      <span class="v">{def.mame_mode}</span>
                    </div>
                  {/if}
                {:else}
                  <div class="def-row">
                    <span class="v">served by tag only — no stored definition</span>
                  </div>
                {/if}
                {#if view.snapshot !== null}
                  <div class="def-row">
                    <span class="k">snapshot</span>
                    <span class="v">{shortHash(view.snapshot)}</span>
                  </div>
                {/if}
                {#if view.image !== null}
                  {@const img = view.image}
                  <div class="def-row">
                    <span class="k">minted</span>
                    <span class="v">
                      {#if img.minted}
                        {shortHash(img.hash)}{#if img.bytes !== null}{' · '}{fmtSize(img.bytes)}{/if}
                      {:else}
                        not minted yet
                      {/if}
                    </span>
                  </div>
                {/if}
                <CliHint command={`datboi view define ${view.name} …`}>
                  editing is CLI-only for now — redefine, then re-evaluate:
                </CliHint>
              </div>
            {/if}

            {#if hasImage}
              <div class="foot foot-bad">⚠ flashing overwrites on-device saves</div>
            {:else}
              <div class="foot foot-faint">{httpUrl} · read-only</div>
            {/if}
          </div>
        </div>
      {/each}
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

  .new-view {
    all: unset;
    margin-left: auto;
    background: var(--ink);
    color: var(--bg);
    border-radius: var(--r-pill);
    padding: 7px 16px;
    font: 600 13px var(--font-display);
    cursor: pointer;
  }

  .new-view-hint {
    margin: -12px 0 18px;
  }

  .undesigned {
    font: 400 12.5px var(--font-data);
    color: var(--faint);
  }

  .grid {
    display: grid;
    grid-template-columns: 1fr 1fr;
    gap: 18px;
    align-items: start;
  }

  .card {
    background: var(--panel);
    border: 2px solid var(--ink);
    border-radius: var(--r-card);
    overflow: hidden;
    box-shadow: var(--shadow-card);
  }

  .band {
    height: 10px;
  }

  .body {
    padding: 18px 22px 20px;
    position: relative;
  }

  .head {
    display: flex;
    align-items: baseline;
    gap: 10px;
  }

  .name {
    font: 800 17px var(--font-display);
    letter-spacing: -0.02em;
    flex: 1;
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .menu-btn {
    all: unset;
    color: var(--faint);
    cursor: pointer;
    padding: 0 4px;
    font-size: 15px;
  }

  .menu {
    position: absolute;
    top: 40px;
    right: 18px;
    z-index: 1;
    display: flex;
    flex-direction: column;
    background: var(--panel);
    border: 2px solid var(--ink);
    border-radius: var(--r-sub);
    box-shadow: var(--shadow-card);
    overflow: hidden;
  }

  .menu button {
    all: unset;
    padding: 6px 14px;
    font: 500 12px var(--font-data);
    color: var(--mut);
    cursor: pointer;
    text-align: left;
  }

  .menu button:hover {
    background: var(--hover-row);
    color: var(--text);
  }

  .sub-line {
    margin-top: 4px;
    font: 400 12px var(--font-data);
    color: var(--faint);
  }

  .stats {
    display: flex;
    gap: 14px;
    margin-top: 10px;
    font: 500 12.5px var(--font-data);
    color: var(--mut);
  }

  .never {
    color: var(--faint);
  }

  .actions {
    display: flex;
    align-items: center;
    gap: 14px;
    margin-top: 16px;
  }

  .primary {
    all: unset;
    background: var(--ink);
    color: var(--bg);
    border-radius: var(--r-pill);
    padding: 8px 18px;
    font: 600 13px var(--font-display);
    cursor: pointer;
  }

  .links {
    font: 500 12px var(--font-data);
    color: var(--faint);
    display: inline-flex;
    gap: 6px;
    align-items: baseline;
  }

  .link {
    all: unset;
    font: 500 12px var(--font-data);
    color: var(--faint);
    cursor: pointer;
    text-decoration: none;
  }

  .link:hover {
    color: var(--text);
  }

  .sep {
    color: var(--faint);
  }

  .def {
    margin-top: 12px;
    padding-top: 10px;
    border-top: 1px dashed var(--hair);
  }

  .def-row {
    display: flex;
    gap: 8px;
    font: 400 12px var(--font-data);
    line-height: 2;
  }

  .def-row .k {
    width: 76px;
    flex: none;
    color: var(--faint);
  }

  .def-row .v {
    color: var(--mut);
    overflow-wrap: anywhere;
  }

  .foot {
    margin-top: 14px;
    font: 400 11.5px var(--font-data);
  }

  .foot-bad {
    color: var(--bad);
  }

  .foot-faint {
    color: var(--faint);
    overflow-wrap: anywhere;
  }
</style>
