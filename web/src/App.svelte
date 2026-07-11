<script lang="ts">
  import { loadLocale } from 'wuchale/load-utils';
  // Side-effect import: registers the wuchale catalog loaders (generated at
  // startup into src/locales/, gitignored) before the first loadLocale.
  import './locales/main.loader.svelte.js';
  import { STATE_GLYPHS } from './lib/state';

  // Locale is app-level state; a real switcher arrives with the M5 screens.
  let locale = $state('en');
</script>

{#await loadLocale(locale)}
  <!-- Rendered before the catalog is loaded, so untranslatable by design. -->
  <!-- @wc-ignore -->
  <p class="loading">Loading translations…</p>
{:then}
  <header>
    <span class="logo-disc"></span>
    <!-- The wordmark is the brand, not copy. -->
    <!-- @wc-ignore -->
    <span class="wordmark">datboi</span>
    <nav>
      <!-- "view" = compiled shelf (D33), not a UI view (D67 contexts). -->
      <!-- @wc-context: compiled shelf -->
      <span class="nav-item">Views</span>
    </nav>
  </header>
  <main>
    <h2>The shelf</h2>
    <p class="sub">dat/rom management on content-addressed storage</p>
    <!--
      The four-state legend (spec §1.4). The state WORDS collide with everyday
      English senses ("claimed" is a storage state, not a person's claim), so
      each carries `@wc-context: storage state` — a real gettext msgctxt in
      the PO catalog (D67). The meaning copy is unambiguous prose: no context.
    -->
    <ul class="legend">
      <li>
        <span class="dot dot--verified"></span>
        <span class="glyph">{STATE_GLYPHS.verified}</span>
        <span class="state-word state-text--verified">
          <!-- @wc-context: storage state -->verified
        </span>
        <span class="meaning">bytes on hand, hash checked against the catalog</span>
      </li>
      <li>
        <span class="dot dot--claimed"></span>
        <span class="glyph">{STATE_GLYPHS.claimed}</span>
        <span class="state-word state-text--claimed">
          <!-- @wc-context: storage state -->claimed
        </span>
        <span class="meaning">bytes rebuildable, not yet re-verified</span>
      </li>
      <li>
        <span class="dot dot--missing"></span>
        <span class="glyph">{STATE_GLYPHS.missing}</span>
        <span class="state-word state-text--missing">
          <!-- @wc-context: storage state -->missing
        </span>
        <span class="meaning">no blob or claim names this hash</span>
      </li>
      <li>
        <span class="dot dot--nodump"></span>
        <span class="glyph">{STATE_GLYPHS.nodump}</span>
        <span class="state-word state-text--nodump">
          <!-- @wc-context: storage state -->no dump
        </span>
        <span class="meaning">the catalog marks this entry as never dumped — nothing to have</span>
      </li>
    </ul>
  </main>
{/await}

<style>
  header {
    display: flex;
    align-items: center;
    gap: 10px;
    padding: 10px 28px;
    border-bottom: 2px solid var(--ink);
    background: var(--panel);
  }

  .logo-disc {
    width: 18px;
    height: 18px;
    border-radius: 50%;
    background: var(--ok);
    border: 2px solid var(--ink);
  }

  .wordmark {
    font: 800 15px var(--font-display);
    letter-spacing: -0.02em;
  }

  nav {
    margin-left: auto;
  }

  .nav-item {
    font-size: 13px;
    font-weight: 600;
    color: var(--faint);
  }

  main {
    padding: 26px 28px;
  }

  h2 {
    margin: 0;
    font: 800 24px var(--font-display);
    letter-spacing: -0.03em;
  }

  .sub {
    margin: 4px 0 20px;
    font: 400 12.5px var(--font-data);
    color: var(--faint);
  }

  .legend {
    margin: 0;
    padding: 0;
    list-style: none;
  }

  .legend li {
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 9px 0;
    border-bottom: 1px solid var(--rule);
  }

  .glyph,
  .state-word {
    font: 400 11.5px var(--font-data);
  }

  .meaning {
    font: 400 12px var(--font-data);
    color: var(--mut);
  }

  .loading {
    font: 400 12.5px var(--font-data);
    color: var(--faint);
    padding: 26px 28px;
  }
</style>
