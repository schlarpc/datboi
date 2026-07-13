//! Audio backend: accumulates mixer output in a plain buffer that
//! `EmuState::take_audio` drains after each frame — a pull API, matching
//! run_frame's shape, so frame + audio ride one worker message.
//!
//! Deliberately NOT dust-web's callback design (a js_sys::Function held
//! inside the emulator): passing a JS closure into the wasm instance
//! hung `create_emu_state` inside a Web Worker on Chromium 148 headless
//! (fine on the main thread, fine under an attached debugger — an
//! engine-side heisenbug, see docs/88-emulation.md). No JS value crosses
//! into wasm this way, which kills the whole bug class.
//!
//! The DS mixer emits a sample pair every 1024 cycles — 32768 Hz — in
//! 512-pair chunks; `OutputSample` is a u16 in 0..1024, remapped here to
//! f32 in [-1, 1), interleaved L/R.

use dust_core::audio::OutputSample;
use std::{cell::RefCell, rc::Rc};

pub type SharedBuffer = Rc<RefCell<Vec<f32>>>;

pub struct Backend {
    buffer: SharedBuffer,
}

impl Backend {
    pub fn new(buffer: SharedBuffer) -> Self {
        Backend { buffer }
    }
}

impl dust_core::audio::Backend for Backend {
    fn handle_sample_chunk(&mut self, samples: &mut Vec<[OutputSample; 2]>) {
        let mut buf = self.buffer.borrow_mut();
        buf.reserve(samples.len() * 2);
        for [l, r] in samples.drain(..) {
            buf.push(l as f32 * (1.0 / 512.0) - 1.0);
            buf.push(r as f32 * (1.0 / 512.0) - 1.0);
        }
    }
}
