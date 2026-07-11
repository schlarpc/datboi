/**
 * "A job just started" signal: a screen bumps the version after
 * POST /v1/ingest and the JobsTray refetches immediately instead of
 * waiting for its own cadence — without the screen importing the tray
 * (or vice versa).
 */

let version = $state(0);

export const jobsSignal = {
  get version(): number {
    return version;
  },
  bump(): void {
    version += 1;
  },
};
