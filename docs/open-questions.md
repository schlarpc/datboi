# Open questions & active research

Design passes R1–R8 complete; core design ratified through D39. Docs
00–90 are the record.

## Flagged for ruling (raised 2026-07-07, M2/M3 build session)

- ~~`.obao` sidecar format~~ ratified 2026-07-07 as **D52**
  (headerless pre-order obao4, iroh-compatible).
- **Fast cache rebuild / fast recovery** (work item, promoted
  2026-07-07 from the D43 deferral by the cache-migration ruling):
  `recover` currently re-hashes every byte in data/ — days over NFS at
  10M-blob scale, and it is also the fallback path for cache schema
  recreates. Replace with a metadata-only rebuild: parallel READDIR
  walk (hash from the file name, size from stat), full meta/ parse,
  alias + analysis rows from snapshot batches, deterministic dat
  re-import; byte-reading demoted to background scrub that refreshes
  `verified_at`. Tune the walk parallelism with the M1 NFS bench
  numbers when the bench machine exists.
- **Quarantine attribution refinement** (work item, accepted
  2026-07-07): `serve_range` quarantines a component on ANY
  window-verify failure through its seek path, including failures
  actually caused by corrupt *inputs*. Safe but defamatory. Refinement:
  on mismatch, verify the route's inputs (input-side bao / re-hash)
  before writing the quarantine row; only an inputs-clean mismatch
  indicts the component. Slot: `Executor::serve_range`'s error arm.
- **Analyzer identity for native analyzers**: implemented as
  `blake3("datboi-analyzer:<name>/<version>")` tags with parameters
  baked into the name (e.g. `fastcdc-v2020-nc2-64k-256k-1m/1`). Wasm
  analyzers will use their component hash. Convention, not yet a
  ruling.
- **Chunking threshold + eligibility policy**: ChunkAnalyzer currently
  chunks every data blob ≥ 4 MiB. Molten with the D45 config
  vocabulary; also interacts with "don't chunk blobs that already have
  cheaper routes."
- **Deflate rebuild: preflate, not parameter discovery** (researched
  2026-07-07; RECOMMENDATION, not yet ruled). Prior art review found
  the compressor-matching frame is obsolete: **preflate**
  (deus-libri/preflate; Rust port `preflate-rs` 0.7.x, Microsoft,
  Apache-2.0, `forbid(unsafe)`, built for exact-binary cloud storage)
  reconstructs ANY valid deflate stream bit-exactly from plaintext + a
  small corrections blob — no compressor identification needed.
  Measured corrections overhead vs uncompressed: zlib 0.01–0.08%
  (i.e. TorrentZip too), zlib-ng ≤1.07%, libdeflate ≤1.51%,
  miniz ≤2.7%. Related: precomp (zlib-param brute force only),
  reflate, grittibanzli — all subsumed. This would make effectively
  EVERY wild zip rebuildable (kills most of the D24 stays-literal tax):
  recipe = xf-preflate(corrections blob {role: skeleton}, member
  plaintext) → container bytes; corrections are ordinary CAS inputs, so
  determinism needs only the pinned component (preflate version churn
  can't break old recipes — D5 holds by construction). The zlib-exact
  compressor path is DEAD: zlib-rs has had output-determinism bugs and
  zlib-ng guarantees reproducibility only within one identical build.
  Work owed before ruling: verify preflate-rs compiles for
  wasm32-unknown-unknown (deps: cabac, bitcode) under the empty-linker
  rule; measure corrections size on real TorrentZip corpora; decide
  @1 vs @2 world for xf-preflate (members are big → @2); replace the
  deflate-trial analyzer's match-hunting with preflate splitting.
- **Sequential assemble over opaque children spills today**: the
  executor opens assemble children random-access, so a sequential read
  of concat-of-derived (e.g. concat over decompressed members) spills
  each derived child even though pure sequential streaming would do.
  Chunk recipes are unaffected (children are literals). Optimization
  noted, not designed.

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

## Open (design work, ratify before M4 views)

- **Reified views: shares as projections, images as recipes.** Insight
  (2026-07-06): every serving surface is a projection of a view
  snapshot — *live* (NFS/SMB/WebDAV/TNFS dirents walk the manifest;
  reads are verified blob range reads) or *reified* (the whole tree
  encoded as one blob: FAT32/exFAT/ISO/PS2-HDL image). A reified image
  is a plain `assemble@1` recipe — skeleton blobs (boot sector, FAT
  tables, dir clusters) + windowed segments over content blobs + fill
  for slack — with the filesystem-layout math running at view-eval
  time in the policy tier (D23: policies emit recipes; policy code
  needs no determinism because the emitted recipe self-verifies). No
  format code in the read path: nbd serving, Etcher-burnable export,
  and live share are one object at different residency; images are
  recipe-covered by construction so they cost nothing to keep.
  Design work owed: skeleton-generator tier, image params (fixed
  timestamps etc. pin identity), and **writable overlays** — devices
  write saves into shares and even into flashed images; the
  datboi-shaped answer is "writes are ingests" (per-device overlay,
  save history for free), but overlay semantics for live shares and
  dirty-image diff-back are real unbuilt design.
- **Curation distribution without byte distribution** ("moxfield for
  roms"). Because a curated view is a snapshot hash + manifest +
  recipes, sharing it shares *curation, not content*: a curator
  publishes a list; subscribers synthesize the view from bytes they
  already hold and gap-fill from their own swarm (D34 curated
  channels + peer-availability). Design work owed when curated
  channels land (M6): manifest-only subscription UX, gap-fill
  economics, and how curator updates flow (D34's
  no-auto-promotion caveat applies).
- **Pended D49 amendment — carve-out for locally-minted affine
  routes.** Tension: D49 mandates output-bao verification on
  recipe-served ranges, but the outboard requires one full
  materialization — and giant synthesized images (nbd-served OPL
  disks) are designed to never fully materialize. Candidate ruling
  (user leans this way, NOT yet ruled): carve out locally-minted,
  pure-builtin, affine-only routes over verified inputs
  (input-side bao + trusted executor arithmetic suffices — D49's
  target was seekable *transform code*, not `assemble` math), plus an
  optional background "blessing" pass (materialize-to-null, tee,
  cache the outboard) to promote them to full D49 status. M2's
  verify-path implementation should keep this pluggable; rule no
  later than M4.

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
- ~~transform@2 streaming world~~ **FROZEN 2026-07-07** (D51 status):
  executor landed (datboi-exec), full-size exit test green, D49
  quarantine machinery integration-tested against a planted seek bug.
- **M3 next chunks**: xf-deflate wasm component (unblocks wild-zip
  rebuild recipes from the trial analyzer's positives), ECM analyzer,
  7z/rar ingest, aggregation (NFS-bench-gated). Eviction + FastCDC
  chunking + trial-recompression discovery shipped 2026-07-07.

## Resolved

See [decisions.md](decisions.md) (D1–D50).
