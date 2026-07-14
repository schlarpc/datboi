<script lang="ts">
  /**
   * Stacked completeness bar (spec §1.4): verified green, claimed hatch,
   * empty remainder = missing. Segments size against the obtainable
   * total (lib/state.ts barSegments — no-dump excluded, same
   * denominator as the headline percent).
   *
   * Two registers (spec §8 ruling): `shelf` = home cards, 12px with a
   * 2px ink frame; `bench` = audit header, 8px frameless on --panel2.
   */
  import { barSegments, type StateCounts } from '../state';
  import { plural } from '../plural';

  let { counts, register = 'bench' }: { counts: StateCounts; register?: 'shelf' | 'bench' } =
    $props();

  const seg = $derived(barSegments(counts));

  // Hover text: the numbers behind each band (web-ui.md: color is
  // never the only legend).
  const verifiedTitle = $derived(plural(counts.verified, ['# verified', '# verified']));
  const claimedTitle = $derived(plural(counts.claimed, ['# claimed', '# claimed']));
  const missingTitle = $derived(plural(counts.missing, ['# missing', '# missing']));
</script>

<div
  class="bar {register}"
  role="img"
  aria-label="{verifiedTitle} · {claimedTitle} · {missingTitle}"
>
  <div class="fill ok" style:width="{seg.verified}%" title={verifiedTitle}></div>
  <div class="fill claimed" style:width="{seg.claimed}%" title={claimedTitle}></div>
  <div class="fill rest" title={missingTitle}></div>
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

  .fill.rest {
    flex: 1;
    background: transparent;
  }
</style>
