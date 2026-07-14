/**
 * The emu worker protocol (D84, docs/emulation.md § the host
 * contract). Cores are self-contained assets under /emu/{id}/ —
 * descriptor.json (this Descriptor shape) + worker.js (the protocol's
 * other side) + wasm. The host speaks ONLY postMessage to a core;
 * that boundary is also the license line (the core side is GPL-3,
 * everything importing this file is datboi).
 *
 * Deliberately unfrozen until a second core exercises it
 * (open-questions § emulation deferred items).
 */

/** descriptor.json: everything the host must know before load. */
export type Descriptor = {
  id: string;
  name: string;
  /** Worker entry, relative to the core's base path. */
  worker: string;
  /** Composited top-to-bottom by the host; the core knows no layout. */
  screens: { width: number; height: number }[];
  /** Index into screens that accepts pointer input, or null. */
  pointerScreen: number | null;
  audioSampleRate: number;
  frameRate: number;
  /** Button name → bit in the input bitmask. */
  buttons: Record<string, number>;
  romExtensions: string[];
  /**
   * BIOS-from-CAS (emulation.md): each slot names a load-message
   * field (bios7/bios9/firmware) and the blake3 hashes accepted for
   * it. The host tries GET /v1/blobs/{hash}/bytes per hash; a miss or
   * a friend's 403 just means the core falls back to HLE. The hash
   * list IS the verification — the dumps are ordinary ingested blobs.
   */
  biosSlots: { name: string; hashes: string[] }[];
};

/** Pointer state in native pointer-screen coordinates; null = pen up. */
export type Touch = { x: number; y: number } | null;

export type HostToWorker =
  | {
      type: 'load';
      rom: ArrayBuffer;
      bios7?: ArrayBuffer;
      bios9?: ArrayBuffer;
      firmware?: ArrayBuffer;
      /** Firmware user-settings name (≤10 chars; the console's, not a
       * per-game thing) — datboi passes the session username. */
      nickname?: string;
    }
  /** ABSOLUTE input state — the worker diffs, so a lost message can
   * never wedge a button down. */
  | { type: 'input'; buttons: number; touch: Touch }
  | { type: 'pause' }
  | { type: 'resume' }
  | { type: 'dispose' }
  | { type: 'stress'; frames: number };

export type WorkerToHost =
  | { type: 'loaded' }
  | { type: 'error'; message: string }
  /** video: stacked screens, RGBA u32, alpha undefined (the presenter
   * owns it); audio: this frame's samples, interleaved L/R f32 at
   * descriptor.audioSampleRate. Both transferred, not copied. */
  | { type: 'frame'; video: ArrayBuffer; audio: Float32Array }
  | { type: 'stressResult'; frames: number; seconds: number };
