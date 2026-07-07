# Open questions & active research

Design passes R1–R8 complete; core design ratified through D39. Docs
00–90 are the record.

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
- **transform@2 streaming world** (ratified for M2 by D46; M2 is now exactly this platform, D50): streams as
  resources in our own `types` interface, empty-linker property
  preserved, determinism gate extended to @2 — plus the D49
  seek-equivalence property test (random ranges == slices of full
  materialization, boundaries ±1) for declared-seekable components.

## Resolved

See [decisions.md](decisions.md) (D1–D50).
