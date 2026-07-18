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
  flushes through the (optional) filter stage to a member splitter that
  routes byte ranges to slot-indexed host sinks. Memory is
  dictionary-bounded, not folder-bounded.
- `src/lib.rs` is the thin Rust guest (same shape as ex-unrar):
  world bindings, the `datboi_*` hooks, ix↔file-index mapping.

## Coder coverage: full 7zDec parity (policy, not ABI)

Every folder shape upstream `7zDec` accepts, this component accepts —
streaming: single main coder **Copy / LZMA / LZMA2 / PPMd7**; main +
one filter (**Delta**, **x86 BCJ** via the resumable-state converter,
**ARM64/ARM/ARMT/PPC/SPARC/IA64/RISCV** via boundary-carried chunking);
and the **BCJ2** four-coder tree (main stream dictionary-streamed, the
small call/jump/rc streams buffered whole — a bomb-sized side stream
hits the allocator and refuses under the memory cap). Folder graphs
even 7zDec refuses (arbitrary coder chains, the raw-stream BCJ2-only
layout), encryption, and multi-volume sets refuse cleanly — the host
treats any error as whole-archive refusal and the container stays an
opaque literal (D24). There is no second decoder behind this one:
sevenz-rust2's read path left the tree with this component (D110).

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
