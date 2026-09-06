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
| XDVDFS decomposition (Xbox/360 redump + XISO) | in | yes | yes | no | native parser, all assemble@1 (D111): files/dir tables as pieces, video partition as pieces; XGD1 seed-era filler = zero-input `xf-xgd1-prng fill` stream, security ranges + pads = fills; rc4-era filler stays literal gap pieces |
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

## Rebuild long-tail verdicts (research pass, 2026-07-07)

Recorded here from the research that ruled the deferrals; the fixpoint
re-covers today's corpus whenever an analyzer lands, so deferral is
structurally free.

- **7z / LZMA — param discovery, M7.** No preflate-analog exists for
  LZMA anywhere and corrections cannot transfer: the adaptive range
  coder makes divergence global — predicting the optimal parse exactly
  IS the encoder. But parameter discovery is viable in a way it never
  was for zlib: LZMA encoding is deterministic per
  encoder-version+params and byte-stable across multi-year version
  families (SDK 9.04–17.01 identical; 18.06–21.x identical; encode.su
  thread 4187). Candidate design, recorded for M7: header blob stays
  literal; re-encode plaintext against a small pinned matrix (2–3
  vendored encoder families × {fast, normal} × fb ∈ {32, 64, 273} ×
  LZMA2 chunk layout) with incremental-compare early abort; hit → the
  recipe pins (encoder-id, params); miss → stays literal; no
  diff-patch middle path. PPMd/bzip2-in-7z fall out near-free. Needs
  the C-to-wasm lane (7-Zip SDK to wasm32-unknown-unknown) — the same
  infrastructure M7's CHD/RVZ/NSZ work wants, which is why it slots
  there. Interim hedges: the `status` literal-only counter sizes the
  tax; an opt-in drop-containers-without-routes policy is a future
  discussion (byte-destroying, so never a default).
- **RAR — confirmed infeasible, permanently literal.** No recompressor
  exists for v3/v5; the encoder is closed and the unrar license
  forbids using its source to recreate compression. The
  extraction-based ingest (D9/D58: members carry derive recipes, the
  container stays a literal) is the final answer.
- **Ogg Vorbis / Opus — balrogg, feasible, deferred (research pass,
  2026-09-06).** [balrogg](https://github.com/iczelia/balrogg) (C99,
  GPL-3.0, v1.1, format unstable until 2.0) re-entropy-codes the
  Vorbis/Opus packet syntax with an all-integer context-mixing model —
  a bitstream transform, not a codec; README claims 8–12% on Vorbis,
  3–8% on Opus. Proven in-session: the tree compiles to wasm32-wasi
  with one flag (`-std=gnu99`, fseeko) and the scalar mixer kernel;
  wasm and native builds emit BYTE-IDENTICAL archives; wasm runs ~2×
  native (1.1 MB Vorbis decodes in 1.2 s); peak memory <10 MiB native.
  Level `-4` is the right pin: `-4`..`-9` decode identically and `-9`
  is a 13-trial tune search costing 13× encode time for ~0.05%.
  Candidate design: `datboi-xf-balrogg` on the transform lane with
  two ops in ONE component (`pack` at analysis time, `unpack` as the
  derive recipe's op, so the recipe pins exactly the encoder that
  wrote its archive), opaque seek class, ex-7z build pattern
  (hash-pinned tarball → `DATBOI_BALROGG_SRC`, ~35 TUs, no libc++);
  a Structural `balrogg` analyzer gated by balrogg's own sniff (OggS +
  `\x01vorbis`/`OpusHead`) running `pack` through the stream host.
  Engineering notes: the encoder's FATAL is noreturn+exit — map it to
  a trap (the unrar-shim pattern) and let the analyzer read a trap on
  `pack` as Negative (upstream's longjmp bail mode needs wasm
  exception handling, which the runtime does not enable); output must
  be guest-buffered (the tune search seeks/truncates its output);
  the flake's wasi-toolchain gate keys on the `datboi-ex-` prefix and
  would become a per-crate list; decode fuel looked like ~1–2k/byte
  against the 4096/byte budget — measure in the gate. Format churn is
  harmless under D64 (component hash pinned, old components replay
  forever), vendored-snapshot posture as unrar/dust. Two rulings owed
  before code: a GPL-3.0 component embedded via `include_bytes!` in
  the MIT server binary (stronger coupling than D84's dust web asset
  or unrar's redistribution-friendly terms; the D89 publishing path
  could vend it instead), and the deferral itself. WHY deferred:
  corpus relevance is low — console formats don't carry Ogg (SDAT,
  ADPCM, ATRAC; Nintendo's Opus is a non-Ogg wrapper balrogg
  refuses), the real reservoir is PC game data inside Redump PC discs
  and no ISO9660/UDF decomposition exists, so Ogg blobs only arrive
  as loose drops or archive members; ~10% on rare files vs ECM's 12%
  on every CD image. Build triggers: a corpus census finds Ogg
  members, or M7 LZMA param discovery starts — that work needs
  exactly this shape (a C encoder cross-compiled to wasm, run FORWARD
  from an analyzer) and balrogg is a two-session pathfinder for it.
- **JPEG — Lepton via the Rust port, feasible, deferred (research
  pass, 2026-09-06).** Dropbox's Lepton (C++, archived 2023-02-14)
  lives on as Microsoft's
  [lepton_jpeg](https://github.com/microsoft/lepton_jpeg_rust) (pure
  Rust, Apache-2.0, 0.5.8 of 2026-06, actively maintained; Dropbox's
  own deprecation notice points at it). Split: headers zlib'd, scan
  data Huffman-decoded then re-coded with a VP8 CABAC over an integer
  predictor — ~22% on real photos; baseline AND progressive; refuses
  arithmetic-coded, 12-bit, lossless (errors, not panics — the
  transform world's polite refusal path, unlike balrogg's exit()).
  Proven in-session on synthetic 0.1–3.8 MB baseline/progressive/
  4:2:0/grayscale samples: 19–39% smaller, native encode 6–11 MB/s and
  decode 5–12 MB/s single-threaded, roundtrip exact, output bytes
  IDENTICAL across repeat runs, across thread counts (partitioning is
  a format parameter, `max_partitions`, not a thread-count artifact),
  and across native vs wasm builds (hash-compared). It is the
  preflate/ecm shape EXACTLY, not balrogg's: the analyzer calls the
  crate natively for the split (threads welcome — bytes are
  thread-independent) and the recipe pins `unpack` in a
  `datboi-xf-lepton` transform component, exact-pinned dep like
  xf-cso's miniz_oxide, opaque seek class (CABAC; the up-to-8
  horizontal partitions are a someday coarse-range hook). No
  C-to-wasm lane, no forward wasm op. The one wrinkle: the crate calls
  `std::time::Instant` for its metrics on non-Windows targets, which
  traps on wasm32-unknown-unknown (`Instant::now` is unimplemented
  there). A ~12-line cfg patch (metrics.rs `CpuTimeMeasure` → unit +
  `Duration::ZERO` on wasm32; one `Instant::now()` in
  lepton_file_writer.rs routed through it) yields a ZERO-import
  component; upstream PR is the right home, `[patch.crates-io]` on a
  fork rev the interim (D66 fetch+patch posture, Rust flavour; nix
  needs the git dep's outputHash in the crane args). wasm roundtrip
  (encode+decode) of the 3.8 MB sample: 1.4 s including JIT, ~2×
  native; peak ~100 MiB. Memory is 2 B/coefficient (pixels × components
  × 2), so the crate's default 16386-px dimension cap can exceed the
  1 GiB linear-memory ceiling on 4:4:4 — a trap there is a Negative,
  fine, but the analyzer should pre-gate on dimensions. Pin the
  `EnabledFeatures` inside the op (the compat knobs for C++-lepton
  interop change bytes; we never interoperate). Skip
  `encode_lepton_verify` (1.9× encode): D4/D25 replay is the
  verification. WHY deferred: JPEG is commoner than Ogg near a ROM
  corpus but still not IN console dumps. Candidate homes were
  checked, not assumed (2026-09-06): **PSP EBOOT.PBP** carries
  ICON0/PIC0/PIC1 as PNG and ICON1 as PMF (pspsdk `pack-pbp`) — no
  JPEG; the PSP does have a Media-Engine JPEG path (`sceJpeg`,
  HLE'd in PPSSPP) but its use is per-game (Gods Eater Burst save
  portraits, Yu-Gi-Oh! 5D's Tag Force 6 card art). **PS3** content
  files ICON0/PIC0/PIC1 are PNG by requirement (24/32-bit,
  non-interlaced; ICON1 is PAM video); JPEG only where a game calls
  the SDK's `cellJpgDec` (RPCS3 HLEs it) — per-game, no census.
  **Wii opening.bnr** is U8 archives of TPL textures (wiibrew); Wii
  game data is TPL-dominant with no system JPEG decoder. **MAME snap
  packs** (progettoSNAPS snaps, titles, cabinets) are PNG; manuals
  are PDF; flyers are offline and unverified. **Switch** control-NCA
  icons (`icon_<Language>.dat`) ARE 256×256 JPEG — verified, but
  ~100 KB apiece. So the console-adjacent JPEG mass is per-game
  interior data reachable only through disc/package decomposition,
  and the PNG cases above point at a different lane entirely
  (preflate over the concatenated IDAT zlib stream). PC discs need
  ISO9660 first, and the real reservoir (box art / screenshot /
  manual-scan media libraries) is a scope datboi has not ruled on.
  Triggers: an ISO9660 analyzer, a corpus census finding JPEG
  members, or an artwork/media-library scope ruling. Build cost when
  it fires: under a session — it is xf-cso with a different crate.
