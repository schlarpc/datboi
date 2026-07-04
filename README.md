# datboi

dat/rom management on content-addressed storage: automatic dat updates,
recipe-based storage transformations, scripted output views, and (later)
p2p library sharing between friends.

**Status: design complete, implementation starting (M1).** The design
record lives in [docs/](docs/) — start with [00-vision.md](docs/00-vision.md),
[decisions.md](docs/decisions.md) (D1–D39), and
[90-roadmap.md](docs/90-roadmap.md).

## Layout

- `crates/` — host Rust workspace (daemon, CLI, core, parsers, store,
  wasmtime runtime)
- `transforms/` — separate Rust workspace built for `wasm32-wasip2`;
  `transforms/wit/` holds the `datboi:transform` ABI (draft)
- `docs/` — the design record
- `web/` — Svelte UI (arrives M4)

## Developing

Nix + direnv: `direnv allow`, then `cargo build` / `cargo nextest run`.
Checks: `nix flake check` (build, clippy -D warnings, fmt, nextest, wasm
lane).
