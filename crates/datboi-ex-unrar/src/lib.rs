//! ex-unrar — the RAR extractor guest for the `datboi:extractor@1` world
//! (D58 pathfinder, D89 shape). Thin Rust over the vendored unrar C++
//! staticlib: pregenerated bindings (datboi-guest-extractor) for
//! the world's resource bindings, plus `unsafe` FFI into the `extern "C"`
//! veneer in `csrc/glue.cpp`.
//!
//! Determinism (D5/D46): the compiled component imports NOTHING. The C++
//! side's libc calls are all satisfied by `csrc/shim.cpp` (see its header);
//! archive I/O reroutes onto the host `file` resource and member bytes onto
//! the host `sink` resource through the three `datboi_*` hooks below, which
//! this guest implements against the WIT resources passed to each call.
//!
//! Threading model note: a component instance is single-threaded and each
//! export call runs to completion, so the "current file / current sink"
//! statics below are only ever touched by one in-flight call. They are the
//! bridge between the C++ callbacks (which have no user-data channel we can
//! thread a Rust reference through cleanly) and the borrowed WIT resources.

#![cfg_attr(target_arch = "wasm32", no_std)]

#[cfg(target_arch = "wasm32")]
extern crate alloc;

// ---- allocator: one heap, owned by the vendored C++ (wasi-libc dlmalloc).
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
            malloc(l.size())
        }
        unsafe fn dealloc(&self, p: *mut u8, _l: Layout) {
            free(p);
        }
        unsafe fn realloc(&self, p: *mut u8, _l: Layout, new: usize) -> *mut u8 {
            realloc(p, new)
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

    // ---- open modes / ops / flags mirrored from dll.hpp ----
    const RAR_OM_LIST: u32 = 0;
    const RAR_OM_EXTRACT: u32 = 1;
    const RAR_SKIP: i32 = 0;
    const RAR_TEST: i32 = 1;

    const RHDF_ENCRYPTED: u32 = 0x04;
    const RHDF_DIRECTORY: u32 = 0x20;
    const ROADF_VOLUME: u32 = 0x0001;
    const ROADF_ENCHEADERS: u32 = 0x0080;
    const RAR_HASH_CRC32: u32 = 1;

    // Mirror of `struct ExHeader` in csrc/glue.cpp — keep in sync.
    #[repr(C)]
    struct ExHeader {
        unp_size: u64,
        pack_size: u64,
        flags: u32,
        file_crc: u32,
        hash_type: u32,
        redir_type: u32,
        name_len: u32,
        name: [u32; 1024],
    }

    impl ExHeader {
        const fn zeroed() -> Self {
            ExHeader {
                unp_size: 0,
                pack_size: 0,
                flags: 0,
                file_crc: 0,
                hash_type: 0,
                redir_type: 0,
                name_len: 0,
                name: [0; 1024],
            }
        }
        fn is_directory(&self) -> bool {
            self.flags & RHDF_DIRECTORY != 0
        }
        fn is_encrypted(&self) -> bool {
            self.flags & RHDF_ENCRYPTED != 0
        }
        // A plain file member (not a link / NTFS-stream redirect).
        fn is_plain_file(&self) -> bool {
            !self.is_directory() && self.redir_type == 0
        }
        fn name_utf8(&self) -> String {
            let mut s = String::new();
            for &u in &self.name[..self.name_len as usize] {
                s.push(char::from_u32(u).unwrap_or('\u{FFFD}'));
            }
            s
        }
    }

    unsafe extern "C" {
        fn ex_open(mode: u32, arc_flags: *mut u32, err: *mut i32) -> *mut c_void;
        fn ex_read_header(h: *mut c_void, out: *mut ExHeader) -> i32;
        fn ex_process(h: *mut c_void, op: i32) -> i32;
        fn ex_close(h: *mut c_void) -> i32;
    }

    // ---- the bridge: current borrowed resources for the in-flight call ----
    // Raw pointers, valid only for the duration of one export call (the C++
    // callbacks fire synchronously inside the ex_* calls below).
    static mut CUR_FILE: *const File = core::ptr::null();
    static mut CUR_SINK: *const Sink = core::ptr::null();

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

    struct SinkGuard;
    impl SinkGuard {
        fn set(s: &Sink) -> Self {
            unsafe { CUR_SINK = s as *const Sink };
            SinkGuard
        }
    }
    impl Drop for SinkGuard {
        fn drop(&mut self) {
            unsafe { CUR_SINK = core::ptr::null() };
        }
    }

    // ---- hooks the C++ shim/glue call ----
    #[unsafe(no_mangle)]
    extern "C" fn datboi_input_len() -> u64 {
        let f = unsafe { CUR_FILE.as_ref() };
        f.map_or(0, File::len)
    }

    #[unsafe(no_mangle)]
    unsafe extern "C" fn datboi_input_read_at(off: u64, buf: *mut u8, n: usize) -> usize {
        // SAFETY: CUR_FILE is a valid borrow for the duration of the ex_*
        // call that triggered this callback (set by FileGuard); the C++
        // caller owns `buf` for at least `n` bytes.
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
    unsafe extern "C" fn datboi_sink_write(buf: *const u8, n: usize) {
        // SAFETY: CUR_SINK is a valid borrow for the duration of the matched
        // RAR_TEST call (set by SinkGuard); `buf`/`n` come straight from
        // unrar's UCM_PROCESSDATA callback.
        unsafe {
            if let Some(s) = CUR_SINK.as_ref() {
                let slice = core::slice::from_raw_parts(buf, n);
                s.write(slice);
            }
        }
    }

    struct Ex;

    // A refused archive: encrypted, multi-volume, or unparseable. The error
    // string is diagnostic only — the host treats any error (or trap) as a
    // whole-archive refusal.
    fn open_or_refuse(mode: u32) -> Result<(*mut c_void, u32), String> {
        let mut arc_flags = 0u32;
        let mut err = 0i32;
        let h = unsafe { ex_open(mode, &mut arc_flags, &mut err) };
        if h.is_null() {
            return Err(refusal(err));
        }
        if arc_flags & ROADF_VOLUME != 0 {
            unsafe { ex_close(h) };
            return Err(String::from("multi-volume rar is unsupported (v1 scope cut)"));
        }
        if arc_flags & ROADF_ENCHEADERS != 0 {
            unsafe { ex_close(h) };
            return Err(String::from("header-encrypted rar is unsupported (v1 scope cut)"));
        }
        Ok((h, arc_flags))
    }

    fn refusal(err: i32) -> String {
        let mut s = String::from("rar open failed (ERAR ");
        // tiny int format without std
        push_i32(&mut s, err);
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
    /// container only (multi-volume refuses), no params understood
    /// (non-empty params refuse — params are recipe content).
    fn single_archive(archives: &[File]) -> Result<&File, String> {
        let [archive] = archives else {
            return Err(String::from(
                "multi-volume rar is unsupported (v1 scope cut)",
            ));
        };
        Ok(archive)
    }

    fn refuse_params(params: &[u8]) -> Result<(), String> {
        if params.is_empty() {
            Ok(())
        } else {
            Err(String::from("ex-unrar takes no params"))
        }
    }

    impl Guest for Ex {
        fn enumerate(archives: Vec<File>, params: Vec<u8>) -> Result<Vec<u8>, String> {
            refuse_params(&params)?;
            let archive = single_archive(&archives)?;
            let _fg = FileGuard::set(archive);
            let (h, _flags) = open_or_refuse(RAR_OM_LIST)?;
            let mut members = Vec::new();
            let mut hd = ExHeader::zeroed();
            let mut ix: u32 = 0;
            loop {
                let r = unsafe { ex_read_header(h, &mut hd) };
                if r == 1 {
                    break; // end of archive
                }
                if r != 0 {
                    unsafe { ex_close(h) };
                    return Err(header_error(r));
                }
                if hd.is_encrypted() {
                    unsafe { ex_close(h) };
                    return Err(String::from("encrypted member is unsupported (v1 scope cut)"));
                }
                if hd.is_plain_file() {
                    members.push(Member {
                        ix,
                        name: hd.name_utf8(),
                        size: hd.unp_size,
                        packed_size: hd.pack_size,
                        crc32: if hd.hash_type == RAR_HASH_CRC32 {
                            hd.file_crc
                        } else {
                            0
                        },
                        solid: false, // set by extract-time cost, not identity
                    });
                    ix += 1;
                }
                // Advance past the member (SKIP: unrar decodes internally
                // where solidity requires it; cost, not semantics).
                let p = unsafe { ex_process(h, RAR_SKIP) };
                if p != 0 {
                    unsafe { ex_close(h) };
                    return Err(process_error(p));
                }
            }
            unsafe { ex_close(h) };
            Ok(encode_members(&members))
        }

        /// One pass over the archive serves the whole batch — the D89
        /// reshape's point: each solid block decodes ONCE however many
        /// members are requested. Member bytes stay a pure function of
        /// (container, ix); the request set changes cost only.
        fn extract(
            archives: Vec<File>,
            params: Vec<u8>,
            requests: Vec<ExtractRequest>,
        ) -> Result<(), String> {
            refuse_params(&params)?;
            let archive = single_archive(&archives)?;

            // Sort by ix so matching is a cursor walk; duplicate targets
            // are ambiguous (two sinks, one member) and refuse.
            let mut wanted: Vec<(u32, Sink)> =
                requests.into_iter().map(|r| (r.ix, r.out)).collect();
            wanted.sort_unstable_by_key(|(ix, _)| *ix);
            if wanted.windows(2).any(|w| w[0].0 == w[1].0) {
                return Err(String::from("duplicate member index in extract batch"));
            }
            if wanted.is_empty() {
                return Ok(());
            }

            let _fg = FileGuard::set(archive);
            let (h, _flags) = open_or_refuse(RAR_OM_EXTRACT)?;
            let mut hd = ExHeader::zeroed();
            let mut ix: u32 = 0;
            let mut next = 0usize; // cursor into `wanted`
            loop {
                let r = unsafe { ex_read_header(h, &mut hd) };
                if r == 1 {
                    unsafe { ex_close(h) };
                    return Err(String::from("member index out of range"));
                }
                if r != 0 {
                    unsafe { ex_close(h) };
                    return Err(header_error(r));
                }
                if hd.is_encrypted() {
                    unsafe { ex_close(h) };
                    return Err(String::from("encrypted member is unsupported (v1 scope cut)"));
                }
                let is_file = hd.is_plain_file();
                let matched = is_file && ix == wanted[next].0;
                let op = if matched { RAR_TEST } else { RAR_SKIP };
                // The sink only receives bytes during a matched TEST.
                let p = if matched {
                    let _sg = SinkGuard::set(&wanted[next].1);
                    unsafe { ex_process(h, op) }
                } else {
                    unsafe { ex_process(h, op) }
                };
                if p != 0 {
                    unsafe { ex_close(h) };
                    return Err(process_error(p));
                }
                if matched {
                    next += 1;
                    if next == wanted.len() {
                        unsafe { ex_close(h) };
                        return Ok(());
                    }
                }
                if is_file {
                    ix += 1;
                }
            }
        }
    }

    fn header_error(code: i32) -> String {
        let mut s = String::from("rar header read failed (ERAR ");
        push_i32(&mut s, code);
        s.push(')');
        s
    }

    fn process_error(code: i32) -> String {
        let mut s = String::from("rar member decode failed (ERAR ");
        push_i32(&mut s, code);
        s.push(')');
        s
    }

    datboi_guest_extractor::export!(Ex);
}
