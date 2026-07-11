# datboi

dat/rom management on content-addressed storage: automatic dat updates,
recipe-based storage transformations, scripted output views, and (later)
p2p library sharing between friends.

**Status: M1–M5 shipped; M6 (friends/p2p) is next.** The design record
lives in [docs/](docs/) — start with [00-vision.md](docs/00-vision.md),
[decisions.md](docs/decisions.md), and
[90-roadmap.md](docs/90-roadmap.md).

## Layout

- `crates/` — host Rust workspace (daemon, CLI, core, parsers, store,
  wasmtime runtime) plus the standalone wasm component crates
  (`datboi-xf-*`, `datboi-ex-*`)
- `wit/` — the `datboi:{transform,extractor}` component ABIs
- `docs/` — the design record
- `web/` — Svelte web UI, embedded into the daemon and served at `/`
  (owner screens + the friend-facing shelves)

## Developing

Nix + direnv: `direnv allow`, then `cargo build` / `cargo nextest run`.
Checks: `nix flake check` (build, clippy -D warnings, fmt, nextest, wasm
lane).
