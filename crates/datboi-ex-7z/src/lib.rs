//! ex-7z — the 7z extractor guest for the `datboi:extractor@1` world
//! (D110, following the ex-unrar D58/D89 shape). Thin Rust over the
//! vendored public-domain C decoder plus the datboi streaming folder
//! decode in `csrc/glue.c`: pregenerated bindings (datboi-guest-extractor)
//! for the world's resources, `unsafe` FFI into the `ex7z_*` veneer.
//!
//! Determinism (D5/D46): the compiled component imports NOTHING. The C
//! side's libc surface is malloc/memcpy-class only (satisfied by
//! wasi-libc's non-importing objects at link time); archive I/O rides
//! the host `file` resource and member bytes ride the host `sink`
//! resources through the three `datboi_*` hooks below. Unlike ex-unrar
//! the sink hook is SLOT-INDEXED: the streaming folder decode serves a
//! whole D89 batch in one pass, so member bytes for different sinks
//! interleave within a single `extract` call.
//!
//! Threading model note: a component instance is single-threaded and
//! each export call runs to completion, so the "current file / current
//! sinks" statics below are only ever touched by one in-flight call.

#![cfg_attr(target_arch = "wasm32", no_std)]

#[cfg(target_arch = "wasm32")]
extern crate alloc;

// ---- allocator: one heap, owned by the vendored C (wasi-libc dlmalloc).
// Rust's alloc forwards to C `malloc`/`free` so the two sides never run
// independent allocators over the same linear memory.
#[cfg(target_arch = "wasm32")]
mod allocator {
    use core::alloc::{GlobalAlloc, Layout};

    unsafe extern "C" {
        fn malloc(n: usize) -> *mut u8;
        fn free(p: *mut u8);
        fn realloc(p: *mut u8, n: usize) -> *mut u8;
    }

    struct CMalloc;
    // SAFETY: forwards directly to the C heap; alignment beyond malloc's
    // guarantee (16 on wasm32) is not requested by our small allocations.
    unsafe impl GlobalAlloc for CMalloc {
        unsafe fn alloc(&self, l: Layout) -> *mut u8 {
            unsafe { malloc(l.size()) }
        }
        unsafe fn dealloc(&self, p: *mut u8, _l: Layout) {
            unsafe { free(p) }
        }
        unsafe fn realloc(&self, p: *mut u8, _l: Layout, new: usize) -> *mut u8 {
            unsafe { realloc(p, new) }
        }
    }

    #[global_allocator]
    static ALLOC: CMalloc = CMalloc;

    #[panic_handler]
    fn panic(_: &core::panic::PanicInfo) -> ! {
        // A guest panic is a bug, not a data condition — refuse the archive.
        core::arch::wasm32::unreachable()
    }
}

#[cfg(target_arch = "wasm32")]
#[allow(unsafe_code)]
mod component {
    use alloc::string::String;
    use alloc::vec::Vec;
    use core::ffi::c_void;

    // Bindings come pregenerated from `datboi-guest-extractor` — the
    // same crate external authors get (docs/worlds.md §vending).
    use datboi_guest_extractor::{ExtractRequest, File, Guest, Member, Sink, encode_members};

    unsafe extern "C" {
        fn ex7z_open(err: *mut i32) -> *mut c_void;
        fn ex7z_close(h: *mut c_void);
        fn ex7z_num_files(h: *mut c_void) -> u32;
        fn ex7z_file_info(
            h: *mut c_void,
            i: u32,
            size: *mut u64,
            crc: *mut u32,
            crc_defined: *mut i32,
            is_dir: *mut i32,
            solid: *mut i32,
        );
        fn ex7z_file_name_utf16(h: *mut c_void, i: u32, dest: *mut u16) -> usize;
        fn ex7z_extract(h: *mut c_void, files: *const u32, slots: *const u32, n: u32) -> i32;
    }

    // ---- the bridge: current borrowed resources for the in-flight call ----
    // Raw pointers, valid only for the duration of one export call (the C
    // callbacks fire synchronously inside the ex7z_* calls below).
    static mut CUR_FILE: *const File = core::ptr::null();
    static mut CUR_SINKS: *const Sink = core::ptr::null();
    static mut CUR_SINKS_LEN: usize = 0;

    struct FileGuard;
    impl FileGuard {
        fn set(f: &File) -> Self {
            unsafe { CUR_FILE = f as *const File };
            FileGuard
        }
    }
    impl Drop for FileGuard {
        fn drop(&mut self) {
            unsafe { CUR_FILE = core::ptr::null() };
        }
    }

    struct SinksGuard;
    impl SinksGuard {
        fn set(s: &[Sink]) -> Self {
            unsafe {
                CUR_SINKS = s.as_ptr();
                CUR_SINKS_LEN = s.len();
            }
            SinksGuard
        }
    }
    impl Drop for SinksGuard {
        fn drop(&mut self) {
            unsafe {
                CUR_SINKS = core::ptr::null();
                CUR_SINKS_LEN = 0;
            }
        }
    }

    // ---- hooks the C glue calls ----
    #[unsafe(no_mangle)]
    extern "C" fn datboi_input_len() -> u64 {
        let f = unsafe { CUR_FILE.as_ref() };
        f.map_or(0, File::len)
    }

    #[unsafe(no_mangle)]
    unsafe extern "C" fn datboi_input_read_at(off: u64, buf: *mut u8, n: usize) -> usize {
        // SAFETY: CUR_FILE is a valid borrow for the duration of the
        // ex7z_* call that triggered this callback (set by FileGuard);
        // the C caller owns `buf` for at least `n` bytes.
        unsafe {
            let Some(f) = CUR_FILE.as_ref() else {
                return 0;
            };
            // Host `read-at` returns exactly n unless the range passes EOF.
            let want = u32::try_from(n).unwrap_or(u32::MAX);
            let data = f.read_at(off, want);
            let k = data.len().min(n);
            core::ptr::copy_nonoverlapping(data.as_ptr(), buf, k);
            k
        }
    }

    #[unsafe(no_mangle)]
    unsafe extern "C" fn datboi_sink_write(slot: u32, buf: *const u8, n: usize) {
        // SAFETY: CUR_SINKS is a valid borrow for the duration of the
        // ex7z_extract call (set by SinksGuard); `buf`/`n` come from the
        // C splitter. An out-of-range slot is a glue bug — trap, never
        // misroute bytes.
        unsafe {
            let slot = slot as usize;
            if CUR_SINKS.is_null() || slot >= CUR_SINKS_LEN {
                core::arch::wasm32::unreachable()
            }
            let s = &*CUR_SINKS.add(slot);
            let slice = core::slice::from_raw_parts(buf, n);
            s.write(slice);
        }
    }

    struct Ex;

    /// A refused archive. The error string is diagnostic only — the host
    /// treats any error (or trap) as a whole-archive refusal.
    fn refusal(what: &str, code: i32) -> String {
        let mut s = String::from("7z ");
        s.push_str(what);
        s.push_str(" failed (code ");
        push_i32(&mut s, code);
        s.push(')');
        s
    }

    fn push_i32(s: &mut String, mut v: i32) {
        if v < 0 {
            s.push('-');
            v = -v;
        }
        let mut buf = [0u8; 12];
        let mut i = buf.len();
        loop {
            i -= 1;
            buf[i] = b'0' + (v % 10) as u8;
            v /= 10;
            if v == 0 {
                break;
            }
        }
        for &b in &buf[i..] {
            s.push(b as char);
        }
    }

    /// This component's v1 policy cuts (D89: policy, not ABI): one
    /// container only, no params understood (non-empty params refuse —
    /// params are recipe content).
    fn single_archive(archives: &[File]) -> Result<&File, String> {
        let [archive] = archives else {
            return Err(String::from("multi-volume 7z is unsupported (v1 scope cut)"));
        };
        Ok(archive)
    }

    fn refuse_params(params: &[u8]) -> Result<(), String> {
        if params.is_empty() {
            Ok(())
        } else {
            Err(String::from("ex-7z takes no params"))
        }
    }

    /// RAII over the C handle so every early return closes it.
    struct Handle(*mut c_void);
    impl Handle {
        fn open() -> Result<Self, String> {
            let mut err = 0i32;
            let h = unsafe { ex7z_open(&mut err) };
            if h.is_null() {
                return Err(refusal("open", err));
            }
            Ok(Handle(h))
        }
    }
    impl Drop for Handle {
        fn drop(&mut self) {
            unsafe { ex7z_close(self.0) };
        }
    }

    struct FileInfo {
        size: u64,
        crc: u32,
        is_dir: bool,
        solid: bool,
    }

    fn file_info(h: &Handle, i: u32) -> FileInfo {
        let (mut size, mut crc, mut crc_defined, mut is_dir, mut solid) = (0u64, 0u32, 0, 0, 0);
        unsafe {
            ex7z_file_info(
                h.0,
                i,
                &mut size,
                &mut crc,
                &mut crc_defined,
                &mut is_dir,
                &mut solid,
            );
        }
        FileInfo {
            size,
            crc: if crc_defined != 0 { crc } else { 0 },
            is_dir: is_dir != 0,
            solid: solid != 0,
        }
    }

    fn file_name(h: &Handle, i: u32) -> String {
        let need = unsafe { ex7z_file_name_utf16(h.0, i, core::ptr::null_mut()) };
        let mut buf = alloc::vec![0u16; need];
        unsafe { ex7z_file_name_utf16(h.0, i, buf.as_mut_ptr()) };
        // Drop the NUL; decode UTF-16 (surrogate-aware, lossy like the
        // rar path — `name` is metadata, never an identity).
        let units = buf.strip_suffix(&[0u16]).unwrap_or(&buf);
        // 7z stores paths with '\\' separators on some writers; keep the
        // bytes as-is (the ingest layer treats names as opaque labels).
        char::decode_utf16(units.iter().copied())
            .map(|r| r.unwrap_or('\u{FFFD}'))
            .collect()
    }

    impl Guest for Ex {
        fn enumerate(archives: Vec<File>, params: Vec<u8>) -> Result<Vec<u8>, String> {
            refuse_params(&params)?;
            let archive = single_archive(&archives)?;
            let _fg = FileGuard::set(archive);
            let h = Handle::open()?;
            let num = unsafe { ex7z_num_files(h.0) };
            let mut members = Vec::new();
            let mut ix: u32 = 0;
            for i in 0..num {
                let info = file_info(&h, i);
                if info.is_dir {
                    continue;
                }
                members.push(Member {
                    ix,
                    name: file_name(&h, i),
                    size: info.size,
                    // 7z packs per solid folder, not per member; no
                    // honest per-member figure exists (advisory key).
                    packed_size: 0,
                    crc32: info.crc,
                    solid: info.solid,
                });
                ix += 1;
            }
            Ok(encode_members(&members))
        }

        /// One pass over the archive serves the whole batch — the D89
        /// reshape's point: each solid folder decodes ONCE however many
        /// members are requested (the C splitter routes byte ranges to
        /// slot-indexed sinks). Member bytes stay a pure function of
        /// (container, ix); the request set changes cost only.
        fn extract(
            archives: Vec<File>,
            params: Vec<u8>,
            requests: Vec<ExtractRequest>,
        ) -> Result<(), String> {
            refuse_params(&params)?;
            let archive = single_archive(&archives)?;

            let mut wanted: Vec<(u32, Sink)> = requests.into_iter().map(|r| (r.ix, r.out)).collect();
            wanted.sort_unstable_by_key(|(ix, _)| *ix);
            if wanted.windows(2).any(|w| w[0].0 == w[1].0) {
                return Err(String::from("duplicate member index in extract batch"));
            }
            if wanted.is_empty() {
                return Ok(());
            }

            let _fg = FileGuard::set(archive);
            let h = Handle::open()?;

            // Map member ix (files-only numbering, enumerate's contract)
            // back to db file indices.
            let num = unsafe { ex7z_num_files(h.0) };
            let mut files: Vec<u32> = Vec::with_capacity(wanted.len());
            let mut slots: Vec<u32> = Vec::with_capacity(wanted.len());
            let mut cursor = 0usize;
            let mut ix: u32 = 0;
            for i in 0..num {
                if file_info(&h, i).is_dir {
                    continue;
                }
                if cursor < wanted.len() && wanted[cursor].0 == ix {
                    files.push(i);
                    slots.push(cursor as u32);
                    cursor += 1;
                }
                ix += 1;
            }
            if cursor != wanted.len() {
                return Err(String::from("member index out of range"));
            }

            let sinks: Vec<Sink> = wanted.into_iter().map(|(_, s)| s).collect();
            let _sg = SinksGuard::set(&sinks);
            let r = unsafe { ex7z_extract(h.0, files.as_ptr(), slots.as_ptr(), files.len() as u32) };
            if r != 0 {
                return Err(refusal("member decode", r));
            }
            Ok(())
        }
    }

    datboi_guest_extractor::export!(Ex);
}
