//! Cross-compile the vendored 7-Zip C decoder (fetched out-of-tree, see
//! nix/lzma-sdk.nix) + the datboi streaming glue to wasm32 objects and
//! hand them to the final Rust link (D110, same lane as ex-unrar's D58
//! build — but plain C: no libc++, no exceptions, no shim; the decoder's
//! libc surface is malloc/memcpy-class only, all satisfied by wasi-libc's
//! non-importing objects).
//!
//! Why a relocatable pre-link (kept from ex-unrar even without a shim):
//! one combined object keeps the final link's inputs ordered and GC'd
//! identically to the proven extractor lane.
//!
//! The flake sets the toolchain paths in the environment (see flake.nix
//! ex-* wiring). Outside Nix, set them yourself:
//!   DATBOI_WASI_CC           — the wasm32-wasi clang driver
//!   DATBOI_WASI_WASMLD       — the wasm32-wasi wasm-ld
//!   DATBOI_WASI_LIBC_DIR     — dir holding libc.a (wasi-libc)
//!   DATBOI_WASI_BUILTINS_DIR — dir holding libclang_rt.builtins-wasm32.a
//!   DATBOI_LZMA_SRC          — the 7-Zip source's C/ tree (nix/lzma-sdk.nix)
//! When they are absent (a host-native `cargo test` of the pure-Rust units)
//! the build.rs is a no-op — the C only matters for the wasm component.

use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The decoder TUs the streaming glue needs — decode-only: no encoders,
/// no file/thread code. 7zDec is here because SzArEx_Open needs its
/// SzAr_DecodeFolder for COMPRESSED HEADERS (the 7z default), which are
/// small and always LZMA — the in-memory decode is fine there; DATA
/// folders go through csrc/glue.c's streaming decode instead. The
/// filter/PPMd TUs back glue.c's full-coverage folder shapes (D108).
const LZMA_TUS: &[&str] = &[
    "7zArcIn", "7zBuf", "7zCrc", "7zCrcOpt", "7zDec", "7zStream", "Bcj2", "Bra", "Bra86",
    "BraIA64", "CpuArch", "Delta", "LzmaDec", "Lzma2Dec", "Ppmd7", "Ppmd7Dec",
];

fn main() {
    let target = env::var("TARGET").unwrap_or_default();
    if target != "wasm32-unknown-unknown" {
        // Host-native build (unit tests): the C component is irrelevant.
        return;
    }
    let Some(tc) = Toolchain::from_env() else {
        println!(
            "cargo:warning=ex-7z: DATBOI_WASI_* toolchain env not set; \
             the wasm component cannot be built. See build.rs."
        );
        return;
    };

    let out = PathBuf::from(env::var("OUT_DIR").unwrap());
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let vendor = match env::var("DATBOI_LZMA_SRC") {
        Ok(dir) => PathBuf::from(dir),
        Err(_) => {
            println!(
                "cargo:warning=ex-7z: DATBOI_LZMA_SRC not set; the 7-Zip C \
                 source is required to build the wasm component. See nix/lzma-sdk.nix."
            );
            return;
        }
    };
    let csrc = manifest.join("csrc");

    let cflags: Vec<String> = [
        "-O2",
        "-std=c11",
        "-DNDEBUG",
        // Single-thread build of a decode-only TU set; also keeps any
        // stray thread-aware paths compiled out.
        "-DZ7_ST",
        "-Wall",
    ]
    .iter()
    .map(|s| (*s).to_string())
    .collect();

    let mut objs = Vec::new();
    for tu in LZMA_TUS {
        let src = vendor.join(format!("{tu}.c"));
        let obj = out.join(format!("lzma_{tu}.o"));
        tc.compile(&src, &obj, &cflags, &[&vendor]);
        objs.push(obj);
    }
    let glue_obj = out.join("glue.o");
    tc.compile(&csrc.join("glue.c"), &glue_obj, &cflags, &[&vendor]);
    objs.push(glue_obj);

    // Combine into ONE relocatable object (see module doc). Undefined
    // libc symbols (malloc, memcpy, ...) resolve from libc/builtins at
    // the final link.
    let combined = out.join("ex7z.o");
    tc.relocatable_link(&combined, &objs);

    // Ordered positional link-args: the combined object, then the
    // sysroot archives supplying its libc leftovers.
    let libc = tc.libc_dir.join("libc.a");
    let builtins = tc.builtins_dir.join("libclang_rt.builtins-wasm32.a");
    for path in [&combined, &libc, &builtins] {
        println!("cargo:rustc-link-arg={}", path.display());
    }

    // Rebuild triggers.
    println!("cargo:rerun-if-changed=csrc");
    println!("cargo:rerun-if-changed={}", vendor.display());
    for var in [
        "DATBOI_LZMA_SRC",
        "DATBOI_WASI_CC",
        "DATBOI_WASI_WASMLD",
        "DATBOI_WASI_LIBC_DIR",
        "DATBOI_WASI_BUILTINS_DIR",
    ] {
        println!("cargo:rerun-if-env-changed={var}");
    }
}

struct Toolchain {
    cc: String,
    wasmld: String,
    libc_dir: PathBuf,
    builtins_dir: PathBuf,
}

impl Toolchain {
    fn from_env() -> Option<Self> {
        Some(Toolchain {
            cc: env::var("DATBOI_WASI_CC").ok()?,
            wasmld: env::var("DATBOI_WASI_WASMLD").ok()?,
            libc_dir: PathBuf::from(env::var("DATBOI_WASI_LIBC_DIR").ok()?),
            builtins_dir: PathBuf::from(env::var("DATBOI_WASI_BUILTINS_DIR").ok()?),
        })
    }

    fn compile(&self, src: &Path, obj: &Path, flags: &[String], includes: &[&Path]) {
        let mut cmd = Command::new(&self.cc);
        cmd.args(flags);
        for inc in includes {
            cmd.arg("-I").arg(inc);
        }
        cmd.arg("-c").arg(src).arg("-o").arg(obj);
        run(&mut cmd, &format!("compile {}", src.display()));
    }

    fn relocatable_link(&self, out: &Path, objs: &[PathBuf]) {
        let _ = std::fs::remove_file(out);
        let mut cmd = Command::new(&self.wasmld);
        cmd.arg("-r").args(objs).arg("-o").arg(out);
        run(&mut cmd, "relocatable-link ex7z.o");
    }
}

fn run(cmd: &mut Command, what: &str) {
    let status = cmd
        .status()
        .unwrap_or_else(|e| panic!("ex-7z build: failed to spawn ({what}): {e}"));
    assert!(status.success(), "ex-7z build: {what} failed ({status})");
}
