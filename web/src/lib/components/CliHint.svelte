<script lang="ts">
  /**
   * CLI-hint reveal body (M5 scope ruling, open-questions 2026-07-11):
   * mutating pipeline actions are CLI-only in M5, so screens render the
   * design's action slots but the act itself is a revealed incantation.
   * Extracted from Library's "+ import a dat" card, which established
   * the pattern. The note text is the caller's children (extracted for
   * i18n at the call site); the command is data — verified against the
   * real clap definitions in crates/datboi/src/main.rs, never translated.
   */
  import type { Snippet } from 'svelte';

  let { command, children }: { command: string; children?: Snippet } = $props();
</script>

<span class="cli-hint">
  <span>{@render children?.()}</span>
  <code>{command}</code>
</span>

<style>
  .cli-hint {
    display: block;
    margin-top: 8px;
    font: 400 0.71875rem var(--font-data);
    color: var(--mut);
    line-height: 1.6;
  }

  code {
    font-family: var(--font-data);
    color: var(--text);
    overflow-wrap: anywhere;
  }
</style>
