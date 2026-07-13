//! D67: the embedded web UI comes from the nix-built `packages.web`
//! dist, never a checked-in artifact — the same shape as the component
//! embedding (D66). D79 adds the compiled magic database
//! (`packages.magicdb`, nixpkgs' file(1) magic.mgc) for the blob
//! sniff, on the same rails.
//!
//! Hermetic builds (crane/CI) set `DATBOI_WEB_DIST` / `DATBOI_MAGIC_DB`
//! and this script only re-exports them. In a dev checkout the
//! variables are unset and the script builds the outputs itself via
//! the flake — nix caching makes that near-free, and the
//! `rerun-if-changed` watches mean a web/ edit lands in the next
//! `cargo build` with no shell reloads.
//!
//! Kept in sync by hand with datboi-runtime/build.rs and
//! datboi-ingest/build.rs (same shape, different derivation; a shared
//! build-dependency crate isn't worth the surface).

use std::path::{Path, PathBuf};
use std::process::Command;

/// The web/ inputs that shape the dist (a dir entry recurses).
/// node_modules and dist stay out — outputs, not inputs.
const WEB_INPUTS: &[&str] = &[
    "src",
    "index.html",
    "package.json",
    "package-lock.json",
    "vite.config.ts",
    "wuchale.config.js",
    "tsconfig.json",
];

fn repo_root() -> PathBuf {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").expect("cargo sets this");
    Path::new(&manifest)
        .join("../..")
        .canonicalize()
        .expect("repo root")
}

/// Env-or-flake resolution: hermetic builds set `var`; a dev checkout
/// builds `installable` itself.
fn env_or_flake(var: &str, installable: &str) -> String {
    println!("cargo:rerun-if-env-changed={var}");
    match std::env::var(var) {
        Ok(path) => path,
        Err(_) => {
            let repo = repo_root();
            let out = Command::new("nix")
                .args(["build", installable, "--print-out-paths", "--no-link"])
                .current_dir(&repo)
                .output()
                .unwrap_or_else(|e| panic!("running `nix build {installable}`: {e}"));
            assert!(
                out.status.success(),
                "`nix build {installable}` failed: set {var} or fix the build\n{}",
                String::from_utf8_lossy(&out.stderr)
            );
            let store = String::from_utf8(out.stdout).expect("utf8 store path");
            store.trim().to_owned()
        }
    }
}

fn main() {
    let repo = repo_root();
    if std::env::var("DATBOI_WEB_DIST").is_err() {
        for input in WEB_INPUTS {
            println!(
                "cargo:rerun-if-changed={}",
                repo.join("web").join(input).display()
            );
        }
        println!(
            "cargo:rerun-if-changed={}",
            repo.join("flake.nix").display()
        );
    }
    let dist = env_or_flake("DATBOI_WEB_DIST", ".#web");
    println!("cargo:rustc-env=DATBOI_WEB_DIST={dist}");

    // D79: the blob sniff embeds the compiled magic database (an
    // external artifact — no source watches; the flake pin moves it).
    let magic = env_or_flake("DATBOI_MAGIC_DB", ".#magicdb");
    println!("cargo:rustc-env=DATBOI_MAGIC_DB={magic}");
}
