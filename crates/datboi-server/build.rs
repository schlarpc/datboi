//! D67: the embedded web UI comes from the nix-built `packages.web`
//! dist, never a checked-in artifact — the same shape as the component
//! embedding (D66).
//!
//! Hermetic builds (crane/CI) set `DATBOI_WEB_DIST` and this script
//! only re-exports it. In a dev checkout the variable is unset and the
//! script builds the dist itself via the flake — nix caching makes
//! that near-free, and the `rerun-if-changed` watches mean a web/ edit
//! lands in the next `cargo build` with no shell reloads.
//!
//! Kept in sync by hand with datboi-runtime/build.rs and
//! datboi-ingest/build.rs (same shape, different derivation; a shared
//! build-dependency crate isn't worth the surface).

use std::path::Path;
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

fn main() {
    println!("cargo:rerun-if-env-changed=DATBOI_WEB_DIST");
    let dir = match std::env::var("DATBOI_WEB_DIST") {
        Ok(dir) => dir,
        Err(_) => {
            let manifest = std::env::var("CARGO_MANIFEST_DIR").expect("cargo sets this");
            let repo = Path::new(&manifest)
                .join("../..")
                .canonicalize()
                .expect("repo root");
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
            let out = Command::new("nix")
                .args(["build", ".#web", "--print-out-paths", "--no-link"])
                .current_dir(&repo)
                .output()
                .expect("running `nix build .#web` (D67 dev fallback)");
            assert!(
                out.status.success(),
                "`nix build .#web` failed (D67): set DATBOI_WEB_DIST \
                 or fix the web build\n{}",
                String::from_utf8_lossy(&out.stderr)
            );
            let store = String::from_utf8(out.stdout).expect("utf8 store path");
            store.trim().to_owned()
        }
    };
    println!("cargo:rustc-env=DATBOI_WEB_DIST={dir}");
}
