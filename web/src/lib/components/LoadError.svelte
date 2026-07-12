<script lang="ts">
  /**
   * The undesigned error line, now with a way out: every first-load
   * failure used to be a dead end (recovery meant navigating away and
   * back). One component so every screen's retry looks and behaves the
   * same.
   */
  let { msg, onretry }: { msg: string; onretry: () => void } = $props();
</script>

<p class="undesigned">
  something went wrong — {msg}
  <button class="retry" onclick={onretry}>retry</button>
</p>

<style>
  .undesigned {
    margin: 0;
    font: 400 0.78125rem var(--font-data);
    color: var(--faint);
  }

  .retry {
    all: unset;
    cursor: pointer;
    margin-left: 6px;
    font: inherit;
    color: var(--mut);
    text-decoration: underline;
  }
</style>
