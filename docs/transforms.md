# Transform catalog & dedupe strategy

*From research pass R3 (oxyromon, RomVault, igir, retool, clrmamepro, romm +
format deep-dives). Full sourcing in research notes; this is the distilled
design input.*

## Gap datboi fills (nothing does these today)

Cross-dat/cross-system dedupe at the storage layer; content-addressed
derived artifacts; reproducible transforms (existing tools shell out to
version-drifting external binaries — oxyromon shells to ~10); p2p;
verify-without-materializing.

## Lessons adopted from prior art

- **oxyromon**: `convert` vs `export` distinction (derived copies for
  external consumers vs in-place representation changes) maps to our
  residency planner vs output transforms. Closest prior art; study its
  sqlite schema.
- **RomVault**: incremental-rescan cache (rescans must be O(changed), keyed
  better than relative paths); TorrentZip/RVZSTD canonical archives prove
  the community accepts new deterministic formats.
- **igir**: cheap-first hash escalation (CRC32+size from zip central
  directory without decompressing, escalate to stronger hashes only when
  the dat demands); parent/clone inference for dats that lack it (Redump).
- **retool**: the 1G1R filter model (ordered region priority, ordered
  language priority, category exclusions, regex include/exclude,
  out-of-band clonelists) is the expressiveness floor for our selection
  criteria. We consume community clonelists directly; we filter at
  query/output time and never mutate source dats.
- **clrmamepro**: header skipper XMLs are a community-maintained declarative
  spec for header detect/strip — implement an interpreter for them as an
  input transform (D9).

## Transform catalog (abridged; Det = deterministic, LL = lossless)

| Transform | Dir | Det | LL | Secrets | Notes |
|---|---|---|---|---|---|
| zip/7z unpack | in | – | yes | no | rar: ingest-only (license, D9) |
| TorrentZip/RVZSTD write | both | yes (by spec) | yes | no | pin zlib behavior exactly; vendor as wasm |
| Header strip/add (iNES, FDS, Lynx, A7800, SMC…) | both | yes | yes | no | fixed-offset concat recipe; skipper XMLs |
| N64 byte-order (z64/v64/n64) | both | yes | yes | no | trivial |
| GBA trim/re-pad | both | yes | yes | no | pad byte + length in recipe |
| NDS NitroFS decomposition | in | yes | yes | no | pure concat → all assemble@1, native parser, no wasm (D83); rebuild/derive/trim all affine |
| NDS trim | out | yes | see notes | no | header-derived prefix slice; DSi at 210h, NTR at 80h + 88h "ac" RSA block (Download Play); tail-must-be-pad gate (D83). Trimmed-*in* can't recover the full dump if the sig was already stripped |
| NDS secure-area KEY1 normalize | in | yes | yes | **BIOS key table** | collapses encrypted/decrypted dumps onto one ARM9 blob; future wasm (D83) |
| DSi modcrypt strip | both | yes | yes | **console keys** | ARM9i/ARM7i AES-CTR; joins the NSZ/3DS key-policy question; future wasm (D83) |
| NDS interior decompress (LZ overlays, NARC/SDAT) | in | yes | yes | no | preflate-shaped corrections lane; future wasm (D83); overlay +1Ch flag bits are tool lore, verify before building |
| SRAM/save-type patch (GBA) | out | yes | one-way | no | original stays in CAS |
| IPS/BPS/xdelta apply | out | yes | patch separate | no | |
| **ECM strip (CD EDC/ECC)** | in | yes | yes (recomputable) | no | ~12% of raw sectors; **ideal first wasm transform** |
| bin/cue split, 2352↔2048 | both | yes | yes | no | |
| CHD | both | **not across chdman versions** | yes | no | pin one impl as versioned wasm module; MAME dats hash the *container*, so byte-repro matters. chd-rs = pure-Rust read |
| CSO/ZSO/DAX | out | per settings | yes | no | |
| RVZ (GC/Wii) | both | per version | yes incl. junk regen | no | `nod` crate (pure Rust r+w) near-free; read NKit, never write it |
| XISO ↔ Redump Xbox | both | yes | via recipe sidecars | no | video partition shared across catalog → big dedupe |
| NSZ / 3DS / WiiU / PS3 decrypt | both | yes/mostly | yes | **console keys** | biggest storage win; key policy = open question |
| Generic zstd/lz4/xz | both | yes (pin lib) | yes | no | the baseline recipe |
| FAT32/exFAT & console FS views | out | yes (fixed ts) | n/a | no | the "whole filesystem view" feature; `fatfs` crate |

## Dedupe wins, ranked

1. **Store-decrypted, serve-encrypted** (Switch/3DS/WiiU/PS3) — encrypted
   bytes are incompressible and unique; decrypted interiors compress and
   dedupe across regions. Needs keys.
2. **ECM + cue normalization** for CD-era media — pure recomputable
   redundancy, then variants chunk-dedupe.
3. **Uncompressed-interior storage** — containers (zip/CHD/CSO/RVZ) become
   recipes; interiors dedupe, compressed containers never do.
4. **Disc decomposition** — shared partitions + per-file granularity
   (Xbox video partition, NDS NitroFS files across regional variants).
5. **Junk/padding elimination** (Wii junk regen, GBA/NDS pad, Xbox gaps) —
   bytes that are functions of position need zero storage.
6. **Header strip** — small bytes, big *identity* win (collapses
   headered/headerless dat worlds onto one object).
7. **CDC chunking** of what remains — 2–4× across language/revision
   variants once containers/encryption are out of the way.
