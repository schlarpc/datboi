# ex-7z — the 7z extractor component (D110)

The second `datboi:extractor@1` component (after ex-unrar, D58/D89): 7z
extraction moves off the pure-Rust sevenz-rust2 decoder (measured ~5x
slower than native on real Redump archives — D110) and onto 7-Zip's own
public-domain ANSI-C decoder, inside the wasm sandbox. A seekable
archive stream goes in; member metadata and member byte streams come
out, all through host-implemented WIT resources, with ZERO wasm imports
(D5/D46 empty-linker determinism contract).

## What is vendored, what is datboi

- `nix/lzma-sdk.nix` pins the 7-Zip source tarball by content hash and
  exposes its `C/` tree (LZMA SDK lineage — **public domain**, per the
  files' own headers; no unrar-style license restriction applies).
  Only decode-side TUs compile: `7zArcIn`, `7zBuf`, `7zCrc`,
  `7zStream`, `CpuArch`, `Delta`, `LzmaDec`, `Lzma2Dec`.
- `csrc/glue.c` is datboi-written: container open/enumerate over the
  SDK's `7zArcIn`, plus a **streaming folder decode** replacing the
  SDK's `7zDec` (which materializes a whole solid folder in memory —
  a big-ISO folder would blow the 1 GiB guest cap). LzmaDec/Lzma2Dec
  stream through their circular dictionary window; every produced span
  flushes to a member splitter that routes byte ranges to slot-indexed
  host sinks. Memory is dictionary-bounded, not folder-bounded.
- `src/lib.rs` is the thin Rust guest (same shape as ex-unrar):
  world bindings, the `datboi_*` hooks, ix↔file-index mapping.

## v1 coder scope (policy, not ABI)

Supported folder shapes: single coder **Copy / LZMA / LZMA2**, or a
main coder + **Delta** filter. Branch filters (BCJ/BCJ2/ARM…), PPMd,
encryption, and multi-volume sets refuse cleanly — the host treats any
error as whole-archive refusal and ingest falls back to the extraction
lane it had before ex-7z (sevenz-rust2), so coverage never regresses
while the coder set grows.

Verification: running CRC32 per requested member (checked against the
header's per-file CRC when defined) + folder CRC when defined; the
host additionally hash-tees every sink (D4) and verifies sizes, so a
decoder bug wastes CPU but cannot corrupt the store.

## Batch semantics (D89)

`extract` takes a request batch; each solid folder decodes ONCE for
however many of its members are requested (the splitter discards
unrequested ranges). Member bytes are a pure function of
`(containers, ix)` regardless of the request set — the gate tests
assert the subset property.
