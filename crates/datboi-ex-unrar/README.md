# ex-unrar — the RAR extractor component (D58)

The first **extractor** component (`ex-` prefix = the
`datboi:extractor@1` world; `xf-` = transform). It moves the one
memory-unsafe wild-byte parser in the datboi tree — unrar's vendored C++
— INSIDE the wasm sandbox (D58 census, 2026-07-10). A seekable archive
stream goes in; member metadata and member byte streams come out, all
through host-implemented WIT resources, with ZERO wasm imports (D5/D46
empty-linker determinism contract).

## Licensing (READ BEFORE TOUCHING `vendor/`)

`vendor/unrar/` is the official UnRAR source (RARVER 7.1.0, 2024-05-12),
by Alexander Roshal. Its license (`vendor/unrar/license.txt`) permits
using the source in any software to **decompress** RAR archives free of
charge, but **forbids** using it to develop a RAR-compatible archiver or
to re-create the RAR compression algorithm, which is proprietary. This
crate only decompresses. Per the license, the full text of its clause 2
is reproduced in `vendor/unrar/license.txt` and this notice, and the
vendored code carries it in `license.txt` comments; keep that file
intact when redistributing.

This matches the ingest direction exactly: rar is extraction-only, and
the "RAR rebuild is permanently infeasible" ruling stands (no
recompressor exists; the encoder is closed). Extraction being
deterministic-by-construction under wasmtime is what lets rar members
carry DERIVE RECIPES (container→member) and become evictable — the
recipe rebuilds a member's bytes by re-running THIS component, never by
recompressing.

## Guest shape (D58 spike outcome: Rust-over-staticlib)

The ruled-preferred shape won the spike: a thin Rust guest crate
(`src/lib.rs`, wit-bindgen rust for the `datboi:extractor@1` world) over
a C++ staticlib built by `build.rs` via wasi-sdk. The C++ side is two
small glue files plus the vendored unrar:

- `csrc/glue.cpp` — an `extern "C"` veneer over unrar's own dll API
  (`vendor/unrar/dll.hpp`, already `extern "C"`): open → (read-header →
  process)* → close, member bytes flowing out through the
  `UCM_PROCESSDATA` callback. Extraction runs as `RAR_TEST` (in-memory
  decode + CRC check, never a filesystem write).
- `csrc/shim.cpp` — the determinism/freestanding shim. wasi-libc's
  syscall wrappers bottom out in `wasi_snapshot_preview1` imports, so
  every libc symbol unrar can reach is DEFINED HERE first (so the
  importing wasi-libc objects never link): archive I/O reroutes onto the
  guest's stream hooks (`datboi_input_*` / `datboi_sink_write`),
  clock/env/fs queries answer with fixed deterministic values, and any
  real filesystem write path traps (`__builtin_trap` → wasm
  `unreachable`). The component imports nothing; the gate enforces it.
- `csrc/compat/` — force-included stubs for the handful of POSIX headers
  and symbols wasi-libc lacks (`pwd.h`, `grp.h`, `sys/file.h`,
  `dup`/`umask`/`lchown`), all on owner-restore / stdout-dup paths that
  test-mode extraction never runs.

## Build knobs (ruled, D58)

- `RAR_SMP` OFF (no threads — matches the deterministic engine).
- `ErrHandler` → trap: the vendored source is patched (see
  `vendor/unrar_sys-patches.txt` provenance and the `DATBOI(D58)`
  comments) so `throw` becomes a trap and the dll try/catch scaffolding
  is removed — an archive fails whole, matching the
  refuse-suspicious-archives posture.
- v1 scope cuts: NO encrypted archives, NO multi-volume (the volume /
  password callbacks refuse), links / NTFS streams ignored.
- The wasmtime memory cap turns RAR5 big-dictionary bombs into clean
  refusals (`UCM_LARGEDICT` also refuses).

## Vendored source provenance

`vendor/unrar/` is the unrarsrc tree as redistributed by the `unrar_sys`
crate (v0.5.8, unrar 7.1.0), the native lane this component replaces.
`vendor/unrar_sys-patches.txt` records that crate's upstream patch
hashes. The only datboi modifications are the `DATBOI(D58)`-tagged
exception-removal edits; everything else is byte-for-byte upstream.
