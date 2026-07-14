//! Synchronous in-instance 3D renderer. dust-web splits the software
//! rasterizer into a second worker sharing wasm memory (spin-loops on
//! atomics → +atomics, -Zbuild-std, SharedArrayBuffer, COOP/COEP); for
//! v1 we trade that parallelism away and rasterize the whole frame
//! eagerly inside `start_rendering`, on the emulator thread. Same
//! Tx/Rx trait pair, no threads anywhere (docs/emulation.md).
//!
//! The static + UnsafeCell shape mirrors dust-web's: the renderer halves
//! are handed to dust-core as `Box<dyn ...>` with Send bounds, which an
//! Rc-shared pair can't satisfy. Sound because this wasm instance is
//! single-threaded and the borrows below never escape a call: Tx writes
//! (emulation), then Rx reads (2D compositing) — strictly sequential.

use dust_core::{
    gpu::{
        engine_3d::{
            Polygon, RendererTx, RenderingState as CoreRenderingState, ScreenVertex,
            SoftRendererRx,
        },
        Scanline, SCREEN_HEIGHT,
    },
    utils::Bytes,
};
use dust_soft_3d::{Renderer, RenderingData};
use std::{cell::UnsafeCell, sync::OnceLock};

struct SharedData {
    rendering_data: Box<UnsafeCell<RenderingData>>,
    renderer: Box<UnsafeCell<Renderer>>,
    scanline_buffer: Box<UnsafeCell<[Scanline<u32>; SCREEN_HEIGHT]>>,
}

// Single-threaded wasm: there is no second thread to race with.
unsafe impl Sync for SharedData {}

static SHARED_DATA: OnceLock<SharedData> = OnceLock::new();

macro_rules! shared_data {
    () => {
        unsafe { SHARED_DATA.get().unwrap_unchecked() }
    };
}

pub struct Tx;

impl RendererTx for Tx {
    fn set_capture_enabled(&mut self, _capture_enabled: bool) {}

    fn swap_buffers(
        &mut self,
        vert_ram: &[ScreenVertex],
        poly_ram: &[Polygon],
        state: &CoreRenderingState,
    ) {
        unsafe { &mut *shared_data!().rendering_data.get() }.prepare(vert_ram, poly_ram, state);
    }

    fn repeat_last_frame(&mut self, state: &CoreRenderingState) {
        unsafe { &mut *shared_data!().rendering_data.get() }.repeat_last_frame(state);
    }

    fn start_rendering(
        &mut self,
        texture: &Bytes<0x8_0000>,
        tex_pal: &Bytes<0x1_8000>,
        state: &CoreRenderingState,
    ) {
        let shared = shared_data!();
        let rendering_data = unsafe { &mut *shared.rendering_data.get() };
        rendering_data.copy_vram(texture, tex_pal, state);

        // The whole frame, now — dust-web's run_worker loop body, minus
        // the cross-worker handshake (render_line leads postprocess_line
        // by one: the rasterizer's edge state wants it).
        let renderer = unsafe { &mut *shared.renderer.get() };
        let rendering_data = unsafe { &*shared.rendering_data.get() };
        let scanlines = unsafe { &mut *shared.scanline_buffer.get() };
        renderer.start_frame(rendering_data);
        renderer.render_line(0, rendering_data);
        for y in 0..SCREEN_HEIGHT as u8 {
            if (y as usize) < SCREEN_HEIGHT - 1 {
                renderer.render_line(y + 1, rendering_data);
            }
            renderer.postprocess_line(y, &mut scanlines[y as usize], rendering_data);
        }
    }

    fn skip_rendering(&mut self) {}
}

pub struct Rx {
    next_scanline: usize,
}

impl SoftRendererRx for Rx {
    fn start_frame(&mut self) {
        self.next_scanline = 0;
    }

    fn read_scanline(&mut self) -> &Scanline<u32> {
        let result = unsafe { &(*shared_data!().scanline_buffer.get())[self.next_scanline] };
        self.next_scanline += 1;
        result
    }

    fn skip_scanline(&mut self) {
        self.next_scanline += 1;
    }
}

pub fn init() -> (Tx, Rx) {
    SHARED_DATA.get_or_init(|| unsafe {
        SharedData {
            rendering_data: Box::new_zeroed().assume_init(),
            renderer: Box::new(UnsafeCell::new(Renderer::new())),
            scanline_buffer: Box::new_zeroed().assume_init(),
        }
    });
    (Tx, Rx { next_scanline: 0 })
}
