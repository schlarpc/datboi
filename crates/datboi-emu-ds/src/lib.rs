//! DS browser core (D84, docs/88-emulation.md): `dust-core` behind the
//! smallest wasm-bindgen surface that can boot and run — create from ROM
//! bytes, pump frames, feed input. Cribbed from dust's own web frontend
//! crate (frontend/web/crate) with the v1 exclusions applied: no save
//! device, no save import/export, no BIOS/firmware persistence. Audio is
//! a pull API (take_audio drains what run_frame accumulated — see
//! audio.rs for why it is not dust-web's callback design). The worker
//! protocol itself lives in asset/worker.js — this crate is just the
//! compute surface it wraps.
//!
//! Threading: none. dust-web renders 3D in a second worker over shared
//! wasm memory (+atomics, -Zbuild-std); we run the software rasterizer
//! synchronously in-instance instead (see renderer_3d.rs), so this module
//! needs no SharedArrayBuffer, no COOP/COEP, and no build-std.

mod audio;
mod renderer_3d;

use dust_core::{
    cpu::{arm7, arm9, interpreter::Interpreter},
    ds_slot,
    emu::{self, input::Keys, Emu},
    flash::Flash,
    gpu::{SCREEN_HEIGHT, SCREEN_WIDTH},
    rtc,
    spi::firmware,
    utils::{zeroed_box, BoxedByteSlice, Bytes},
    Model, SaveContents,
};
use js_sys::{Float32Array, Uint32Array, Uint8Array};
use wasm_bindgen::prelude::*;

#[wasm_bindgen]
pub struct EmuState {
    emu: Emu<Interpreter>,
    audio: audio::SharedBuffer,
}

#[allow(non_snake_case)]
#[wasm_bindgen]
impl EmuState {
    /// Keys bitmask per dust_core::emu::input::Keys: A=1, B=2, SELECT=4,
    /// START=8, RIGHT=16, LEFT=32, UP=64, DOWN=128, R=256, L=512,
    /// X=1<<16, Y=1<<17. The test page and the (milestone 3) worker
    /// protocol both speak this encoding.
    pub fn update_input(&mut self, pressed: u32, released: u32) {
        self.emu.press_keys(Keys::from_bits_truncate(pressed));
        self.emu.release_keys(Keys::from_bits_truncate(released));
    }

    /// Bottom-screen touch in native 256×192 coordinates; None = pen up.
    pub fn update_touch(&mut self, x: Option<u16>, y: Option<u16>) {
        if let Some((x, y)) = x.zip(y) {
            self.emu.set_touch_pos([x, y]);
        } else {
            self.emu.end_touch();
        }
    }

    /// Runs one frame and returns both screens, top then bottom, as
    /// 256×384 RGBA u32 pixels (alpha undefined — the presenter owns it).
    pub fn run_frame(&mut self) -> Uint32Array {
        self.emu.run();
        Uint32Array::from(unsafe {
            core::slice::from_raw_parts(
                self.emu.gpu.renderer_2d().framebuffer().as_ptr() as *const u32,
                SCREEN_WIDTH * SCREEN_HEIGHT * 2,
            )
        })
    }

    /// Drains the audio run_frame accumulated: interleaved L/R f32 at
    /// 32768 Hz (descriptor.audioSampleRate). ~548 pairs per frame.
    pub fn take_audio(&mut self) -> Float32Array {
        let mut buf = self.audio.borrow_mut();
        let arr = Float32Array::from(&buf[..]);
        buf.clear();
        arr
    }
}

/// The in-memory save device for a dust game_db save-type string. A
/// game without its expected save chip can hang at boot probing for it
/// (MKDS sat on white screens forever) — so this is emulation
/// completeness, NOT the out-of-scope persistence story: contents are
/// fresh zeroes every session and evaporate on close. `has_ir` marks
/// the infrared cart family (gamecode 'I…'), which gates the flash
/// wiring the same way dust-web does. Unknown/nand types fall back to
/// Empty (dust has no NAND save support yet either).
fn ds_slot_spi(save_type: Option<&str>, has_ir: bool) -> Result<ds_slot::spi::Spi, JsError> {
    use ds_slot::spi;
    let eeprom_fram = |len: usize| {
        spi::eeprom_fram::EepromFram::new(SaveContents::New(len), None)
            .map(Into::into)
            .map_err(|_| JsError::new("couldn't create EEPROM/FRAM save device"))
    };
    let flash = |len: usize| {
        spi::flash::Flash::new(SaveContents::New(len), [0; 20], has_ir)
            .map(Into::into)
            .map_err(|_| JsError::new("couldn't create FLASH save device"))
    };
    match save_type {
        Some("eeprom-4k") => spi::eeprom_4k::Eeprom4k::new(SaveContents::New(0x200), None)
            .map(Into::into)
            .map_err(|_| JsError::new("couldn't create EEPROM save device")),
        Some("eeprom-fram-64k") => eeprom_fram(0x2000),
        Some("eeprom-fram-512k") => eeprom_fram(0x1_0000),
        Some("eeprom-fram-1m") => eeprom_fram(0x2_0000),
        Some("flash-2m") => flash(0x4_0000),
        Some("flash-4m") => flash(0x8_0000),
        Some("flash-8m") => flash(0x10_0000),
        _ => Ok(spi::Empty::new().into()),
    }
}

/// BIOS/firmware are optional (docs/88-emulation.md: v1 ships no BIOS
/// story) — dust's HLE BIOS direct-boots decrypted dumps with nothing.
/// KEY1-encrypted secure-area dumps are the one case that genuinely needs
/// real BIOS bytes; dust reports that as a build error we surface below.
/// `save_type` speaks the dust game_db vocabulary; the worker looks it
/// up by gamecode (asset/game_db.json) before calling in.
#[wasm_bindgen]
pub fn create_emu_state(
    rom_arr: Uint8Array,
    arm7_bios_arr: Option<Uint8Array>,
    arm9_bios_arr: Option<Uint8Array>,
    firmware_arr: Option<Uint8Array>,
    save_type: Option<String>,
    has_ir: bool,
) -> Result<EmuState, JsError> {
    console_error_panic_hook::set_once();

    let model = Model::Lite;

    let arm7_bios = arm7_bios_arr.map(|arr| {
        let mut buf = zeroed_box::<Bytes<{ arm7::BIOS_SIZE }>>();
        arr.copy_to(&mut **buf);
        buf
    });
    let arm9_bios = arm9_bios_arr.map(|arr| {
        let mut buf = zeroed_box::<Bytes<{ arm9::BIOS_SIZE }>>();
        arr.copy_to(&mut **buf);
        buf
    });

    let firmware = firmware_arr
        .map(|arr| {
            let mut buf = BoxedByteSlice::new_zeroed(arr.length() as usize);
            arr.copy_to(&mut buf);
            buf
        })
        .unwrap_or_else(|| firmware::default(model));

    // The DS cart bus addresses power-of-two sizes; pad with zeros like
    // every dumper (and dust-web) does.
    let mut rom = BoxedByteSlice::new_zeroed(rom_arr.length().next_power_of_two() as usize);
    rom_arr.copy_to(&mut rom[..rom_arr.length() as usize]);
    if !ds_slot::rom::is_valid_size(rom.len() as u64, model) {
        return Err(JsError::new("invalid ROM size for a DS cart"));
    }

    let (tx_3d, rx_3d) = renderer_3d::init();
    let audio_buffer: audio::SharedBuffer = Default::default();

    let mut emu_builder = emu::Builder::new(
        Flash::new(
            SaveContents::Existing(firmware),
            firmware::id_for_model(model),
        )
        .map_err(|_| JsError::new("invalid firmware contents"))?,
        Some(Box::new(rom)),
        ds_slot_spi(save_type.as_deref(), has_ir)?,
        Box::new(audio::Backend::new(audio_buffer.clone())),
        None,
        Box::new(rtc::DummyBackend),
        Box::new(dust_soft_2d::sync::Renderer::new(Box::new(rx_3d))),
        Box::new(tx_3d),
        None,
    );

    emu_builder.arm7_bios = arm7_bios;
    emu_builder.arm9_bios = arm9_bios;
    emu_builder.model = model;
    emu_builder.direct_boot = true;

    match emu_builder.build(Interpreter) {
        Ok(emu) => Ok(EmuState {
            emu,
            audio: audio_buffer,
        }),
        Err(emu::BuildError::RomNeedsDecryptionButNoBiosProvided) => Err(JsError::new(
            "this dump has an encrypted secure area, which needs real BIOS files — \
             provide them or use a decrypted dump",
        )),
        Err(_) => Err(JsError::new("couldn't start the emulator")),
    }
}
