/**
 * The emu worker protocol (D84, docs/88-emulation.md § the host
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
  /** Future BIOS-from-CAS story (88-emulation.md); empty in v1. */
  biosSlots: unknown[];
};

/** Pointer state in native pointer-screen coordinates; null = pen up. */
export type Touch = { x: number; y: number } | null;

export type HostToWorker =
  | { type: 'load'; rom: ArrayBuffer; bios7?: ArrayBuffer; bios9?: ArrayBuffer; firmware?: ArrayBuffer }
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
