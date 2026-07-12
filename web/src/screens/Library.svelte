<script lang="ts">
  /**
   * Library home — "The shelf" (spec §3.1, hi-fi 4c). System cards from
   * GET /v1/systems in the full cartridge register: band, 2px ink,
   * offset shadow, stacked bar with ink frame, views chips.
   */
  import { startIngest, systems as fetchSystems, uploadRom } from '../lib/api/client';
  import type { System } from '../lib/api/types';
  import { bandFor } from '../lib/bands';
  import Link from '../lib/components/Link.svelte';
  import StackedBar from '../lib/components/StackedBar.svelte';
  import { followJob, jobsSignal, LostContact } from '../lib/jobs.svelte';
  import { router } from '../lib/router.svelte';
  import { completenessPct } from '../lib/state';
  import { errorText } from '../lib/remote';
  import { plural } from '../lib/plural';
  import LoadError from '../lib/components/LoadError.svelte';

  let systems = $state<System[] | null>(null);
  let error = $state<string | null>(null);

  // Dat import rides the unified ingest flow (the job classifies each
  // file by content — dat, zipped dat, or ROM — so zipped dats just
  // work): stage each file, start one job, poll it to done, then read
  // the receipts from the report's dats-imported and errors lanes.
  // The card stays compact — "importing <name>…" is enough here, the
  // jobs tray carries the progress bar.
  let fileInput = $state<HTMLInputElement | null>(null);
  let dragOver = $state(false);
  let importing = $state<string | null>(null);
  let imports = $state<{ name: string; ok: boolean; detail: string }[]>([]);

  // Unmount stops the follow loop — the tray owns job visibility; a
  // destroyed screen must not keep a private poll running for hours.
  let alive = true;
  $effect(() => () => {
    alive = false;
  });

  /** Bumped by the error line's retry — the load effect re-runs. */
  let attempt = $state(0);
  $effect(() => {
    void attempt;
    error = null;
    fetchSystems().then(
      (body) => (systems = body.systems),
      (e: unknown) => (error = errorText(e)),
    );
  });

  async function importFiles(files: FileList): Promise<void> {
    const staged: string[] = [];
    for (const file of Array.from(files)) {
      importing = file.name;
      try {
        staged.push((await uploadRom(file.name, file)).upload);
      } catch (e) {
        imports.push({
          name: file.name,
          ok: false,
          detail: errorText(e),
        });
      }
    }
    if (staged.length > 0) {
      try {
        const started = await startIngest(staged);
        jobsSignal.bump(); // wake the tray now, not on its own cadence
        const job = await followJob(started.job, {
          alive: () => alive,
          onUpdate: (detail) => (importing = detail.current ?? importing),
        });
        if (job === null) return; // unmounted mid-job: the tray takes over
        jobsSignal.bump(); // flip the tray row to done promptly
        for (const dat of job.report.dats_imported) {
          imports.push({
            name: dat.path,
            ok: true,
            detail: `${dat.provider}/${dat.system} — ${dat.entries.toLocaleString()} entries`,
          });
        }
        for (const refusal of job.report.errors) {
          imports.push({ name: refusal.path, ok: false, detail: refusal.error });
        }
        if (job.error != null) {
          imports.push({ name: 'ingest', ok: false, detail: job.error });
        }
        // The shelf just changed (new source, or a source's current
        // revision flipped) — re-fetch rather than guess the rollups.
        systems = (await fetchSystems()).systems;
      } catch (e) {
        imports.push({
          name: 'ingest',
          ok: false,
          detail:
            e instanceof LostContact
              ? `lost contact with the job — it may still be running (${e.message})`
              : e instanceof Error
                ? e.message
                : String(e),
        });
      }
    }
    importing = null;
  }

  function onDrop(e: DragEvent): void {
    e.preventDefault();
    dragOver = false;
    if (importing === null && e.dataTransfer && e.dataTransfer.files.length > 0) {
      void importFiles(e.dataTransfer.files);
    }
  }

  function onPick(): void {
    if (fileInput?.files && fileInput.files.length > 0) {
      void importFiles(fileInput.files);
      fileInput.value = '';
    }
  }

  const entryTotal = $derived((systems ?? []).reduce((sum, sys) => sum + sys.total, 0));

  /** Completeness color per the comps: green ≥90, amber below. */
  function pctClass(pct: number): string {
    return pct >= 90 ? 'pct-ok' : 'pct-warn';
  }
</script>

<svelte:window
  onbeforeunload={(e) => {
    // Tab close mid-import kills the upload for real — prompt first.
    if (importing !== null) e.preventDefault();
  }}
/>
<main>
  <div class="title-row">
    <h2>The shelf</h2>
    {#if systems !== null}
      <span class="sub">
        {plural(systems.length, ['# system', '# systems'])} · {plural(entryTotal, ['# entry', '# entries'])}
      </span>
    {/if}
  </div>

  {#if error !== null}
    <!-- Loading/error states have no design (spec has none) — plain
         mono text in --faint until they get one. -->
    <LoadError msg={error} onretry={() => (attempt += 1)} />
  {:else if systems === null}
    <p class="undesigned">loading…</p>
  {:else}
    <div class="grid">
      {#each systems as sys (sys.id)}
        {@const pct = completenessPct(sys.counts)}
        <!-- Whole card opens the audit drill-down (spec §3.1); a div
             with role=link because a real <a> can't nest the view-chip
             links and <article> refuses the interactive role. -->
        <div
          class="card"
          onclick={() => router.navigate(`/library/${sys.id}`)}
          onkeydown={(e) => {
            // @wc-ignore
            if (e.key === 'Enter') router.navigate(`/library/${sys.id}`);
          }}
          role="link"
          tabindex="0"
        >
          <div class="band" style:background={bandFor(sys.system)}></div>
          <div class="body">
            <div class="head">
              <span class="name">{sys.system}</span>
              <!-- Truncated, full text on hover: header-derived provider
                   strings can be arbitrarily long and must not wreck the
                   card (a No-Intro author roll call is ~700 chars). -->
              <span
                class="rev"
                title={sys.revision?.version ? `${sys.provider} ${sys.revision.version}` : sys.provider}
              >
                <!-- Revision is data (provider label + dat version). -->
                {sys.provider}{#if sys.revision?.version}&nbsp;{sys.revision.version}{/if}
              </span>
              <span class="pct {pctClass(pct)}">{pct}%</span>
            </div>
            <StackedBar counts={sys.counts} register="shelf" />
            <div class="counts">
              <!-- @wc-context: storage state -->
              <span>{sys.counts.verified.toLocaleString()} verified</span>
              <!-- @wc-context: storage state -->
              <span>{sys.counts.claimed.toLocaleString()} claimed</span>
              <!-- @wc-context: storage state -->
              <span class="bad">{sys.counts.missing.toLocaleString()} missing</span>
            </div>
            {#if sys.views.length > 0}
              <div class="chips">
                {#each sys.views as view (view)}
                  <!-- Chips open the view — the Views screen for now;
                       per-view pages are a later task. -->
                  <Link
                    href="/views"
                    class="chip"
                    onclick={(e: MouseEvent) => e.stopPropagation()}
                  >
                    {view}
                  </Link>
                {/each}
              </div>
            {/if}
          </div>
        </div>
      {/each}
    </div>

    <!-- Hidden picker behind the card; `accept` is a hint, not a
         gate — the server's format detection is the real judge. -->
    <input
      bind:this={fileInput}
      type="file"
      accept=".dat,.xml,.zip"
      multiple
      hidden
      onchange={onPick}
    />
    <button
      class="empty-card"
      class:drag={dragOver}
      disabled={importing !== null}
      onclick={() => fileInput?.click()}
      ondragover={(e) => {
        e.preventDefault();
        dragOver = true;
      }}
      ondragleave={() => (dragOver = false)}
      ondrop={onDrop}
    >
      {#if importing !== null}
        <span>importing {importing}…</span>
      {:else}
        <span>+ import a dat (zipped is fine) to start a new system — drop files here or click to pick</span>
      {/if}
    </button>
    {#if imports.length > 0}
      <ul class="import-log">
        {#each imports as result, i (i)}
          <li class:bad={!result.ok}>
            {#if result.ok}
              <!-- @wc-context: dat import result -->
              <span>imported</span>
            {:else}
              <!-- @wc-context: dat import result -->
              <span class="bad">refused</span>
            {/if}
            <span class="file">{result.name}</span>
            <span class="detail">{result.detail}</span>
          </li>
        {/each}
      </ul>
    {/if}
  {/if}
</main>

<style>
  main {
    flex: 1;
    overflow-y: auto;
    padding: 26px var(--pad-x) 30px;
  }

  .title-row {
    display: flex;
    align-items: baseline;
    gap: 14px;
    margin-bottom: 22px;
  }

  h2 {
    margin: 0;
    font: 800 1.5rem var(--font-display);
    letter-spacing: -0.03em;
  }

  .sub {
    font: 400 0.8125rem var(--font-data);
    color: var(--faint);
  }

  .undesigned {
    font: 400 0.78125rem var(--font-data);
    color: var(--faint);
  }

  .grid {
    display: grid;
    grid-template-columns: 1fr 1fr;
    gap: 18px;
  }

  .card {
    background: var(--panel);
    border: 2px solid var(--ink);
    border-radius: var(--r-card);
    overflow: hidden;
    box-shadow: var(--shadow-card);
    cursor: pointer;
  }

  .band {
    height: 12px;
  }

  .body {
    padding: 18px 22px 20px;
  }

  .head {
    display: flex;
    align-items: baseline;
    gap: 10px;
    margin-bottom: 14px;
  }

  .name {
    font: 800 1.0625rem var(--font-display);
    letter-spacing: -0.02em;
  }

  .rev {
    font: 400 0.75rem var(--font-data);
    color: var(--faint);
    /* One line, ellipsized — the pct must stay on the card no matter
       how long the header-derived provider string is. */
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .pct {
    margin-left: auto;
    font: 800 1.125rem var(--font-display);
  }

  .pct-ok {
    color: var(--okT);
  }

  .pct-warn {
    color: var(--warnT);
  }

  .counts {
    display: flex;
    gap: 14px;
    margin-top: 10px;
    font: 500 0.75rem var(--font-data);
    color: var(--mut);
  }

  .counts .bad {
    color: var(--bad);
  }

  .chips {
    display: flex;
    flex-wrap: wrap;
    gap: 8px;
    margin-top: 14px;
    padding-top: 12px;
    border-top: 1px solid var(--rule);
  }

  .chips :global(a.chip) {
    font: 500 0.75rem var(--font-data);
    background: color-mix(in srgb, var(--ok) 10%, var(--panel));
    border: 1.5px solid var(--ink);
    border-radius: var(--r-pill);
    padding: 2px 10px;
    color: var(--text);
    text-decoration: none;
  }

  .empty-card {
    all: unset;
    display: block;
    box-sizing: border-box;
    width: 100%;
    margin-top: 18px;
    border: 2px dashed var(--dim);
    border-radius: var(--r-card);
    padding: 16px;
    text-align: center;
    color: var(--faint);
    font: 600 0.8125rem var(--font-display);
    cursor: pointer;
  }

  .empty-card.drag {
    border-color: var(--ink);
    color: var(--text);
    background: var(--panel);
  }

  .empty-card:disabled {
    cursor: progress;
  }

  .import-log {
    list-style: none;
    margin: 12px 0 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: 4px;
  }

  .import-log li {
    display: flex;
    gap: 10px;
    font: 400 0.75rem var(--font-data);
    color: var(--mut);
  }

  .import-log .bad {
    color: var(--bad);
  }

  .import-log .file {
    font-weight: 600;
  }

  .import-log .detail {
    color: var(--faint);
  }

  /* One card per row once two won't fit comfortably; the card body also
     loosens its side padding to match the tighter shell gutter. */
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
