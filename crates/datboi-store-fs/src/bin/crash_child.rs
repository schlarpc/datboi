//! Test-support child for the crash-consistency harness (built only under
//! the `crash-injection` feature). The parent spawns this, then either lets
//! an injection point abort it (`single` mode, precise phase coverage) or
//! `SIGKILL`s it at an arbitrary real moment (`loop` mode). It touches only
//! the public [`Store`] API; the abort happens inside `put_new` when
//! `DATBOI_CRASH_PHASE` is set in this process's environment.
//!
//! Env: `DATBOI_STORE_ROOT` (required), `DATBOI_BLOB_SIZE` (optional).

use std::io::Read;

use datboi_store_fs::{Namespace, Store};

const DEFAULT_SIZE: usize = 512 * 1024;

fn blob_size() -> usize {
    std::env::var("DATBOI_BLOB_SIZE")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_SIZE)
}

/// A never-ending byte source, so `put_new` spends real time in its write
/// loop (maximizing the chance a `SIGKILL` lands mid-write). `seed` varies
/// the bytes so successive loop iterations produce distinct blobs.
struct Pattern {
    seed: u64,
    pos: u64,
}

impl Read for Pattern {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        for byte in buf.iter_mut() {
            *byte = (self.pos ^ self.seed)
                .wrapping_mul(0x9E37_79B9_7F4A_7C15)
                .to_le_bytes()[0];
            self.pos += 1;
        }
        Ok(buf.len())
    }
}

fn deterministic_blob(size: usize, seed: u64) -> Vec<u8> {
    let mut src = Pattern { seed, pos: 0 };
    let mut buf = vec![0u8; size];
    let mut off = 0;
    while off < size {
        off += src.read(&mut buf[off..]).expect("infallible");
    }
    buf
}

fn main() {
    let mode = std::env::args().nth(1).unwrap_or_default();
    let root = std::env::var("DATBOI_STORE_ROOT").expect("DATBOI_STORE_ROOT");
    let store = Store::open(&root).expect("open store");

    match mode.as_str() {
        // Put one deterministic blob; an injection point aborts partway when
        // DATBOI_CRASH_PHASE is set, otherwise this completes and prints the
        // hash for the parent to check.
        "single" => {
            let data = deterministic_blob(blob_size(), 0);
            let (hash, _, _) = store
                .put_new(Namespace::Data, data.as_slice())
                .expect("put_new");
            println!("{hash}");
        }
        // Put distinct blobs forever; the parent SIGKILLs at an arbitrary
        // point. Errors are ignored — the parent is about to kill us.
        "loop" => {
            let size = blob_size();
            let mut seed = u64::from(std::process::id());
            loop {
                seed = seed.wrapping_add(1);
                let data = deterministic_blob(size, seed);
                let _ = store.put_new(Namespace::Data, data.as_slice());
            }
        }
        other => {
            eprintln!("datboi-crash-child: unknown mode {other:?} (want `single` or `loop`)");
            std::process::exit(2);
        }
    }
}
