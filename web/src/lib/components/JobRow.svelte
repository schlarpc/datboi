<script lang="ts">
  /**
   * One running job in the tray (spec §2.2): name, 120×5px mini
   * progress bar, percent label flipping to `done ✓` at 100. Split out
   * of JobsTray so the row renders (and is tested) against mock jobs
   * while the daemon's registry doesn't exist yet.
   */
  import type { Job } from '../api/types';

  let { job }: { job: Job } = $props();
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
