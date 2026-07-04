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

Own WIT world, `datboi:transform@1.x` (WIT packages support semver).
Implemented today on wasip2-style chunked pull/push streaming (nothing may
buffer whole blobs); WASI 0.3 `stream<u8>` (ratified, landing in wasmtime
now, rustc target tier 3) is adopted later as an *internal* migration —
the world is ours, so p2→p3 is not a format break. "Which world does this
component target" is recipe metadata. Old worlds stay executable forever.
