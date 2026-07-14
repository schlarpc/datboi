# Transform runtime (wasm)

*From research pass R4. Decisions D5–D7.*

## Model

A transform is a content-addressed wasm **component** (component-model
binary format, stable since WASI 0.2). A recipe addresses it as
`(component_blake3, export_name, canonical_params)`. Content-address the
wasm binary only — never wasmtime-compiled cwasm (machine/version-specific
cache material).

## Determinism (D5)

Storage recipes must replay bit-exact forever, across versions and
architectures — the residency planner (drop literal bytes, keep recipe) and
p2p claims depend on it. Requirements:

- wasmtime `cranelift_nan_canonicalization` on; threads off; relaxed-simd
  deterministic mode.
- Pure-transform world imports **no** clock/random/fs — just
  streams + params. Determinism by construction, not convention.
- Recipe pins the exact component hash; tool upgrades mint new components;
  old components run forever (they're in CAS).
- Fuel metering is deterministic (same input → same fuel) — usable as
  reproducible cost metadata if ever needed.

## Sandboxing untrusted (peer) transforms

- Epoch interruption for wall-clock kill (near-zero overhead) +
  `StoreLimits` memory caps; pooling allocator for concurrency.
- Peer transforms can't corrupt data (CAS verifies output hashes) — the
  threat is resource abuse only. Native fast-paths never run peer code.

## Native vs wasm boundary (D6)

Native: blake3/alias hashing, baseline zstd, bao. Wasm: everything
semantic (format-aware transforms, codecs for containers, peer-supplied
code). Wasm runs ~1.5–2.5× native — fine for the long tail, wasteful on
the hot path. Instantiation is negligible (~120µs AOT).

## ABI (D7)

*SUPERSEDED by D89 ([worlds.md](worlds.md)) — the numbered-profile
scheme below describes the pre-break ABI and stands only until the
epoch break lands; this section then retires to a pointer.*

Own WIT world, `datboi:transform@1.x` (WIT packages support semver),
frozen at M1 as the *whole-buffer* profile (D41): inputs/outputs are
complete by-value blobs, and the world imports nothing but its own types
interface — the empty import surface is the sandbox. Streaming (chunked
pull/push resources, or WASI 0.3 `stream<u8>` once its rustc target
matures) is the future `datboi:transform@2` world, a sibling rather than
a revision — the world is ours, so adding it is not a format break.
"Which world does this component target" is recipe metadata. Old worlds
stay executable forever. Components compile to wasm32-unknown-unknown
core modules and are componentized with `wasm-tools component new`
(D42: wasip2's std would drag WASI imports into every component).
