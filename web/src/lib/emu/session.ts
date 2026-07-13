/**
 * One running emulator: the host side of the worker protocol
 * (protocol.ts). Owns the Worker, the AudioContext scheduling, and the
 * input send; the screen owns presentation (canvas) and input
 * *collection* (events, gamepad polling).
 *
 * Audio is scheduled AudioBuffers with drift correction (dust-web's
 * proven main-thread shape, 88-emulation.md): each frame's samples are
 * queued at a running tail timestamp; a tail that falls behind the
 * clock snaps forward (dropout, not slow-motion), a tail too far ahead
 * drops the chunk (bounded latency, not unbounded lag). The
 * AudioWorklet + SharedArrayBuffer ring is the deliberate later step.
 */

import type { Descriptor, HostToWorker, Touch, WorkerToHost } from './protocol';

/** Cap on scheduled-ahead audio: jitter cushion, not perceivable lag. */
const MAX_AUDIO_LEAD_SECONDS = 0.1;

export type SessionCallbacks = {
  /** The core booted; frames follow. */
  onloaded: () => void;
  /** Terminal — the worker is dead (message already user-legible). */
  onerror: (message: string) => void;
  /** Stacked-screens RGBA pixels, alpha undefined. ~60/s. */
  onframe: (video: Uint32Array) => void;
};

export class EmuSession {
  private readonly descriptor: Descriptor;
  private readonly worker: Worker;
  private audio: AudioContext | null = null;
  private audioTail = 0;
  private buttons = 0;
  private touch: Touch = null;
  private disposed = false;

  constructor(
    base: string,
    descriptor: Descriptor,
    rom: ArrayBuffer,
    /** Resolved BIOS-slot bytes, keyed by slot name (empty = HLE). */
    sysFiles: Record<string, ArrayBuffer>,
    cb: SessionCallbacks,
  ) {
    this.descriptor = descriptor;
    this.worker = new Worker(`${base}/${descriptor.worker}`, { type: 'module' });
    this.worker.onmessage = (e: MessageEvent<WorkerToHost>) => {
      const msg = e.data;
      switch (msg.type) {
        case 'loaded':
          cb.onloaded();
          break;
        case 'error':
          cb.onerror(msg.message);
          break;
        case 'frame':
          cb.onframe(new Uint32Array(msg.video));
          this.scheduleAudio(msg.audio);
          break;
        case 'stressResult':
          break; // CI-only surface (the core's test page uses it)
      }
    };
    this.worker.onerror = (e) => cb.onerror(e.message || 'emulator worker failed');
    this.post(
      {
        type: 'load',
        rom,
        bios7: sysFiles['bios7'],
        bios9: sysFiles['bios9'],
        firmware: sysFiles['firmware'],
      },
      [rom, ...Object.values(sysFiles)],
    );
  }

  private post(msg: HostToWorker, transfer: Transferable[] = []): void {
    if (!this.disposed) this.worker.postMessage(msg, transfer);
  }

  /** Absolute input state; cheap to call on every event/poll. */
  setInput(buttons: number, touch: Touch): void {
    if (buttons === this.buttons && touch === this.touch) return;
    this.buttons = buttons;
    this.touch = touch;
    this.post({ type: 'input', buttons, touch });
  }

  pause(): void {
    this.post({ type: 'pause' });
    void this.audio?.suspend();
  }

  resume(): void {
    this.post({ type: 'resume' });
    void this.audio?.resume();
  }

  /**
   * Autoplay policy: an AudioContext only runs after a user gesture —
   * the screen calls this from its first pointer/key event, which is
   * also the moment a player demonstrably wants sound.
   */
  unlockAudio(): void {
    if (this.audio === null) {
      // Older iOS Safari only has the webkit-prefixed constructor.
      const Ctor =
        globalThis.AudioContext ??
        (globalThis as { webkitAudioContext?: typeof AudioContext }).webkitAudioContext;
      if (Ctor === undefined) return; // no audio is better than no game
      this.audio = new Ctor();
    }
    // Not just 'suspended': iOS reports a nonstandard 'interrupted'
    // after calls/backgrounding, and resume() is the answer to both.
    if (this.audio.state !== 'running') void this.audio.resume();
  }

  dispose(): void {
    if (this.disposed) return;
    this.post({ type: 'dispose' });
    this.disposed = true;
    // dispose asks the worker to close itself; terminate is the
    // backstop that also covers a worker wedged mid-frame.
    this.worker.terminate();
    void this.audio?.close();
    this.audio = null;
  }

  private scheduleAudio(interleaved: Float32Array): void {
    const ctx = this.audio;
    const pairs = interleaved.length / 2;
    if (ctx === null || ctx.state !== 'running' || pairs === 0) return;
    const rate = this.descriptor.audioSampleRate;
    const now = ctx.currentTime + (ctx.baseLatency || 1 / 60);
    if (this.audioTail > now + MAX_AUDIO_LEAD_SECONDS) return; // ahead: drop
    if (this.audioTail < now) this.audioTail = now; // behind: snap
    const l = new Float32Array(pairs);
    const r = new Float32Array(pairs);
    for (let i = 0; i < pairs; i++) {
      l[i] = interleaved[2 * i];
      r[i] = interleaved[2 * i + 1];
    }
    // The buffer carries the CORE's rate; playback resamples to the
    // device rate for free.
    const buffer = ctx.createBuffer(2, pairs, rate);
    buffer.copyToChannel(l, 0);
    buffer.copyToChannel(r, 1);
    const src = ctx.createBufferSource();
    src.buffer = buffer;
    src.connect(ctx.destination);
    src.start(this.audioTail);
    this.audioTail += pairs / rate;
  }
}
