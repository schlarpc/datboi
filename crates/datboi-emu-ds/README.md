# datboi-emu-ds

Nintendo DS browser core (D84, [docs/88-emulation.md](../../docs/88-emulation.md)):
[dust](https://github.com/kelpsyberry/dust)'s `dust-core` behind a
minimal wasm-bindgen surface. Third wasm lane — a web-bundle asset,
not a CAS component: no WIT, no wasmtime, no determinism contract.

- **Build**: `nix build .#emu-ds` (dust is nightly-only; the flake's
  emu lane carries the pinned nightly — the workspace toolchain
  cannot compile this crate, which is why dev builds go through nix).
- **Try it**: serve the build output and open the test page —
  `python3 -m http.server -d result/` then
  `http://localhost:8000/?rom=<same-origin url>` (or pick a file).
- **License**: GPL-3.0-only, inherited from dust and scoped to this
  crate + its wasm/glue output (D84; the workspace stays MIT).
  Upstream is pinned by exact rev in Cargo.toml; patches, when they
  become necessary, follow the ex-unrar posture (fetched + patched,
  provenance in-tree).
- **asset/game_db.json** is dust's game database (gamecode → save
  type), copied verbatim from the pinned rev — the worker uses it to
  give each game its expected in-memory save chip (games hang at boot
  without one). Refresh it when the dust pin moves.
