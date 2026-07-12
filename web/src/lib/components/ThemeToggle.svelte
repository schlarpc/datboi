<script lang="ts">
  /** 3-state theme toggle, spec §2.3: segmented pill `sys / ☀ / ☾`. */
  import { prefs, type Theme } from '../prefs.svelte';

  // ☀/☾ are glyphs (untranslatable by content); "sys" is markup text and
  // extracted below. The glyph buttons carry catalog-routed names, and
  // every segment exposes its selection as aria-pressed — the .active
  // background is invisible to a screen reader.
  const glyphs: Record<Exclude<Theme, 'system'>, string> = { light: '☀', dark: '☾' };
  // @wc-include
  const groupLabel = 'theme';
  // @wc-include
  const lightLabel = 'light theme';
  // @wc-include
  const darkLabel = 'dark theme';
</script>

<div class="toggle" role="group" aria-label={groupLabel}>
  <button
    class="seg"
    class:active={prefs.theme === 'system'}
    aria-pressed={prefs.theme === 'system'}
    onclick={() => prefs.setTheme('system')}
  >
    sys
  </button>
  {#each ['light', 'dark'] as const as theme (theme)}
    <button
      class="seg"
      class:active={prefs.theme === theme}
      aria-pressed={prefs.theme === theme}
      aria-label={theme === 'light' ? lightLabel : darkLabel}
      onclick={() => prefs.setTheme(theme)}
    >
      {glyphs[theme]}
    </button>
  {/each}
</div>

<style>
  .toggle {
    display: inline-flex;
    border: 1.5px solid var(--hair);
    border-radius: var(--r-pill);
    overflow: hidden;
    font: 500 11px var(--font-data);
  }

  .seg {
    all: unset;
    padding: 3px 9px;
    cursor: pointer;
    color: var(--faint);
  }

  .seg.active {
    background: var(--ink);
    color: var(--bg);
    font-weight: 600;
  }
</style>
