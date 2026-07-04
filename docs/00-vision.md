# datboi — vision

*Status: draft, actively being designed. See [open-questions.md](open-questions.md) and [decisions.md](decisions.md).*

## What it is

A dat/rom management system built on three pillars:

1. **Content-addressed storage (CAS)** — every object (roms, dats, transform
   scripts, wasm modules) lives in a pluggable content-addressed store.
   Storage backends are interchangeable: local/NFS filesystem first, with
   HTTP, S3, and iroh (p2p) as feasible targets.
2. **Recipes / "CAS scripts"** — alongside literal storage `H(x) → x`, the
   system stores *claims* of the form `H(f(x)) = y`: a recorded sequence of
   operations that reproduces an object from other stored objects. This
   enables better-than-naive storage (decrypt-before-store, decompose ISOs,
   rolling-hash dedupe of language variants, zstd, ECM, …) while still
   satisfying dat-specified hashes.
3. **Transformations as content-addressed code** — input transforms (unpack
   archives, strip/add headers, recurse into weird formats) and output
   transforms (recompression, 1G1R selection, trimming, patching, whole
   scripted filesystem views) are implemented either in a small sequencing
   language we control or as wasm modules with a known ABI. The transform
   code itself is content-addressed, so old versions of the software can
   verify/reproduce outputs produced by newer versions — critical for p2p
   sharing.

## Non-goals / anti-goals

- Not a RetroArch: avoid an explosion of config screens. Opinionated defaults,
  scriptability for power users.
- Not a downloader/piracy tool: it manages and verifies what you have.

## Deployment model

- Rust daemon (12-factor, env-configured), started via CLI; CLI also provides
  client subcommands. TypeScript web UI on top. Shared-friends tenancy
  ("plex server" model) with access control. Future: in-browser emulator
  cores for direct play; p2p library sharing over iroh or similar.

## Implementation commitments (from initial brief)

- Rust, leaning heavily on correct-by-construction type system design.
- Wasm transforms written in Rust, executed in wasmtime.
- Everything built with Nix; monorepo (structure TBD — see open questions).
- Web UI in TypeScript.
