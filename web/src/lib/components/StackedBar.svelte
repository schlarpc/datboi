<script lang="ts">
  /**
   * Stacked completeness bar (spec §1.4): verified green, claimed hatch,
   * empty remainder = missing + no-dump. Segments size against the FULL
   * total (lib/state.ts barSegments), unlike the headline percent.
   *
   * Two registers (spec §8 ruling): `shelf` = home cards, 12px with a
   * 2px ink frame; `bench` = audit header, 8px frameless on --panel2.
   */
  import { barSegments, type StateCounts } from '../state';

  let { counts, register = 'bench' }: { counts: StateCounts; register?: 'shelf' | 'bench' } =
    $props();

  const seg = $derived(barSegments(counts));
</script>

<div class="bar {register}">
  <div class="fill ok" style:width="{seg.verified}%"></div>
  <div class="fill claimed" style:width="{seg.claimed}%"></div>
</div>

<style>
  .bar {
    display: flex;
    overflow: hidden;
  }

  .bar.shelf {
    height: 12px;
    border: 2px solid var(--ink);
    border-radius: var(--r-bar-home);
    background: var(--track-home);
  }

  .bar.bench {
    height: 8px;
    border-radius: var(--r-bar);
    background: var(--panel2);
  }

  .fill {
    transition: width 0.4s;
  }

  .fill.ok {
    background: var(--ok);
  }

  .fill.claimed {
    background: var(--hatch-claimed);
  }
</style>
