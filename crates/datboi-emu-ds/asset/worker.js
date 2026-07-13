// The core's side of the datboi emu worker protocol (D84,
// docs/88-emulation.md §"The host contract"). This file ships INSIDE the
// core asset next to descriptor.json — the web host only ever speaks
// postMessage to it, which is also the GPL-3 boundary: everything on
// this side of the Worker is dust-derived, everything on the other side
// is datboi.
//
// Host → worker:
//   {type:"load", rom:ArrayBuffer, bios7?, bios9?, firmware?:ArrayBuffer}
//   {type:"input", buttons:number, touch:{x,y}|null}   // ABSOLUTE state
//   {type:"pause"} {type:"resume"} {type:"dispose"}
//   {type:"stress", frames:number}   // flat-out synchronous run, for CI
// Worker → host:
//   {type:"loaded"} | {type:"error", message}
//   {type:"frame", video:ArrayBuffer, audio:Float32Array}  // transferred
//     video: 256×384 RGBA u32; audio: what this frame produced,
//     interleaved L/R f32 at descriptor.audioSampleRate (~548 pairs)
//   {type:"stressResult", frames, seconds}
//
// Input is absolute (the host reports state, not events) so a missed or
// re-ordered message can never wedge a button down; the diff to dust's
// pressed/released API happens here. Audio rides the frame message (a
// pull API on the wasm side) rather than a wasm-held JS callback — see
// src/audio.rs for the engine bug that design dodges.

import init, { create_emu_state } from "./pkg/datboi_emu_ds.js";

const FRAME_INTERVAL = 1000 / 59.8261; // DS refresh; descriptor.frameRate

let emu = null;
let timer = null;
let expected = 0;
let buttons = 0;
let appliedButtons = 0;
let touch = null;
let appliedTouch = null;

function applyInput() {
  const pressed = buttons & ~appliedButtons;
  const released = appliedButtons & ~buttons;
  if (pressed || released) emu.update_input(pressed, released);
  appliedButtons = buttons;
  if (touch !== appliedTouch) {
    if (touch) emu.update_touch(touch.x, touch.y);
    else emu.update_touch(undefined, undefined);
    appliedTouch = touch;
  }
}

function frame() {
  applyInput();
  const video = emu.run_frame();
  const audio = emu.take_audio();
  postMessage({ type: "frame", video: video.buffer, audio }, [
    video.buffer,
    audio.buffer,
  ]);
}

// Drift-corrected setTimeout pacing (dust-web's FpsLimiter, simplified):
// each tick reschedules against when the frame SHOULD have happened, so
// jitter doesn't accumulate; a stall skips ahead rather than sprinting.
function tick() {
  frame();
  const now = performance.now();
  expected = Math.max(expected + FRAME_INTERVAL, now);
  timer = setTimeout(tick, expected - now);
}

function pause() {
  if (timer !== null) clearTimeout(timer);
  timer = null;
}

function resume() {
  if (timer !== null || !emu) return;
  expected = performance.now();
  tick();
}

self.onmessage = async (e) => {
  const msg = e.data;
  try {
    switch (msg.type) {
      case "load": {
        await init();
        emu = create_emu_state(
          new Uint8Array(msg.rom),
          msg.bios7 && new Uint8Array(msg.bios7),
          msg.bios9 && new Uint8Array(msg.bios9),
          msg.firmware && new Uint8Array(msg.firmware),
        );
        postMessage({ type: "loaded" });
        resume();
        break;
      }
      case "input":
        buttons = msg.buttons;
        touch = msg.touch;
        break;
      case "pause":
        pause();
        break;
      case "resume":
        resume();
        break;
      case "stress": {
        pause();
        const t0 = performance.now();
        for (let i = 0; i < msg.frames - 1; i++) {
          emu.run_frame();
          emu.take_audio(); // keep the pull buffer from growing unbounded
        }
        frame(); // last one goes to the host so it can present it
        const seconds = (performance.now() - t0) / 1000;
        postMessage({ type: "stressResult", frames: msg.frames, seconds });
        break;
      }
      case "dispose":
        pause();
        if (emu) emu.free();
        self.close();
        break;
    }
  } catch (err) {
    pause();
    postMessage({ type: "error", message: String(err?.message ?? err) });
  }
};
