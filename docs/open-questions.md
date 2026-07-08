# Open questions & active research

Design passes R1–R8 complete; decisions ratified through D52. Docs
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
- ~~Deflate rebuild: preflate, not parameter discovery~~ ruled
  2026-07-07 as **D53** after the spike (wasm build verified, zero
  imports; TorrentZip corpus bit-exact at ≈0.002% corrections):
  xf-preflate targets the @2 streaming world; estimator failures fall
  back to stays-literal.
- **preflate coverage gap on unmodeled compressors** (open issue,
  accepted with D53 as an optimization): preflate-rs 0.7.6 cleanly
  errors on deflate streams whose match-finder fits none of its
  modeled compressors — 7-Zip's deflate encoder reproducibly fails at
  every level, and some real-world zips fail per-member. Those
  containers keep paying the D24 stays-literal tax. Paths if it ever
  matters: upstream issue / patch the fixed 4096-chain ceiling in
  complevel_estimator, or a fallback corrections codec. Revisit when
  wild-corpus hit rates are measurable (M3 sweep telemetry).
- **xf- policy is creeping into core crates** (watch item, raised
  2026-07-07 during the xf-preflate build-out): the component boundary
  isolates *replay* (framing format owned by the component hash; the
  executor knows nothing about preflate), but discovery is native and
  coupled — datboi-ingest links preflate-rs directly and runs it
  UNSANDBOXED over wild bytes (the D5 sandbox protects replay only),
  and the datboi-runtime gate — nominally about the @2 world — now
  pins a specific component fixture. Consistent with D23 (analyzers
  are the policy tier) but worth watching: if a third native analyzer
  dependency lands, consider (a) analyzers-as-wasm-components (the
  identity convention already anticipates component hashes), (b) a
  separate conformance test crate for shipped components, (c) at
  minimum a hardened/fuzzed parsing path for wild containers.
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

**Position as of 2026-07-07**: M1 complete (additive ingest+audit,
recovery drill in CI). **M2 complete** — transform@2 FROZEN, streaming
executor (datboi-exec: replay licensing, spill rule, serve_range with
D49 output-bao verify + seek quarantine), obao machinery (D52),
fixpoint skeleton (provenance incl. negatives, snapshot batches,
recovery drill green), full-size exit test passed (3.9 GiB member,
bounded memory). **M3 partial** — shipped: eviction + residency
planner (evict_covered), FastCDC ChunkAnalyzer (dedup→evict→serve
proven e2e), deflate-trial discovery analyzer (provenance only), cache
migration ladder. CLI: ingest/dat/audit/export/recover/snapshot/
scrub/status/sweep/evict/materialize.

Priority order:

1. **xf-preflate build-out** (spike done, ruled as D53): mint the @2
   component, replace the deflate-trial analyzer's match-hunting with
   preflate splitting + recipe minting. This is the wild-zip shrink
   unlock.
2. **Quarantine attribution refinement** (work item above; small).
3. **Fast recovery / metadata-only rebuild** (work item above;
   parallelism tuning wants the M1 NFS bench, but the structure can
   land first).
4. Remaining M3 analyzers: ECM, 7z/rar ingest.
5. **M1 NFS store benchmark** — still DEFERRED (dev machine isn't the
   NFS-bearing one); gates aggregation (D36), freezes shard fanout,
   tunes the recovery walk.
6. Rule the pended D49 affine carve-out no later than M4 views work.

## Resolved

See [decisions.md](decisions.md) (D1–D52).
