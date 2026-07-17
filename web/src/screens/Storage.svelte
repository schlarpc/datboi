<script lang="ts">
  /**
   * Storage subpage (spec §3.7 — wireframe 2b, restyled with tokens).
   * The stat tiles are REAL from /v1/storage; the savings % is computed
   * client-side (the API ships raw byte counts only).
   *
   * Action cards:
   * - Scrub: last-run line reads the D74 job ledger (CLI runs stamp
   *   terminal rows); the CLI hint stays — scrub itself is CLI-only.
   * - Quarantine: REAL count + reason lines. The `review →` flow was
   *   never designed (open-questions § "Quarantine review screen") —
   *   the inline items list IS the review for M5.
   * - Eviction: automatic at the watermark since D72 (reversible by
   *   construction); the card tunes rather than plans.
   * - Orphans: the D73 review gate — list, keep, two-click apply.
   */
  import {
    analyzerConfig,
    analyzers as fetchAnalyzers,
    evict as startEvict,
    evictPlan,
    gcApply,
    gcConfig,
    gcConfigSet,
    gcKeep,
    gcOrphans,
    scrub as startScrub,
    snapshot as saveSnapshot,
    storage as fetchStorage,
    storageBreakdown,
    sweep as startSweep,
  } from '../lib/api/client';
  import type {
    AnalyzerInfo,
    AnalyzersBody,
    EvictPlanBody,
    GcConfigBody,
    GcConfigParams,
    OrphansBody,
    StorageBody,
    StorageBreakdownBody,
  } from '../lib/api/types';
  import Link from '../lib/components/Link.svelte';
  import { fmtDate, fmtSize, shortHash } from '../lib/format';
  import { followJob, jobsSignal } from '../lib/jobs.svelte';
  import { residencyLabel } from '../lib/residency.svelte';
  import { errorText, loading, ready, settle, type Remote } from '../lib/remote';
  import LoadError from '../lib/components/LoadError.svelte';

  // Unmount stops any job-follow loop — the tray owns job visibility.
  let alive = true;
  $effect(() => () => {
    alive = false;
  });

  // Three independent resources, three Remotes: a failed orphan refresh
  // (say, after a keep toggle) must never blank fully-rendered stats.
  let stats = $state<Remote<StorageBody>>(loading());
  let breakdown = $state<Remote<StorageBreakdownBody>>(loading());
  let orphans = $state<Remote<OrphansBody>>(loading());
  let config = $state<Remote<GcConfigBody>>(loading());
  const refreshConfig = () => settle(gcConfig(), (value) => (config = value));

  // Scrub runs as a background job (the corpus walk is long); the tray
  // carries the progress bar, and the last-run line refreshes when it
  // finishes. A full pass by default — a sample % is a power option (CLI).
  let scrubbing = $state(false);
  let scrubError = $state<string | null>(null);
  const runScrub = () => {
    if (scrubbing) return;
    scrubbing = true;
    scrubError = null;
    startScrub().then(
      async (started) => {
        jobsSignal.bump(); // wake the tray now, not on its own cadence
        try {
          await followJob(started.job, { alive: () => alive });
        } catch {
          // Lost contact: the tray still tracks it; nothing to show here.
        }
        jobsSignal.bump();
        scrubbing = false;
        if (alive) refreshStats(); // the last-scrub line just changed
      },
      (e: unknown) => {
        scrubbing = false;
        scrubError = errorText(e);
      },
    );
  };

  // Catalog backup (D75 state snapshot): auto-saved after every change,
  // but a manual "save now" is the force-a-restore-point action.
  // Synchronous — the receipt line confirms it.
  let saving = $state(false);
  let savedNote = $state<string | null>(null);
  const saveNow = () => {
    if (saving) return;
    saving = true;
    savedNote = null;
    saveSnapshot().then(
      (rep) => {
        saving = false;
        savedNote = `saved ${shortHash(rep.hash)} · point ${rep.sequence}`;
      },
      (e: unknown) => {
        saving = false;
        savedNote = errorText(e);
      },
    );
  };

  // Eviction tuning (D72 watermarks) + a manual "reclaim now". Behind a
  // "tune" toggle — automatic eviction is the default, so this stays
  // quiet until the user goes looking (web-ui.md: management by exception).
  let evictOpen = $state(false);
  // Editable watermark/grace fields, seeded from config once it loads.
  let highWater = $state('');
  let lowWater = $state('');
  let graceDays = $state('');
  let seeded = false;
  $effect(() => {
    if (config.st === 'ready' && !seeded) {
      seeded = true;
      highWater = config.data.high_water;
      lowWater = config.data.low_water;
      graceDays = String(Math.round(config.data.grace_secs / 86_400));
    }
  });

  let savingConfig = $state(false);
  let configError = $state<string | null>(null);
  let configSaved = $state(false);
  const saveConfig = () => {
    if (savingConfig) return;
    savingConfig = true;
    configError = null;
    configSaved = false;
    const grace = Number(graceDays);
    const body: GcConfigParams = {
      high_water: highWater.trim(),
      low_water: lowWater.trim(),
      grace_secs: Number.isFinite(grace) && graceDays.trim() !== '' ? Math.round(grace * 86_400) : null,
    };
    gcConfigSet(body).then(
      (updated) => {
        savingConfig = false;
        configSaved = true;
        config = ready(updated);
      },
      (e: unknown) => {
        savingConfig = false;
        configError = errorText(e);
      },
    );
  };

  // Manual reclaim: show the dry-run plan (everything rebuildable), then
  // confirm the guarded drop. target 0 = evict every covered literal.
  let plan = $state<EvictPlanBody | null>(null);
  let planning = $state(false);
  let evicting = $state(false);
  let evictError = $state<string | null>(null);
  const checkPlan = () => {
    if (planning) return;
    planning = true;
    evictError = null;
    plan = null;
    evictPlan(0).then(
      (p) => {
        planning = false;
        plan = p;
      },
      (e: unknown) => {
        planning = false;
        evictError = errorText(e);
      },
    );
  };
  const reclaimNow = () => {
    if (evicting) return;
    evicting = true;
    evictError = null;
    startEvict(0).then(
      async (started) => {
        jobsSignal.bump();
        try {
          await followJob(started.job, { alive: () => alive });
        } catch {
          // The tray still tracks it.
        }
        jobsSignal.bump();
        evicting = false;
        plan = null;
        if (alive) refreshStats(); // bytes just left the disk
      },
      (e: unknown) => {
        evicting = false;
        evictError = errorText(e);
      },
    );
  };

  // Optimization = the analyzer families that find rebuild recipes (they
  // are what turns NOT-YET-OPTIMIZED bytes into rebuildable ones). The
  // panel enables/disables a family and runs a sweep on demand.
  let analyzers = $state<Remote<AnalyzersBody>>(loading());
  const refreshAnalyzers = () => settle(fetchAnalyzers(), (value) => (analyzers = value));
  $effect(refreshAnalyzers);
  let optOpen = $state(false);
  let optError = $state<string | null>(null);
  /** Per-family in-flight guards: toggling config, and a running sweep. */
  let toggling = $state<Record<string, boolean>>({});
  let sweeping = $state<Record<string, boolean>>({});

  const toggleFamily = (a: AnalyzerInfo) => {
    if (toggling[a.family] === true) return;
    toggling[a.family] = true;
    optError = null;
    // The PUT sets the whole config (D60), so preserve the params.
    analyzerConfig(a.family, { enabled: !a.enabled, params_hex: a.params_hex }).then(
      () => {
        toggling[a.family] = false;
        refreshAnalyzers();
      },
      (e: unknown) => {
        toggling[a.family] = false;
        optError = errorText(e);
      },
    );
  };

  const runSweep = (family: string) => {
    if (sweeping[family] === true) return;
    sweeping[family] = true;
    optError = null;
    startSweep(family).then(
      async (started) => {
        jobsSignal.bump();
        try {
          await followJob(started.job, { alive: () => alive });
        } catch {
          // The tray still tracks it.
        }
        jobsSignal.bump();
        if (alive) {
          sweeping[family] = false;
          refreshStats(); // a sweep may have shrunk the store
        }
      },
      (e: unknown) => {
        sweeping[family] = false;
        optError = errorText(e);
      },
    );
  };

  const activeAnalyzers = $derived(
    analyzers.st !== 'ready' ? 0 : analyzers.data.analyzers.filter((a) => a.enabled).length,
  );
  /** Two-click delete: first click arms, second applies (D73's human
   * gate deserves more than one tap, less than a modal). */
  let applyArmed = $state(false);
  let applying = $state(false);
  /** A failed apply keeps the reviewed list; this line says why. */
  let applyError = $state<string | null>(null);

  const refreshOrphans = () => {
    settle(gcOrphans(), (value) => (orphans = value));
  };
  const refreshStats = () => {
    settle(fetchStorage(), (value) => (stats = value));
    settle(storageBreakdown(), (value) => (breakdown = value));
  };

  /** Keep pills in flight — no double-toggle races per row. */
  let keepBusy = $state<Record<string, boolean>>({});
  let keepError = $state<string | null>(null);

  const toggleKeep = (hash: string, keep: boolean) => {
    if (keepBusy[hash] === true) return;
    keepBusy[hash] = true;
    keepError = null;
    gcKeep(hash, keep).then(
      () => {
        keepBusy[hash] = false;
        refreshOrphans();
      },
      (e: unknown) => {
        // The pill snaps back on refresh — say why, or it reads as a
        // misclick.
        keepBusy[hash] = false;
        keepError = errorText(e);
        refreshOrphans();
      },
    );
  };

  const applyAll = () => {
    if (!applyArmed) {
      applyArmed = true;
      // An armed destructive control decays: walking away and clicking
      // in this corner minutes later must not delete on what is
      // effectively one click.
      setTimeout(() => (applyArmed = false), 8000);
      return;
    }
    applying = true;
    applyError = null;
    gcApply().then(
      () => {
        applying = false;
        applyArmed = false;
        refreshOrphans();
        // Blobs just left the disk: the stat tiles and breakdown are
        // stale until they refetch too.
        refreshStats();
      },
      (e: unknown) => {
        applying = false;
        applyArmed = false;
        applyError = errorText(e);
      },
    );
  };

  $effect(refreshStats);

  $effect(refreshOrphans);

  $effect(refreshConfig);

  const reviewable = $derived(orphans.st !== 'ready' ? [] : orphans.data.orphans);

  /** `−68% via recipes`: how much smaller disk is than what it represents. */
  const savingsPct = $derived(
    stats.st !== 'ready' || stats.data.represented_bytes === 0
      ? null
      : Math.round(100 * (1 - stats.data.on_disk_bytes / stats.data.represented_bytes)),
  );

  /** Class bars are proportional to the LARGEST cell, not the sum —
   * meta/ is invisible next to data/ either way; against the max the
   * big cell reads full-scale. */
  const maxClassBytes = $derived(
    breakdown.st !== 'ready'
      ? 1
      : Math.max(1, ...breakdown.data.by_class.map((cell) => cell.bytes)),
  );
</script>

<main>
  <div class="title-row">
    <h2>Storage</h2>
    <span class="sub">what the store holds vs what it represents</span>
  </div>

  {#if stats.st === 'error'}
    <!-- Undesigned loading/error states: plain mono in --faint. -->
    <LoadError msg={stats.msg} onretry={refreshStats} />
  {:else if stats.st === 'loading'}
    <p class="undesigned">loading…</p>
  {:else}
    {@const s = stats.data}
    <div class="tiles">
      <div class="tile">
        <span class="label">BLOBS</span>
        <span class="num">{s.blob_count.toLocaleString()}</span>
      </div>
      <div class="tile">
        <span class="label">ON DISK</span>
        <span class="num">{fmtSize(s.on_disk_bytes)}</span>
      </div>
      <div class="tile brag">
        <span class="label">REPRESENTED</span>
        <span class="num">{fmtSize(s.represented_bytes)}</span>
        {#if savingsPct !== null && savingsPct > 0}
          <span class="tile-sub ok">−{savingsPct}% via recipes</span>
        {/if}
      </div>
      <div class="tile dashed">
        <span class="label">NOT YET OPTIMIZED</span>
        <span class="num">{fmtSize(s.literal_only_bytes)}</span>
      </div>
    </div>

    <!-- Maintenance status (web-ui.md: management by exception).
         One quiet line per subsystem while healthy; quarantine and
         orphans grow into real cards below ONLY when they need a
         human. Scrub and eviction never do — they stay lines. -->
    <div class="maint">
      <div class="maint-row">
        <span class="maint-label">Scrub</span>
        <span class="maint-copy">
          {#if scrubError !== null}
            <span class="row-error">couldn't start — {scrubError}</span>
          {:else if s.last_scrub !== null}
            last: {fmtDate(s.last_scrub.finished_at)} · {s.last_scrub.name}
          {:else}
            never run
          {/if}
        </span>
        <!-- Re-hash every stored blob against its name and report
             corruption; runs in the jobs tray. -->
        <button class="pill" disabled={scrubbing} onclick={runScrub}>
          {#if scrubbing}<!-- @wc-context: scrub job state -->running…{:else}<!-- @wc-context: start a scrub -->run{/if}
        </button>
      </div>
      <div class="maint-row">
        <span class="maint-label"><!-- @wc-context: catalog restore point -->Backup</span>
        <span class="maint-copy">
          {#if savedNote !== null}
            {savedNote}
          {:else}
            auto-saved after every change — the restore point recover rebuilds from
          {/if}
        </span>
        <button class="pill" disabled={saving} onclick={saveNow}>
          {#if saving}<!-- @wc-context: backup in progress -->saving…{:else}<!-- @wc-context: force a backup -->save now{/if}
        </button>
      </div>
      <div class="maint-row">
        <span class="maint-label"><!-- @wc-context: storage eviction -->Eviction</span>
        <!-- D72: watermark eviction is automatic and REVERSIBLE by
             construction — every drop has a locally-replayed rebuild
             route. The panel tunes the watermarks or reclaims now. -->
        <span class="maint-copy">
          {#if config.st === 'ready' && config.data.high_water === 'off'}
            disarmed — nothing is evicted automatically
          {:else if config.st === 'ready'}
            automatic at {config.data.high_water} — drops only what's rebuildable
          {:else}
            automatic at the watermark — drops only what's rebuildable
          {/if}
        </span>
        <button class="pill" aria-expanded={evictOpen} onclick={() => (evictOpen = !evictOpen)}>
          <!-- @wc-context: open the eviction settings panel -->tune
        </button>
      </div>
      {#if evictOpen}
        <div class="panel">
          <div class="fields">
            <label class="field">
              <!-- @wc-context: disk-fullness threshold that starts eviction -->
              <span class="field-label">start at</span>
              <input class="field-input" bind:value={highWater} placeholder="90%" />
            </label>
            <label class="field">
              <!-- @wc-context: disk-fullness threshold that stops eviction -->
              <span class="field-label">down to</span>
              <input class="field-input" bind:value={lowWater} placeholder="85%" />
            </label>
            <label class="field">
              <!-- @wc-context: how long a new blob is protected from cleanup -->
              <span class="field-label">grace (days)</span>
              <input class="field-input" type="number" min="0" bind:value={graceDays} />
            </label>
            <button class="pill" disabled={savingConfig} onclick={saveConfig}>
              {#if savingConfig}<!-- @wc-context: saving eviction settings -->saving…{:else}<!-- @wc-context: save eviction settings -->save{/if}
            </button>
          </div>
          <p class="panel-hint">
            thresholds take a percentage ("90%"), an absolute size, or "off" to disarm
          </p>
          {#if configSaved}
            <p class="panel-ok">settings saved</p>
          {/if}
          {#if configError !== null}
            <p class="panel-error">couldn't save — {configError}</p>
          {/if}

          <div class="reclaim">
            {#if plan === null}
              <button class="pill" disabled={planning || evicting} onclick={checkPlan}>
                {#if planning}<!-- @wc-context: computing what can be freed -->checking…{:else}<!-- @wc-context: preview a manual space reclaim -->reclaim space now…{/if}
              </button>
            {:else if plan.evictable === 0}
              <span class="panel-hint">nothing to reclaim — everything on disk is either needed or already rebuildable</span>
            {:else}
              <span class="panel-hint">
                {plan.evictable.toLocaleString()} blob(s) · {fmtSize(plan.reclaimable_bytes)} can be freed and rebuilt on demand
              </span>
              <button class="pill" disabled={evicting} onclick={reclaimNow}>
                {#if evicting}<!-- @wc-context: eviction running -->reclaiming…{:else}<!-- @wc-context: confirm the manual reclaim -->reclaim now{/if}
              </button>
              <button class="pill" disabled={evicting} onclick={() => (plan = null)}>
                <!-- @wc-context: cancel the reclaim -->cancel
              </button>
            {/if}
          </div>
          {#if evictError !== null}
            <p class="panel-error">{evictError}</p>
          {/if}
        </div>
      {/if}
      <div class="maint-row">
        <span class="maint-label"><!-- @wc-context: shrinking the store -->Optimization</span>
        <span class="maint-copy">
          {#if analyzers.st === 'ready'}
            {activeAnalyzers}/{analyzers.data.analyzers.length} active — finds rebuild recipes that shrink the store
          {:else}
            finds rebuild recipes that shrink the store
          {/if}
        </span>
        <button class="pill" aria-expanded={optOpen} onclick={() => (optOpen = !optOpen)}>
          <!-- @wc-context: open the optimization settings panel -->tune
        </button>
      </div>
      {#if optOpen}
        <div class="panel">
          {#if analyzers.st === 'error'}
            <LoadError msg={analyzers.msg} onretry={refreshAnalyzers} />
          {:else if analyzers.st === 'loading'}
            <span class="panel-hint">loading…</span>
          {:else}
            {#each analyzers.data.analyzers as a (a.family)}
              <div class="opt-row">
                <span class="opt-name">{a.family}</span>
                <button
                  class="pill"
                  aria-pressed={a.enabled}
                  disabled={toggling[a.family] === true}
                  onclick={() => toggleFamily(a)}
                >
                  {#if a.enabled}<!-- @wc-context: analyzer is on -->on{:else}<!-- @wc-context: analyzer is off -->off{/if}
                </button>
                <button
                  class="pill"
                  disabled={!a.enabled || sweeping[a.family] === true}
                  onclick={() => runSweep(a.family)}
                >
                  {#if sweeping[a.family] === true}<!-- @wc-context: sweep running -->running…{:else}<!-- @wc-context: run one analyzer -->run{/if}
                </button>
              </div>
            {/each}
            <p class="panel-hint">
              a run analyzes what's not yet optimized; disabled families are skipped
            </p>
          {/if}
          {#if optError !== null}
            <p class="panel-error">{optError}</p>
          {/if}
        </div>
      {/if}
      {#if s.quarantine.count === 0}
        <div class="maint-row">
          <span class="maint-label"><!-- @wc-context: bad components, not people -->Quarantine</span>
          <span class="maint-copy">empty</span>
        </div>
      {/if}
      {#if orphans.st === 'loading'}
        <div class="maint-row">
          <span class="maint-label"><!-- @wc-context: unreferenced blobs, not people -->Orphans</span>
          <span class="maint-copy">loading…</span>
        </div>
      {:else if orphans.st === 'error'}
        <div class="maint-row">
          <span class="maint-label"><!-- @wc-context: unreferenced blobs, not people -->Orphans</span>
          <!-- Row-local failure: stats and breakdown stay rendered. -->
          <LoadError msg={orphans.msg} onretry={refreshOrphans} />
        </div>
      {:else if reviewable.length === 0}
        <div class="maint-row">
          <span class="maint-label"><!-- @wc-context: unreferenced blobs, not people -->Orphans</span>
          <span class="maint-copy">none</span>
        </div>
      {/if}
    </div>

    {#if s.quarantine.count > 0 || reviewable.length > 0}
      <div class="cards">
        {#if s.quarantine.count > 0}
          <div class="action-card danger">
            <div class="card-title">
              <!-- @wc-context: bad components, not people -->
              Quarantine · {s.quarantine.count.toLocaleString()}
            </div>
            <!-- The `review →` flow was never designed; this inline list
                 IS the M5 review (open-questions § "Quarantine review
                 screen"; M5 scope ruling, open-questions 2026-07-11). -->
            <div class="q-items">
              {#each s.quarantine.items as item (item.component)}
                <div class="q-item">
                  <span class="q-hash">{shortHash(item.component)}</span>
                  <span class="q-reason">{item.reason}</span>
                  <span class="q-when">{fmtDate(item.quarantined_at)}</span>
                </div>
              {/each}
            </div>
          </div>
        {/if}

        {#if orphans.st === 'ready' && reviewable.length > 0}
          <div class="action-card" class:danger={reviewable.some((o) => !o.kept)}>
            <div class="card-title">
              <!-- @wc-context: unreferenced blobs, not people -->
              Orphans · {reviewable.length.toLocaleString()}
            </div>
            <!-- D73 review gate: deletion is THIS surface, never ambient.
                 Each row shows ingest provenance; keep excludes it. -->
            <div class="q-items">
              {#each reviewable as orphan (orphan.hash)}
                <div class="q-item">
                  <span class="q-hash">{shortHash(orphan.hash)}</span>
                  <span class="q-reason">
                    {fmtSize(orphan.size)}{orphan.sources.length > 0
                      ? ` · ${orphan.sources.join(', ')}`
                      : ''}
                  </span>
                  <button
                    class="pill"
                    aria-pressed={orphan.kept}
                    disabled={keepBusy[orphan.hash] === true}
                    onclick={() => toggleKeep(orphan.hash, !orphan.kept)}
                  >
                    {orphan.kept ? 'kept ✓' : 'keep'}
                  </button>
                </div>
              {/each}
            </div>
            <p class="copy">
              {fmtSize(orphans.data.reclaimable_bytes)} reclaimable · every delete re-verifies
              unreferenced at delete time
            </p>
            <button class="pill" class:bad-pill={applyArmed} disabled={applying} onclick={applyAll}>
              {#if applying}deleting…{:else if applyArmed}confirm: delete non-kept{:else}delete non-kept…{/if}
            </button>
            {#if applyError !== null}
              <p class="copy apply-error">delete failed — {applyError}</p>
            {/if}
            {#if keepError !== null}
              <p class="copy apply-error">keep toggle failed — {keepError}</p>
            {/if}
          </div>
        {/if}
      </div>
    {/if}

    <!-- Debug-grade introspection (open-questions 2026-07-11): the
         /v1/storage/breakdown aggregates, with the largest blobs
         linking into the /storage/blob/{hash} inspector. -->
    {#if breakdown.st === 'error'}
      <!-- Breakdown-only failure: the tiles and cards above stand. -->
      <LoadError msg={breakdown.msg} onretry={refreshStats} />
    {:else if breakdown.st === 'ready'}
      {@const b = breakdown.data}
      <div class="bytes-card">
        <div class="bytes-title">WHERE THE BYTES LIVE</div>

        <div class="classes">
          {#each b.by_class as cell (cell.namespace + cell.residency)}
            <div class="class-row">
              <span class="class-label">{cell.namespace} · {residencyLabel(cell.residency)}</span>
              <span class="track">
                <span
                  class="fill"
                  style:width="{Math.max(1, Math.round((100 * cell.bytes) / maxClassBytes))}%"
                ></span>
              </span>
              <span class="class-bytes">{fmtSize(cell.bytes)}</span>
              <span class="class-blobs">
                {cell.blobs.toLocaleString()} blobs{#if cell.sizeless > 0}
                  · {cell.sizeless.toLocaleString()} sizeless{/if}
              </span>
            </div>
          {/each}
        </div>

        <div class="split">
          <div class="col">
            <div class="col-title">by source</div>
            <div class="grid-table sources">
              <span class="th">source</span>
              <span class="th num">blobs</span>
              <span class="th num">bytes</span>
              {#each b.by_source as row (row.source)}
                <span class="td data">{row.source}</span>
                <span class="td num">{row.blobs.toLocaleString()}</span>
                <span class="td num">{fmtSize(row.bytes)}</span>
              {/each}
            </div>
          </div>
          <div class="col">
            <div class="col-title">largest blobs</div>
            <div class="grid-table largest">
              <span class="th">hash</span>
              <span class="th num">size</span>
              <span class="th">where</span>
              {#each b.largest as blob (blob.hash)}
                <Link class="td data blob-link" href={`/storage/blob/${blob.hash}`}>
                  {shortHash(blob.hash)}
                </Link>
                <span class="td num">{blob.size === null ? '—' : fmtSize(blob.size)}</span>
                <span class="td data">{residencyLabel(blob.residency)}</span>
              {/each}
            </div>
          </div>
        </div>
      </div>
    {/if}
  {/if}
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

  .undesigned {
    font: 400 0.78125rem var(--font-data);
    color: var(--faint);
  }

  .tiles {
    display: grid;
    grid-template-columns: repeat(4, 1fr);
    gap: 16px;
  }

  .tile {
    background: var(--panel);
    border: 2px solid var(--ink);
    border-radius: var(--r-card);
    box-shadow: var(--shadow-card);
    padding: 14px 18px 16px;
    display: flex;
    flex-direction: column;
    gap: 6px;
  }

  .tile.dashed {
    border-style: dashed;
    border-color: var(--dim);
    box-shadow: none;
  }

  .label {
    font: 800 0.8125rem var(--font-display);
    letter-spacing: 0.02em;
    color: var(--mut);
  }

  .num {
    font: 800 1.25rem var(--font-display);
    letter-spacing: -0.02em;
  }

  .brag .num {
    color: var(--okT);
  }

  .tile-sub {
    font: 400 0.71875rem var(--font-data);
    color: var(--faint);
  }

  .tile-sub.ok {
    color: var(--okT);
  }

  .maint {
    margin-top: 18px;
    background: var(--panel);
    border: 1px solid var(--hair);
    border-radius: var(--r-sub);
    padding: 10px 20px;
  }

  .maint-row {
    display: flex;
    align-items: center;
    gap: 14px;
    padding: 6px 0;
    font: 400 0.78125rem var(--font-data);
    color: var(--mut);
  }

  .maint-row + .maint-row {
    border-top: 1px dashed var(--hair);
  }

  .maint-label {
    width: 90px;
    flex: none;
    font: 800 0.78125rem var(--font-display);
    letter-spacing: 0.02em;
    color: var(--text);
  }

  .maint-copy {
    flex: 1;
    min-width: 0;
  }

  .row-error {
    color: var(--bad);
  }

  /* The eviction tuning panel: sits under its maint-row, inset to line
     up past the label column. */
  .panel {
    padding: 4px 0 12px;
    border-top: 1px dashed var(--hair);
    display: flex;
    flex-direction: column;
    gap: 8px;
  }

  .fields {
    display: flex;
    flex-wrap: wrap;
    align-items: flex-end;
    gap: 12px;
  }

  .field {
    display: flex;
    flex-direction: column;
    gap: 4px;
  }

  .field-label {
    font: 600 0.6875rem var(--font-data);
    color: var(--faint);
  }

  .field-input {
    width: 90px;
    border: 1.5px solid var(--dim);
    border-radius: var(--r-input);
    padding: 4px 8px;
    background: var(--panel);
    font: 400 0.78125rem var(--font-data);
    color: var(--text);
  }

  .panel-hint {
    margin: 0;
    font: 400 0.71875rem var(--font-data);
    color: var(--faint);
  }

  .panel-ok {
    margin: 0;
    font: 400 0.71875rem var(--font-data);
    color: var(--okT);
  }

  .panel-error {
    margin: 0;
    font: 400 0.71875rem var(--font-data);
    color: var(--bad);
  }

  .reclaim {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 12px;
    margin-top: 4px;
  }

  .opt-row {
    display: flex;
    align-items: center;
    gap: 10px;
  }

  .opt-name {
    flex: 1;
    min-width: 0;
    font: 400 0.78125rem var(--font-data);
    color: var(--text);
  }

  /* The on/off pill reads pressed when the family is enabled. */
  .opt-row .pill[aria-pressed='true'] {
    border-color: var(--ok);
    color: var(--okT);
  }

  .cards {
    display: grid;
    /* Attention cards only render when something needs a human, so the
       grid sizes to however many fired (one full-width, two split). */
    grid-template-columns: repeat(auto-fit, minmax(320px, 1fr));
    gap: 16px;
    margin-top: 18px;
    align-items: start;
  }

  .action-card {
    background: var(--panel);
    border: 1px solid var(--hair);
    border-radius: var(--r-sub);
    padding: 18px 20px;
  }

  .action-card.danger {
    border: 1.5px solid var(--bad);
    background: color-mix(in srgb, var(--bad) 8%, var(--panel));
  }

  .card-title {
    font: 800 0.875rem var(--font-display);
    letter-spacing: 0.02em;
    margin-bottom: 8px;
  }

  .danger .card-title {
    color: var(--bad);
  }

  .copy {
    margin: 0 0 12px;
    font: 400 0.78125rem var(--font-data);
    color: var(--mut);
    line-height: 1.6;
  }

  .pill {
    all: unset;
    border: 2px solid var(--ink);
    border-radius: var(--r-pill);
    padding: 4px 14px;
    background: var(--panel);
    font: 600 0.75rem var(--font-data);
    cursor: pointer;
  }

  /* The armed state of the two-click delete: unmistakably destructive. */
  .pill.bad-pill {
    border-color: var(--bad);
    color: var(--bad);
  }

  .pill:disabled {
    cursor: default;
    color: var(--faint);
  }

  .apply-error {
    margin: 10px 0 0;
    color: var(--bad);
  }

  .q-items {
    font: 400 0.75rem var(--font-data);
    color: var(--mut);
    line-height: 1.9;
  }

  .q-item {
    display: flex;
    gap: 10px;
    align-items: baseline;
  }

  .q-hash {
    color: var(--text);
  }

  .q-reason {
    flex: 1;
    min-width: 0;
    overflow-wrap: anywhere;
  }

  .q-when {
    color: var(--faint);
  }

  .bytes-card {
    margin-top: 18px;
    background: var(--panel);
    border: 2px solid var(--ink);
    border-radius: var(--r-card);
    box-shadow: var(--shadow-card);
    padding: 16px 20px 20px;
  }

  .bytes-title {
    font: 800 0.875rem var(--font-display);
    letter-spacing: 0.02em;
    margin-bottom: 12px;
  }

  .classes {
    display: flex;
    flex-direction: column;
    gap: 6px;
    margin-bottom: 16px;
  }

  .class-row {
    display: flex;
    align-items: center;
    gap: 12px;
    font: 400 0.75rem var(--font-data);
  }

  .class-label {
    width: 170px;
    flex: none;
    color: var(--text);
  }

  /* The activity-page track/fill register — a width-percent fill is
     enough here (StackedBar is the shelf's multi-state register). */
  .track {
    flex: 1;
    height: 8px;
    background: var(--panel2);
    border: 1px solid var(--hair);
    border-radius: var(--r-fill);
    overflow: hidden;
  }

  .fill {
    display: block;
    height: 100%;
    background: var(--ink);
    border-radius: var(--r-fill);
  }

  .class-bytes {
    width: 72px;
    flex: none;
    text-align: right;
    color: var(--text);
  }

  .class-blobs {
    width: 160px;
    flex: none;
    text-align: right;
    color: var(--faint);
  }

  .split {
    display: grid;
    /* minmax(0, …): a plain 1fr's min size is the content's min-content
       width, so one wide row would silently break the 50/50. */
    grid-template-columns: minmax(0, 1fr) minmax(0, 1fr);
    gap: 24px;
    align-items: start;
  }

  .col-title {
    font: 800 0.78125rem var(--font-display);
    letter-spacing: 0.02em;
    color: var(--mut);
    margin-bottom: 6px;
  }

  .grid-table {
    display: grid;
    font: 400 0.75rem var(--font-data);
    column-gap: 14px;
    row-gap: 2px;
  }

  .grid-table.sources {
    grid-template-columns: minmax(0, 1fr) auto auto;
  }

  /* Hash takes the flexible column so size and residency sit at the
     table's right edge — `auto auto 1fr` left the whole table hugging
     the left margin with a dead 1fr tail. */
  .grid-table.largest {
    grid-template-columns: minmax(0, 1fr) auto auto;
  }

  .th {
    font: 600 0.6875rem var(--font-data);
    color: var(--faint);
    border-bottom: 1px solid var(--rule);
    padding-bottom: 3px;
  }

  .td {
    color: var(--mut);
  }

  .td.data {
    color: var(--text);
  }

  .num {
    text-align: right;
  }

  .grid-table :global(a.blob-link) {
    color: var(--text);
    text-decoration: underline;
    text-decoration-color: var(--dim);
    text-underline-offset: 2px;
  }

  /* Tablet: four stat tiles pair up, the three action cards and the
     by-source / largest-blobs split each go single column. */
  @media (max-width: 720px) {
    .title-row {
      flex-wrap: wrap;
      gap: 4px 14px;
    }

    .tiles {
      grid-template-columns: repeat(2, 1fr);
    }

    .cards {
      grid-template-columns: 1fr;
    }

    .split {
      grid-template-columns: 1fr;
      gap: 20px;
    }
  }

  @media (max-width: 640px) {
    /* The class row's three fixed columns (170 + 72 + 160px) blow past a
       phone width, so the label takes its own line and the track, bytes,
       and blob count share the line below it. */
    .class-row {
      flex-wrap: wrap;
      row-gap: 4px;
    }

    .class-label {
      flex-basis: 100%;
      width: auto;
    }

    .class-blobs {
      width: auto;
      margin-left: auto;
    }
  }
</style>
