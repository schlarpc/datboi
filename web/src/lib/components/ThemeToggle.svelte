<script lang="ts">
  /** 3-state theme toggle, spec §2.3: segmented pill `sys / ☀ / ☾`. */
  import { prefs, type Theme } from '../prefs.svelte';

  // ☀/☾ are glyphs (untranslatable by content); "sys" is markup text and
  // extracted below.
  const glyphs: Record<Exclude<Theme, 'system'>, string> = { light: '☀', dark: '☾' };
</script>

<div class="toggle" role="group" aria-label="theme">
  <button
    class="seg"
    class:active={prefs.theme === 'system'}
    onclick={() => prefs.setTheme('system')}
  >
    sys
  </button>
  {#each ['light', 'dark'] as const as theme (theme)}
    <button class="seg" class:active={prefs.theme === theme} onclick={() => prefs.setTheme(theme)}>
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
