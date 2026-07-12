<script lang="ts">
  /**
   * Shelves home (spec §4.2) — where friends land. Full cartridge
   * register: whole-card click into browse, hover translate, band in
   * the view's color, and the verified promise stated on every card
   * ("personality at the shelf"). /v1/views is already ACL-filtered
   * server-side (D68), so this renders exactly the caller's grants.
   */
  import { views as fetchViews } from '../lib/api/client';
  import type { View } from '../lib/api/types';
  import { bandFor } from '../lib/bands';
  import { fmtAge, fmtSize, snapShort } from '../lib/format';
  import { router } from '../lib/router.svelte';
  import { errorText } from '../lib/remote';

  let views = $state<View[] | null>(null);
  let error = $state<string | null>(null);

  $effect(() => {
    fetchViews().then(
      (body) => (views = body.views),
      (e: unknown) => (error = errorText(e)),
    );
  });

  function open(name: string) {
    router.navigate(`/shelf/${encodeURIComponent(name)}`);
  }
</script>

<main>
  <div class="title-row">
    <h2>Your shelves</h2>
    <span class="sub">shared with you by the owner</span>
  </div>

  {#if error !== null}
    <!-- Undesigned loading/error states: plain mono in --faint. -->
    <p class="undesigned">something went wrong — {error}</p>
  {:else if views === null}
    <p class="undesigned">loading…</p>
  {:else if views.length === 0}
    <p class="undesigned">nothing on your shelves yet — ask the owner for a grant</p>
  {:else}
    <div class="grid">
      {#each views as view (view.name)}
        {@const def = view.definition}
        <div
          class="card"
          onclick={() => open(view.name)}
          onkeydown={(e) => {
            // @wc-ignore
            if (e.key === 'Enter') open(view.name);
          }}
          role="link"
          tabindex="0"
        >
          <div class="band" style:background={bandFor(def?.system ?? view.name)}></div>
          <div class="body">
            <div class="head">
              <span class="name">{view.name}</span>
              <span class="browse">browse →</span>
            </div>
            <div class="sub-line">
              {#if def !== null}
                {def.system}{' · '}{def.provider}{#if def.profile !== null}{' · '}{def.profile} layout{/if}
              {:else}
                <!-- Tag-only views carry no definition; say nothing false. -->
                served read-only
              {/if}
            </div>
            <div class="stats">
              {#if view.rows != null && view.bytes != null}
                <span>{view.rows.toLocaleString()} files · {fmtSize(view.bytes)}</span>
              {/if}
              {#if view.snapshot !== null}
                <span>
                  snap {snapShort(view.snapshot)}{#if view.created_at != null}{' · '}{fmtAge(
                      view.created_at,
                    )} ago{/if}
                </span>
              {/if}
            </div>
            <div class="trust">● verified — hash-checked as it streams</div>
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
  }

  .title-row,
  .grid,
  .undesigned {
    max-width: 960px;
    margin: 0 auto;
    padding: 0 var(--pad-x);
    box-sizing: border-box;
  }

  .title-row {
    display: flex;
    align-items: baseline;
    gap: 14px;
    padding-top: 32px;
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
    padding-bottom: 32px;
  }

  .card {
    background: var(--panel);
    border: 2px solid var(--ink);
    border-radius: var(--r-card);
    overflow: hidden;
    box-shadow: var(--shadow-card);
    cursor: pointer;
    transition: transform 0.08s ease;
  }

  .card:hover {
    transform: translate(-1px, -1px);
  }

  .band {
    height: 10px;
  }

  .body {
    padding: 18px 22px 20px;
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

  .browse {
    font: 600 13px var(--font-data);
    color: var(--faint);
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

  .trust {
    margin-top: 12px;
    font: 400 11.5px var(--font-data);
    color: var(--okT);
  }

  @media (max-width: 640px) {
    .grid {
      grid-template-columns: 1fr;
      gap: 14px;
    }

    .body {
      padding: 16px 18px 18px;
    }
  }
</style>
