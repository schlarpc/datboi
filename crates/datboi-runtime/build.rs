//! D66: embedded components come from the nix-built transform
//! derivations, never a checked-in artifact.
//!
//! Hermetic builds (crane/CI) set `DATBOI_COMPONENTS_DIR` and this
//! script only re-exports it. In a dev checkout the variable is unset
//! and the script builds the components itself via the flake — nix
//! caching makes that near-free, and the `rerun-if-changed` watches
//! mean an edit to any component crate lands in the next `cargo build`
//! with no shell reloads.
//!
//! Kept in sync by hand with datboi-ingest/build.rs (same ~30 lines;
//! a shared build-dependency crate isn't worth the surface).

use std::path::Path;
use std::process::Command;

const COMPONENT_CRATES: &[&str] = &[
    // The lanes' pregen-bindings crates first (D89): their source shapes
    // every component's bytes, so an edit must trigger the rebuild too.
    "datboi-guest-extractor",
    "datboi-guest-transform",
    "datboi-xf-cso",
    "datboi-xf-ecm",
    "datboi-xf-preflate",
    "datboi-xf-reference",
    "datboi-xf-reference-stream",
    "datboi-ex-unrar",
];

fn main() {
    println!("cargo:rerun-if-env-changed=DATBOI_COMPONENTS_DIR");
    let dir = match std::env::var("DATBOI_COMPONENTS_DIR") {
        Ok(dir) => dir,
        Err(_) => {
            let manifest = std::env::var("CARGO_MANIFEST_DIR").expect("cargo sets this");
            let repo = Path::new(&manifest)
                .join("../..")
                .canonicalize()
                .expect("repo root");
            for krate in COMPONENT_CRATES {
                println!(
                    "cargo:rerun-if-changed={}",
                    repo.join("crates").join(krate).display()
                );
            }
            println!("cargo:rerun-if-changed={}", repo.join("wit").display());
            println!(
                "cargo:rerun-if-changed={}",
                repo.join("flake.nix").display()
            );
            let out = Command::new("nix")
                .args(["build", ".#transforms", "--print-out-paths", "--no-link"])
                .current_dir(&repo)
                .output()
                .expect("running `nix build .#transforms` (D66 dev fallback)");
            assert!(
                out.status.success(),
                "`nix build .#transforms` failed (D66): set DATBOI_COMPONENTS_DIR \
                 or fix the component build\n{}",
                String::from_utf8_lossy(&out.stderr)
            );
            let store = String::from_utf8(out.stdout).expect("utf8 store path");
            format!("{}/lib", store.trim())
        }
    };
    println!("cargo:rustc-env=DATBOI_COMPONENTS_DIR={dir}");
}
