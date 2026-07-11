<script lang="ts">
  /**
   * Ingest (spec §3.6 — wireframes 1a/1b/1c, restyled with tokens).
   * There is no ingest API: ingest runs as a CLI process (M5 scope
   * ruling, open-questions 2026-07-11 — mutating pipeline actions are
   * CLI-only; the job registry that would carry live progress and the
   * durable step-2 report is a recorded open question, § "Jobs tray
   * backend"). So this screen is the design's step-1 structure as a
   * styled guide: the custody choice with its real consequences, and
   * the real invocation.
   */
  import CliHint from '../lib/components/CliHint.svelte';
</script>

<main>
  <div class="title-row">
    <h2>Ingest</h2>
    <span class="sub">hash and claim content into the store</span>
  </div>

  <div class="card">
    <div class="caps">WHERE &amp; HOW</div>
    <div class="custody">
      <div class="choice">
        <span class="glyph">◉</span>
        <span class="choice-copy">
          <!-- @wc-context: ingest custody -->copy — source untouched
        </span>
        <span class="tag">default</span>
      </div>
      <div class="choice">
        <span class="glyph faint">○</span>
        <span class="choice-copy bad">
          <!-- @wc-context: ingest custody -->move — destroys source layout
        </span>
        <!-- `--move` bails in cmds.rs ("not implemented yet"): D40
             custody needs a delete-after-durable hook the Ingester
             doesn't expose. The choice stays visible because it is the
             design's point; the state is stated honestly. -->
        <span class="tag">not implemented yet</span>
      </div>
    </div>
    <CliHint command={'datboi ingest <path>…'}>ingest is CLI-only for now — run:</CliHint>
  </div>

  <!-- Step 2 (the durable report card: new blobs · dupes · archive
       members · refused) needs the job registry — deferred with it
       (M5 scope ruling, open-questions 2026-07-11). -->
  <p class="note">
    progress and the report (new blobs · dupes · archive members · refused) print in the CLI today
    — they land here when the daemon grows a job registry
  </p>
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

  .card {
    max-width: 560px;
    background: var(--panel);
    border: 2px solid var(--ink);
    border-radius: var(--r-card);
    box-shadow: var(--shadow-card);
    padding: 18px 22px 20px;
  }

  .caps {
    font: 800 13px var(--font-display);
    letter-spacing: 0.02em;
    margin-bottom: 12px;
  }

  .custody {
    display: flex;
    flex-direction: column;
    gap: 8px;
    margin-bottom: 14px;
  }

  .choice {
    display: flex;
    align-items: baseline;
    gap: 10px;
    font: 400 12.5px var(--font-data);
    color: var(--mut);
  }

  .glyph {
    flex: none;
  }

  .glyph.faint {
    color: var(--faint);
  }

  .choice-copy.bad {
    color: var(--bad);
  }

  .tag {
    font: 400 10.5px var(--font-data);
    color: var(--dim);
  }

  .note {
    margin-top: 18px;
    max-width: 560px;
    font: 400 12px var(--font-data);
    color: var(--faint);
    line-height: 1.7;
  }
</style>
