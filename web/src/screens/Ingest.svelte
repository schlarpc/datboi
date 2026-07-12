<script lang="ts">
  /**
   * Ingest (spec §3.6 — wireframes 1a/1b/1c, restyled with tokens),
   * now the real thing: drop files, zips, or whole folders (or pick
   * them), watch each upload stream with a true byte progress bar,
   * then follow the background job (POST /v1/ingest → poll
   * GET /v1/jobs/{id}) to the step-2 report card — new blobs · dupes ·
   * archive members · refused. Dats are welcome too: the job
   * classifies each file by content (dat / zipped dat / ROM) and the
   * report carries a dats-imported lane.
   *
   * Custody over HTTP is always copy (D40's default): the browser
   * can't move your originals, only send copies. NAS-local ingest
   * (and the eventual --move) stays with the CLI.
   */
  import CliHint from '../lib/components/CliHint.svelte';
  import { ingestFlow } from '../lib/ingest.svelte';
  import { collectDrop, pickedFiles } from '../lib/upload';
  import { plural } from '../lib/plural';

  // The flow itself is app state (lib/ingest.svelte.ts) so navigation
  // can't orphan a multi-GB upload; this screen is a view over it.
  let fileInput = $state<HTMLInputElement | null>(null);
  let dirInput = $state<HTMLInputElement | null>(null);
  let dragOver = $state(false);
  const phase = $derived(ingestFlow.phase);
  const queue = $derived(ingestFlow.queue);
  const job = $derived(ingestFlow.job);
  const failure = $derived(ingestFlow.failure);
  const lostContact = $derived(ingestFlow.lostContact);

  const busy = $derived(ingestFlow.busy);
  const uploadedBytes = $derived(queue.reduce((sum, item) => sum + item.sent, 0));
  const totalBytes = $derived(queue.reduce((sum, item) => sum + item.size, 0));
  const refusedUploads = $derived(queue.filter((item) => item.state === 'failed'));
  const refusedCount = $derived(
    refusedUploads.length +
      (job === null
        ? 0
        : job.report.errors.length +
          job.report.member_skips.length +
          Number(job.report.skipper_skipped_large)),
  );

  function onDrop(e: DragEvent): void {
    e.preventDefault();
    dragOver = false;
    if (!busy && e.dataTransfer) {
      void collectDrop(e.dataTransfer).then((files) => ingestFlow.begin(files));
    }
  }

  function onPick(input: HTMLInputElement | null): void {
    if (input?.files && input.files.length > 0) {
      void ingestFlow.begin(pickedFiles(input.files));
      input.value = '';
    }
  }

  function pct(part: number, whole: number): number {
    return whole === 0 ? 0 : Math.floor((part / whole) * 100);
  }
</script>

<main>
  <div class="title-row">
    <h2>Ingest</h2>
    <span class="sub">hash and claim content into the store</span>
  </div>

  <input
    bind:this={fileInput}
    type="file"
    multiple
    hidden
    onchange={() => onPick(fileInput)}
  />
  <input
    bind:this={dirInput}
    type="file"
    webkitdirectory
    hidden
    onchange={() => onPick(dirInput)}
  />

  <button
    class="dropzone"
    class:drag={dragOver}
    disabled={busy}
    onclick={() => fileInput?.click()}
    ondragover={(e) => {
      e.preventDefault();
      dragOver = true;
    }}
    ondragleave={() => (dragOver = false)}
    ondrop={onDrop}
  >
    {#if phase === 'uploading'}
      <span>uploading… {pct(uploadedBytes, totalBytes)}%</span>
    {:else if phase === 'ingesting'}
      <span>ingesting…</span>
    {:else}
      <span>drop ROMs, zips, dats, or folders here — or click to pick files</span>
    {/if}
  </button>
  <p class="pick-folder">
    <button class="linkish" disabled={busy} onclick={() => dirInput?.click()}>
      …or pick a whole folder
    </button>
  </p>

  <!-- Upload progress card; the report supersedes it (upload failures
       re-appear there as refusals). -->
  {#if queue.length > 0 && phase !== 'report'}
    <div class="card queue">
      <div class="caps">UPLOADS</div>
      <ul>
        {#each queue as item (item.name)}
          <li>
            <span class="file">{item.name}</span>
            {#if item.state === 'failed'}
              <span class="bad">{item.error}</span>
            {:else}
              <span class="track"><span
                  class="fill"
                  style:width="{pct(item.sent, item.size)}%"
                ></span></span>
              <span class="pct-label">
                {#if item.state === 'staged'}
                  <!-- @wc-context: upload state -->staged ✓
                {:else if item.state === 'uploading'}
                  {pct(item.sent, item.size)}%
                {:else}
                  <!-- @wc-context: upload state -->queued
                {/if}
              </span>
            {/if}
          </li>
        {/each}
      </ul>
    </div>
  {/if}

  {#if phase === 'ingesting' && job !== null}
    <div class="card">
      <div class="caps">INGESTING</div>
      <p class="progress-line">
        {job.files_done} / {plural(job.files_total, ['# file', '# files'])}
        {#if job.current !== undefined}
          — processing {job.current}…
        {/if}
      </p>
      <span class="track wide"><span class="fill" style:width="{job.progress}%"></span></span>
    </div>
  {/if}

  {#if phase === 'report'}
    <div class="card">
      <div class="caps">REPORT</div>
      {#if lostContact}
        <p class="bad">lost contact with the job — it may still be running; check the jobs tray</p>
      {/if}
      {#if failure !== null}
        <p class="bad">something went wrong — {failure}</p>
      {/if}
      {#if job !== null && job.matched_total > 0}
        <!-- The user-vocabulary half: which GAMES this run newly
             satisfied — the shelf lights, above the pipeline counts. -->
        <p class="matched-head">
          <b>{job.matched_total.toLocaleString()}</b> matched
        </p>
        <ul class="matched">
          {#each job.matched as m, i (i)}
            <li><span class="file">{m.name}</span> <span class="source">{m.source}</span></li>
          {/each}
          {#if job.matched_total > job.matched.length}
            <li class="more">
              …and {(job.matched_total - job.matched.length).toLocaleString()} more
            </li>
          {/if}
        </ul>
      {/if}
      {#if job !== null && job.report.dats_imported.length > 0}
        <!-- The dat lane: files the job classified (by content) as
             dats — loose or zipped — and imported instead of
             ingesting. Same register as the matched list. -->
        <p class="matched-head">
          <b>{job.report.dats_imported.length.toLocaleString()}</b>
          {plural(job.report.dats_imported.length, ['dat imported', 'dats imported'])}
        </p>
        <ul class="matched">
          {#each job.report.dats_imported as d, i (i)}
            <li>
              <span class="file">{d.path}</span>
              <span class="source">{d.provider}/{d.system} — {plural(d.entries, ['# entry', '# entries'])}</span>
            </li>
          {/each}
        </ul>
      {/if}
      {#if job !== null}
        <div class="counts">
          <span>
            <b>{job.report.files_stored.toLocaleString()}</b>
            {plural(job.report.files_stored, ['new blob', 'new blobs'])}
          </span>
          <span>
            <b>{(job.report.files_already_present + job.report.files_unchanged).toLocaleString()}</b>
            dupes
          </span>
          <span>
            <b>{(job.report.members_claimed + job.report.members_extracted).toLocaleString()}</b>
            {plural(job.report.members_claimed + job.report.members_extracted, ['archive member', 'archive members'])}
          </span>
          <span class:bad={refusedCount > 0}>
            <b>{refusedCount.toLocaleString()}</b>
            <!-- @wc-context: files the pipeline would not take -->refused
          </span>
        </div>
        {#if job.report.members_claimed + job.report.members_extracted > 0}
          <p class="note">
            {job.report.members_claimed.toLocaleString()} claimed in place ·
            {job.report.members_extracted.toLocaleString()} extracted
          </p>
        {/if}
      {/if}
      {#if refusedUploads.length > 0 || (job !== null && (job.report.errors.length > 0 || job.report.member_skips.length > 0))}
        <ul class="refusals">
          {#each refusedUploads as item (item.name)}
            <li><span class="file">{item.name}</span> <span class="why">{item.error}</span></li>
          {/each}
          {#if job !== null}
            {#each job.report.errors as e, i (i)}
              <li><span class="file">{e.path}</span> <span class="why">{e.error}</span></li>
            {/each}
            {#each job.report.member_skips as s, i (i)}
              <li>
                <span class="file">{s.path} :: {s.member}</span>
                <span class="why">{s.reason}</span>
              </li>
            {/each}
          {/if}
        </ul>
      {/if}
      {#if job !== null && job.report.notes.length > 0}
        <ul class="notes">
          {#each job.report.notes as note, i (i)}
            <li>{note}</li>
          {/each}
        </ul>
      {/if}
    </div>
  {/if}

  <!-- Custody (spec §3.6 step 1), now honest for the web path: an
       upload IS a copy — the browser cannot move the originals. The
       CLI keeps NAS-local ingest (and the eventual D40 --move). -->
  <p class="note">
    uploads are copies — your originals stay where they are. for content already on the NAS,
    ingest in place instead:
  </p>
  <CliHint command={'datboi ingest <path>…'}>NAS-local ingest runs in the CLI:</CliHint>
</main>

<style>
  main {
    flex: 1;
    overflow-y: auto;
    padding: 24px var(--pad-x) 30px;
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

  .dropzone {
    all: unset;
    display: block;
    box-sizing: border-box;
    width: 100%;
    max-width: 560px;
    border: 2px dashed var(--dim);
    border-radius: var(--r-card);
    padding: 26px 16px;
    text-align: center;
    color: var(--faint);
    font: 600 0.8125rem var(--font-display);
    cursor: pointer;
  }

  .dropzone.drag {
    border-color: var(--ink);
    color: var(--text);
    background: var(--panel);
  }

  .dropzone:disabled {
    cursor: progress;
  }

  .pick-folder {
    margin: 8px 0 0;
    max-width: 560px;
    text-align: center;
  }

  .linkish {
    all: unset;
    font: 500 0.75rem var(--font-data);
    color: var(--faint);
    text-decoration: underline;
    cursor: pointer;
  }

  .linkish:disabled {
    cursor: progress;
  }

  .card {
    max-width: 560px;
    margin-top: 18px;
    background: var(--panel);
    border: 2px solid var(--ink);
    border-radius: var(--r-card);
    box-shadow: var(--shadow-card);
    padding: 18px 22px 20px;
  }

  .caps {
    font: 800 0.8125rem var(--font-display);
    letter-spacing: 0.02em;
    margin-bottom: 12px;
  }

  .queue ul,
  .matched,
  .refusals,
  .notes {
    list-style: none;
    margin: 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: 6px;
  }

  .queue li,
  .matched li,
  .refusals li {
    display: flex;
    align-items: center;
    gap: 10px;
    font: 400 0.75rem var(--font-data);
    color: var(--mut);
  }

  .file {
    font-weight: 600;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    max-width: 55%;
  }

  .track {
    flex: 1;
    height: 5px;
    border-radius: var(--r-fill);
    background: var(--panel2);
    overflow: hidden;
  }

  .track.wide {
    display: block;
    margin-top: 10px;
  }

  .fill {
    display: block;
    height: 100%;
    background: var(--ok);
    transition: width 0.3s linear;
  }

  .pct-label {
    flex: none;
    font: 500 0.6875rem var(--font-data);
    color: var(--faint);
    min-width: 56px;
    text-align: right;
  }

  .progress-line {
    margin: 0;
    font: 400 0.78125rem var(--font-data);
    color: var(--mut);
  }

  .counts {
    display: flex;
    flex-wrap: wrap;
    gap: 16px;
    font: 400 0.78125rem var(--font-data);
    color: var(--mut);
  }

  .counts b {
    font: 800 0.9375rem var(--font-display);
    color: var(--text);
  }

  .bad {
    color: var(--bad);
  }

  .counts .bad b {
    color: var(--bad);
  }

  .why {
    color: var(--faint);
  }

  .matched-head {
    margin: 0 0 10px;
    font: 400 0.78125rem var(--font-data);
    color: var(--mut);
  }

  .matched-head b {
    font: 800 0.9375rem var(--font-display);
    color: var(--text);
  }

  .matched {
    margin-bottom: 16px;
  }

  .source,
  .more {
    color: var(--faint);
  }

  .refusals {
    margin-top: 12px;
  }

  .notes {
    margin-top: 12px;
    font: 400 0.71875rem var(--font-data);
    color: var(--faint);
  }

  .note {
    margin-top: 18px;
    max-width: 560px;
    font: 400 0.75rem var(--font-data);
    color: var(--faint);
    line-height: 1.7;
  }

  @media (max-width: 720px) {
    .title-row {
      flex-wrap: wrap;
      gap: 4px 14px;
    }
  }
</style>
