/**
 * "A job just started" signal: a screen bumps the version after
 * POST /v1/ingest and the shared registry poll (activity.svelte.ts,
 * run by the Header) refetches immediately instead of waiting for its
 * own cadence — without the screen importing the header (or vice
 * versa).
 */

import { jobDetail } from './api/client';
import type { JobDetailBody } from './api/types';

let version = $state(0);

export const jobsSignal = {
  get version(): number {
    return version;
  },
  bump(): void {
    version += 1;
  },
};

/** followJob gave up after consecutive failed polls. The job itself is
 * NOT known to have failed — we lost contact with the daemon. */
export class LostContact extends Error {}

const FOLLOW_MS = 1000;
/** Consecutive poll failures tolerated before giving up (~5 s outage). */
const FOLLOW_GRACE = 5;

/**
 * Poll a job to its terminal state — the ONE way screens follow a job
 * they just started.
 *
 * Two hazards every hand-rolled loop got wrong live here instead:
 * cancellation (`alive` goes false on unmount → resolve null and stop
 * polling; the tray owns global job visibility, so a destroyed screen
 * must not keep a private 1 s loop running for a multi-hour job) and
 * transient failure (a blip mid-poll is "lost contact", never "the job
 * failed" — the loop rides out FOLLOW_GRACE consecutive failures and
 * only then rejects with LostContact).
 */
export async function followJob(
  id: number,
  opts: { alive?: () => boolean; onUpdate?: (job: JobDetailBody) => void } = {},
): Promise<JobDetailBody | null> {
  const alive = opts.alive ?? (() => true);
  let strikes = 0;
  for (;;) {
    if (!alive()) return null;
    try {
      const job = await jobDetail(id);
      strikes = 0;
      if (!alive()) return null;
      opts.onUpdate?.(job);
      if (job.state !== 'running') return job;
    } catch (e) {
      strikes += 1;
      if (strikes >= FOLLOW_GRACE) {
        throw new LostContact(e instanceof Error ? e.message : String(e));
      }
    }
    await new Promise((resolve) => setTimeout(resolve, FOLLOW_MS));
  }
}
