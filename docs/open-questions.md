# Open questions & active research

Design passes R1–R8 complete; core design ratified through D39. Docs
00–90 are the record.

## Open (minor / deferred to build-time)

- Ingest-policy config vocabulary, detector registry (ordering /
  confidence beyond skipper XMLs), canonical-orientation preference per
  swap/header family: deliberately molten until a second real analyzer
  exists to generalize from (M3, post-D50). Fixpoint/provenance/dat-blindness
  principles are ratified (D45/D47/D48); only the config surface waits.

- Shard fanout + inline-outboard threshold: frozen by the M1 NFS
  benchmark (spec in 90-roadmap.md), not by discussion.
- State snapshot cadence + exact encoding: settle when implementing the
  snapshot encoder (state.db round-trip requirement is already fixed by
  D37).
- Browser-side wasm lane in the web UI: deferred until a concrete need
  (M5 at the earliest, post-D50).
- Auto-fill-gaps-from-peers policy (beyond the manual fetch action):
  later, per-view opt-in, after M6 holdings channels exist (post-D50).
- peer_have bitmap representation: deferred until mirror-scale peers are
  real.

## Next sessions (pick up here)

- ~~Repo bootstrap~~ done 2026-07-03: flake (crane + rust-overlay,
  rust-flake pattern) + host workspace (6 crates) + transforms workspace
  (wit draft + xf-reference) + checks (build/clippy/fmt/nextest × 2
  workspaces + wasm lane). 8 unit tests green.
- ~~WIT world sketch~~ drafted at transforms/wit/transform.wit — marked
  DRAFT; frozen by M1 prototype 3 (determinism PoC).
- ~~CLI surface draft~~ docs/85-cli.md.
- **M1 prototype 1** (NFS store benchmark): DEFERRED — current dev
  machine isn't the NFS-bearing one. Shard fanout stays provisional
  (2×256); run the benchmark (spec in 90-roadmap.md) before declaring the
  on-disk format stable.
- **M1 prototype 2** (in progress): recipe canonical-CBOR codec +
  assemble executor + multi-hash ingest throughput.
- API shape for M5 (axum routes ↔ Svelte, codegen via datboi-api crate) —
  can wait until M4 wraps (post-D50 numbering).
- **transform@2 streaming world** (ratified for M2 by D46; M2 is now exactly this platform, D50): streams as
  resources in our own `types` interface, empty-linker property
  preserved, determinism gate extended to @2 — plus the D49
  seek-equivalence property test (random ranges == slices of full
  materialization, boundaries ±1) for declared-seekable components.

## Resolved

See [decisions.md](decisions.md) (D1–D48).
