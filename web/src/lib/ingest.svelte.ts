/**
 * The ingest flow as APP state, not screen state. A multi-GB upload
 * outlives any screen: navigating away used to unmount the whole flow
 * — uploads kept running invisibly, returning to /ingest showed a
 * pristine dropzone inviting a duplicate drop, and the report was
 * never seen. The flow now lives here (the jobs.svelte.ts precedent);
 * the screen is a view over it, so leaving and coming back lands on
 * the live progress or the finished report.
 *
 * A beforeunload guard warns while the flow is busy — closing the tab
 * mid-upload kills the transfer for real, so that one deserves a
 * browser prompt.
 */
import { startIngest, uploadRom } from './api/client';
import type { JobDetailBody } from './api/types';
import { followJob, jobsSignal, LostContact } from './jobs.svelte';
import { errorText } from './remote';
import type { DropFile } from './upload';

export interface QueueItem {
  name: string;
  file: File;
  size: number;
  sent: number;
  state: 'queued' | 'uploading' | 'staged' | 'failed';
  token?: string;
  error?: string;
}

export type IngestPhase = 'idle' | 'uploading' | 'ingesting' | 'report';

const state = $state({
  phase: 'idle' as IngestPhase,
  queue: [] as QueueItem[],
  job: null as JobDetailBody | null,
  /** Infrastructure failure (start/poll); per-file refusals live in the report. */
  failure: null as string | null,
  /** Polling gave up (LostContact) — the job may well still be running. */
  lostContact: false,
});

async function follow(id: number): Promise<void> {
  try {
    await followJob(id, {
      onUpdate: (detail) => {
        state.job = detail;
      },
    });
  } catch (e) {
    if (e instanceof LostContact) {
      state.lostContact = true; // NOT a job failure — the report card says so
    } else {
      state.failure = errorText(e);
    }
  }
  state.phase = 'report';
  jobsSignal.bump(); // flip the tray row to done promptly
}

export const ingestFlow = {
  get phase(): IngestPhase {
    return state.phase;
  },
  get queue(): QueueItem[] {
    return state.queue;
  },
  get job(): JobDetailBody | null {
    return state.job;
  },
  get failure(): string | null {
    return state.failure;
  },
  get lostContact(): boolean {
    return state.lostContact;
  },
  get busy(): boolean {
    return state.phase === 'uploading' || state.phase === 'ingesting';
  },

  async begin(files: DropFile[]): Promise<void> {
    if (files.length === 0 || this.busy) {
      return;
    }
    state.failure = null;
    state.lostContact = false;
    state.job = null;
    state.phase = 'uploading';
    state.queue = files.map(({ name, file }) => ({
      name,
      file,
      size: file.size,
      sent: 0,
      state: 'queued' as const,
    }));
    // Sequential uploads: steady per-file bars, and the store write is
    // the bottleneck anyway.
    for (const item of state.queue) {
      item.state = 'uploading';
      try {
        const receipt = await uploadRom(item.name, item.file, (sent) => {
          item.sent = sent;
        });
        item.token = receipt.upload;
        item.sent = item.size;
        item.state = 'staged';
      } catch (e) {
        item.state = 'failed';
        item.error = errorText(e);
      }
    }
    const tokens = state.queue.flatMap((item) => (item.token === undefined ? [] : [item.token]));
    if (tokens.length === 0) {
      state.phase = 'report'; // nothing staged; the queue rows carry the reasons
      return;
    }
    try {
      const started = await startIngest(tokens);
      jobsSignal.bump(); // wake the tray now, not on its own cadence
      state.phase = 'ingesting';
      await follow(started.job);
    } catch (e) {
      state.failure = errorText(e);
      state.phase = 'report';
    }
  },
};

// Tab close mid-flow kills the upload for real — that one earns a
// browser prompt. In-app navigation is safe now (the flow lives here).
window.addEventListener('beforeunload', (e) => {
  if (ingestFlow.busy) {
    e.preventDefault();
  }
});
