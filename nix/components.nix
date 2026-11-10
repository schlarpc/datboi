# The wasm component crates (crates/datboi-{xf,ex}-*): each is a
# STANDALONE Cargo workspace with its own lockfile (D54), so each is its
# own breakbulk workspace, named after the crate. Shared by nix/rust.nix
# (which configures them) and flake.nix (which joins their products into
# packages.transforms).
{ lib }:
rec {
  names = [
    "datboi-xf-cso"
    "datboi-xf-ecm"
    "datboi-xf-preflate"
    "datboi-xf-reference"
    "datboi-xf-reference-stream"
    "datboi-xf-xgd1-prng"
    "datboi-xf-gc-junk"
    "datboi-xf-wii-crypt"
    "datboi-ex-unrar"
    "datboi-ex-7z"
  ];

  # Two lanes by crate prefix (D58/D89): `datboi-xf-` = datboi:transform,
  # `datboi-ex-` = datboi:extractor (which also cross-compiles vendored C).
  isExtractor = crate: lib.hasPrefix "datboi-ex-" crate;

  # Which pregen-bindings crate a lane consumes (D89, docs/worlds.md §vending).
  guestCrateFor = crate:
    if isExtractor crate then "datboi-guest-extractor" else "datboi-guest-transform";

  wasmTriple = "wasm32-unknown-unknown";

  # The stamped component a workspace produces (its default target is
  # wasm32, so the shortcut names the stamped product).
  productOf = repo: crate: repo.workspaces.${crate}.packages.${crate};
}
