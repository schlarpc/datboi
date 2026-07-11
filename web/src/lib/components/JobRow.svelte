<script lang="ts" module>
  /**
   * The tray's RENDERING contract (spec §2.2: name + progress + done
   * label), written forward so the rows exist when jobs arrive. This is
   * deliberately NOT the API's Job: the D69 contract specifies no job
   * shape yet (`Job: Record<string, never>` — the daemon has no
   * registry, docs/open-questions.md § "Jobs tray backend"), so the UI
   * shape lives here and JobsTray narrows API items at runtime. When
   * the registry lands and Job gains fields, re-derive this from
   * `components['schemas']['Job']` and delete the guard.
   */
  export interface TrayJob {
    id: number;
    name: string;
    /** 0–100. */
    progress: number;
  }

  export function isTrayJob(job: unknown): job is TrayJob {
    if (typeof job !== 'object' || job === null) {
      return false;
    }
    const candidate = job as Record<string, unknown>;
    return (
      typeof candidate.id === 'number' &&
      typeof candidate.name === 'string' &&
      typeof candidate.progress === 'number'
    );
  }
</script>

<script lang="ts">
  /**
   * One running job in the tray (spec §2.2): name, 120×5px mini
   * progress bar, percent label flipping to `done ✓` at 100. Split out
   * of JobsTray so the row renders (and is tested) against mock jobs
   * while the daemon's registry doesn't exist yet.
   */
  let { job }: { job: TrayJob } = $props();
</script>

<span class="job">
  <span class="name">{job.name}</span>
  <span class="track"><span class="fill" style:width="{job.progress}%"></span></span>
  <span class="label">
    {#if job.progress >= 100}
      done ✓
    {:else}
      {job.progress}%
    {/if}
  </span>
</span>

<style>
  .job {
    display: inline-flex;
    align-items: center;
    gap: 8px;
  }

  .track {
    width: 120px;
    height: 5px;
    border-radius: var(--r-fill);
    background: var(--panel2);
    overflow: hidden;
  }

  .fill {
    display: block;
    height: 100%;
    background: var(--ok);
    transition: width 0.5s linear;
  }
</style>
