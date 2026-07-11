//! Cross-compile the vendored unrar C++ + glue + determinism shim to
//! wasm32 objects and hand them to the final Rust link (D58).
//!
//! The C++ targets wasm32-wasi (the only sysroot with a wasm libc++), the
//! Rust crate targets wasm32-unknown-unknown; both emit wasm32 relocatable
//! objects that `wasm-ld` links together. Zero imports survive because
//! `csrc/shim.cpp` DEFINES every libc symbol unrar can reach, so wasi-libc's
//! importing objects are never pulled in the GC'd final link, and the shim's
//! objects (shim.o, glue.o) are force-included ahead of `-lc` so they win.
//!
//! Why a relocatable pre-link: rust-lld orders build.rs link-arg objects
//! LAST, after `-lc`, so shim.cpp's libc overrides (abort, open, ...) would
//! lose to wasi-libc and duplicate-symbol. Combining all C++ objects into
//! ONE relocatable object first (`wasm-ld -r`, without any sysroot lib)
//! makes shim's definitions the ones that resolve unrar's references, so
//! wasi-libc's importing objects are never pulled in the final GC'd link.
//!
//! The flake sets the toolchain paths in the environment (see
//! flake.nix `ex-unrar` wiring). Outside Nix, set them yourself:
//!   DATBOI_WASI_CXX          — the wasm32-wasi clang++ driver
//!   DATBOI_WASI_WASMLD       — the wasm32-wasi wasm-ld
//!   DATBOI_WASI_LIBCXX_DIR   — dir holding libc++.a / libc++abi.a
//!   DATBOI_WASI_LIBC_DIR     — dir holding libc.a (wasi-libc)
//!   DATBOI_WASI_BUILTINS_DIR — dir holding libclang_rt.builtins-wasm32.a
//! When they are absent (a host-native `cargo test` of the pure-Rust units)
//! the build.rs is a no-op — the C++ only matters for the wasm component.

use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

const UNRAR_TUS: &[&str] = &[
    "strlist", "strfn", "pathfn", "smallfn", "global", "file", "filefn", "filcreat", "archive",
    "arcread", "unicode", "system", "crypt", "crc", "rawread", "encname", "match", "timefn",
    "rdwrfn", "consio", "options", "errhnd", "rarvm", "secpassword", "rijndael", "getbits", "sha1",
    "sha256", "blake2s", "hash", "extinfo", "extract", "volume", "list", "find", "unpack",
    "headers", "threadpool", "rs16", "cmddata", "ui", "filestr", "scantree", "dll", "qopen",
];

fn main() {
    let target = env::var("TARGET").unwrap_or_default();
    if target != "wasm32-unknown-unknown" {
        // Host-native build (unit tests): the C++ component is irrelevant.
        return;
    }
    let Some(tc) = Toolchain::from_env() else {
        // Building the wasm crate without the toolchain env is a
        // configuration error, but we don't want to break `cargo metadata`
        // etc.; emit a clear message and stop.
        println!(
            "cargo:warning=ex-unrar: DATBOI_WASI_* toolchain env not set; \
             the wasm component cannot be built. See build.rs."
        );
        return;
    };

    let out = PathBuf::from(env::var("OUT_DIR").unwrap());
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let vendor = manifest.join("vendor/unrar");
    let csrc = manifest.join("csrc");
    let compat = csrc.join("compat");

    let cxxflags: Vec<String> = [
        "-O2",
        "-std=c++14",
        "-fno-rtti",
        "-fno-exceptions",
        "-DRARDLL",
        "-DSILENT",
        "-D_WASI_EMULATED_SIGNAL",
        "-D_FILE_OFFSET_BITS=64",
        "-D_LARGEFILE_SOURCE",
        "-Wno-switch",
        "-Wno-dangling-else",
        "-Wno-logical-op-parentheses",
    ]
    .iter()
    .map(|s| (*s).to_string())
    .collect();

    let compat_hdr = compat.join("datboi_compat.h");

    // Compile unrar TUs (force-include the compat header for the POSIX bits
    // wasi-libc lacks), plus glue and shim, to objects.
    let mut objs = Vec::new();
    for tu in UNRAR_TUS {
        let src = vendor.join(format!("{tu}.cpp"));
        let obj = out.join(format!("unrar_{tu}.o"));
        tc.compile(
            &src,
            &obj,
            &cxxflags,
            &[&vendor, &compat],
            Some(&compat_hdr),
        );
        objs.push(obj);
    }
    // glue.cpp needs the unrar + compat headers; shim.cpp is freestanding.
    let glue_obj = out.join("glue.o");
    tc.compile(
        &csrc.join("glue.cpp"),
        &glue_obj,
        &cxxflags,
        &[&vendor, &compat],
        Some(&compat_hdr),
    );
    objs.push(glue_obj);
    let shim_obj = out.join("shim.o");
    tc.compile(&csrc.join("shim.cpp"), &shim_obj, &cxxflags, &[], None);
    objs.push(shim_obj);

    // Combine into ONE relocatable object so shim's libc overrides win (see
    // the module doc). No sysroot libs here — undefined C++-runtime symbols
    // (operator new, memcpy, ...) resolve from libc++/libc at the final link.
    let combined = out.join("exunrar.o");
    tc.relocatable_link(&combined, &objs);

    // Pass the combined object AND the sysroot libs as ORDERED positional
    // link-args (not `-l` libs, which rustc places before build.rs args).
    // Order is load-bearing: the combined object comes FIRST, so its
    // abort/open/... definitions are in place before wasi-libc is scanned —
    // libc's conflicting (fd_*-importing) members are then never pulled,
    // and libc/libc++ supply only the C++-runtime symbols (operator new,
    // memcpy, the C++ heap) the combined object leaves undefined.
    let libcxx = tc.libcxx_dir.join("libc++.a");
    let libcxxabi = tc.libcxx_dir.join("libc++abi.a");
    let libc = tc.libc_dir.join("libc.a");
    let builtins = tc.builtins_dir.join("libclang_rt.builtins-wasm32.a");
    for path in [&combined, &libcxx, &libcxxabi, &libc, &builtins] {
        println!("cargo:rustc-link-arg={}", path.display());
    }

    // Rebuild triggers.
    println!("cargo:rerun-if-changed=csrc");
    println!("cargo:rerun-if-changed=vendor/unrar");
    for var in [
        "DATBOI_WASI_CXX",
        "DATBOI_WASI_WASMLD",
        "DATBOI_WASI_LIBCXX_DIR",
        "DATBOI_WASI_LIBC_DIR",
        "DATBOI_WASI_BUILTINS_DIR",
    ] {
        println!("cargo:rerun-if-env-changed={var}");
    }
}

struct Toolchain {
    cxx: String,
    wasmld: String,
    libcxx_dir: PathBuf,
    libc_dir: PathBuf,
    builtins_dir: PathBuf,
}

impl Toolchain {
    fn from_env() -> Option<Self> {
        Some(Toolchain {
            cxx: env::var("DATBOI_WASI_CXX").ok()?,
            wasmld: env::var("DATBOI_WASI_WASMLD").ok()?,
            libcxx_dir: PathBuf::from(env::var("DATBOI_WASI_LIBCXX_DIR").ok()?),
            libc_dir: PathBuf::from(env::var("DATBOI_WASI_LIBC_DIR").ok()?),
            builtins_dir: PathBuf::from(env::var("DATBOI_WASI_BUILTINS_DIR").ok()?),
        })
    }

    fn compile(
        &self,
        src: &Path,
        obj: &Path,
        flags: &[String],
        includes: &[&Path],
        force_include: Option<&Path>,
    ) {
        let mut cmd = Command::new(&self.cxx);
        cmd.args(flags);
        for inc in includes {
            cmd.arg("-I").arg(inc);
        }
        if let Some(fi) = force_include {
            cmd.arg("-include").arg(fi);
        }
        cmd.arg("-c").arg(src).arg("-o").arg(obj);
        run(&mut cmd, &format!("compile {}", src.display()));
    }

    fn relocatable_link(&self, out: &Path, objs: &[PathBuf]) {
        let _ = std::fs::remove_file(out);
        let mut cmd = Command::new(&self.wasmld);
        cmd.arg("-r").args(objs).arg("-o").arg(out);
        run(&mut cmd, "relocatable-link exunrar.o");
    }
}

fn run(cmd: &mut Command, what: &str) {
    let status = cmd
        .status()
        .unwrap_or_else(|e| panic!("ex-unrar build: failed to spawn ({what}): {e}"));
    assert!(status.success(), "ex-unrar build: {what} failed ({status})");
}
