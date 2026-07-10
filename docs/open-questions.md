# Open questions & active research

Design passes R1–R8 complete; decisions ratified through D52. Docs
00–90 are the record.

## Flagged for ruling (raised 2026-07-07, M2/M3 build session)

- ~~`.obao` sidecar format~~ ratified 2026-07-07 as **D52**
  (headerless pre-order obao4, iroh-compatible).
- ~~Fast cache rebuild / fast recovery~~ shipped 2026-07-07: when a
  snapshot authenticates, `recover` does a parallel metadata-only walk
  (hash from filename, size from stat), restores aliases + analysis
  from snapshot batches, and demotes byte verification to `scrub`
  (which now back-fills aliases + `verified_at` in its read). Full
  re-hash remains the no-snapshot fallback. Walk parallelism (8) still
  wants the M1 NFS bench numbers.
- ~~Quarantine attribution refinement~~ shipped 2026-07-07:
  `serve_range` mismatches re-hash the implicated route's literal
  leaves first; only an inputs-clean mismatch quarantines the
  component, corrupt inputs get named in the error instead.
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

## Flagged for ruling (raised 2026-07-09, M4 serving session)

Implementation policies shipped this session with defaults chosen by
the builder — each is reversible and flagged here for ratification or
reversal (details in commit messages + code comments):

- **1G1R held-first scoring**: a held-and-verified clone outranks the
  preferred-but-absent region; re-eval upgrades the pick after ingest.
  Alternative (retool semantics): pure dat-level selection independent
  of holdings. Held-first ships as the default because the serving NAS
  is the consumer; flip or make per-view if curation-exactness should
  win.
- **Opaque long streams materialize on demand**: the daemon and
  `view sync`, when streaming a large opaque-routed non-resident blob,
  run one verified replay into the store (evictable again later)
  instead of re-spilling per window. Storage-for-latency trade chosen
  silently; the residency planner's materialize-at-snapshot-activation
  (80-views.md) is the eventual systematic answer.
- **Daemon bind policy**: 127.0.0.1:2352 default, any other bind is an
  explicit flag with a loud no-auth warning (auth is M5).
- **DAV read chunk**: 1 MiB serve_range calls (route re-planned per
  read). Fine for consoles; revisit if per-read planning shows up in
  profiles.

## Next sessions (pick up here)

**Position as of 2026-07-09**: **M4 serving core SHIPPED** — the
daemon exists (axum + tokio): HTTP Range serving of view snapshots
(`/view/<name>/` per-request tag resolution, `/snap/<hash>/` immutable,
strong content-hash ETags, single-range RFC 9110, D49-verified 8 MiB
windows, EIO-style mid-stream abort on verify failure), WebDAV at
`/dav` (dav-server 0.11, WEBDAV_RO + write-ops-Forbidden, same verified
read path), 1G1R selection (crate::selection; families from cloneof or
base-name inference, held-first scoring — see ruling flag above),
constraint profiles (fat32/everdrive/mister: FAT charset scrub, length
caps with suffix reserve, oversize rows skipped + counted, overfull
dirs reported), and SD sync (`view sync`: incremental, --verify,
--delete, temp+fsync+rename). ViewDef grew additive CBOR keys 4–7;
the viewsnap format is untouched. LATER THE SAME SESSION: in-process
NFSv3 shipped (nfsserve 0.11, opt-in `--nfs-listen`; view-dir fileids
stable across flips, everything beneath keyed (snapshot, path) so held
ids keep serving the old tree — the D33 promise under a stateless
protocol); adversarial hardening shipped (zip-bomb member inflation
bounded at declared+1, overlapping-member archives refused whole,
raw-socket traversal probes + u64-boundary Range tests; CBOR audited,
already guarded); and the tag/config recovery gap CLOSED (statesnap
payload keys 8/9, additive — golden hash unchanged; the bare-NAS
drill now proves a view survives the nuke). REMAINING M4: FAT32 image
synthesis (NEEDS the two rulings: reified views + D49 affine
carve-out — user leaned toward the carve-out but has NOT ruled), MAME
merge-mode rendering (D31 deferred set), retool clonelist consumption.

**Previous position (2026-07-07 late session)**: **M1/M2/M3 COMPLETE**
(bench-gated items indefinitely deferred by ruling). **M4 started** —
shipped: `datboi/viewsnap/1` (canonical manifest object, golden-pinned),
view definitions (state.db config KV) + evaluation (relink → rollups →
have(verified) claims → layout template with deterministic collision
suffixes → snapshot mint), the D33 flip as a `view/<name>` tag move
(doubles as the D27 GC root), CLI `view define/eval/list/manifest` with
an e2e test. NEXT: HTTP Range serving of snapshots (axum in
datboi-server: resolve tag → manifest → executor serve_range; then
WebDAV via dav-server), 1G1R selection + profiles, and the two rulings
owed before image synthesis (reified views + D49 affine carve-out).
Note: view defs/tags don't yet ride the statesnap payload — recovery
loses them (additive payload key later; same class as the existing
tag/config gap).

Priority order:

1. **M4 remainder**: FAT32 image synthesis (rule reified views + the
   D49 carve-out first — the LAST unruled gate), MAME merge-mode
   rendering (D31 deferred set), retool clonelists. Then the user's
   stated post-M4 directions: M6 iroh, M7 formats/xf-s, ingest
   policies/background curing, deeper adversarial testing.
   Carried caveat from M3/ECM: validate EDC/ECC against a real disc
   sector when the NAS corpus is reachable.
2. **7z rebuild via pinned-encoder parameter discovery — DEFERRED to
   the M7 rebuild long tail** (ruled 2026-07-07). Research concluded:
   no preflate-analog exists for LZMA anywhere; corrections can't
   transfer (adaptive range coder makes divergence global — predicting
   the optimal parse exactly IS the encoder); but param discovery is
   viable in a way it never was for zlib — LZMA encoding is
   deterministic per encoder-version+params and byte-stable across
   multi-year version families (SDK 9.04–17.01 identical; 18.06–21.x
   identical; encode.su thread 4187). Candidate design (recorded for
   M7): header blob literal; re-encode plaintext against a small
   pinned matrix (2–3 vendored encoder families × {fast,normal} × fb ∈
   {32,64,273} × LZMA2 chunk layout) with incremental-compare early
   abort; hit → recipe pins (encoder-id, params); miss → literal; no
   diff-patch middle path. PPMd/bzip2-in-7z near-free bonuses. Needs a
   C-to-wasm lane (7-Zip SDK compiled to wasm32-unknown-unknown) — the
   same infrastructure M7's CHD/RVZ/NSZ work wants, which is why it
   slots there. Deferral is structurally free: the fixpoint re-covers
   today's corpus whenever the analyzer lands. Interim hedges: the
   `status` literal-only counter (shipped) sizes the tax; an opt-in
   drop-containers-without-routes policy is a future discussion (byte-
   destroying, so never a default).
3. **RAR rebuild: confirmed infeasible, permanently literal.** No
   recompressor exists for v3/v5; the encoder is closed and the unrar
   license forbids using its source to recreate compression. The
   extraction-based ingest is the final answer for rar.
4. **Component attribution stamping** (decision owed, evidence in
   hand): `wasm-tools metadata add` embeds name/description/authors/
   license/source/revision as execution-inert custom sections — but
   they change the component hash, so the stamping convention should
   be ruled BEFORE any real corpus pins recipes. Candidate: stamp in
   the flake install phase from crate metadata + git rev.
5. **Recipe rehabilitation** (work item): `Failed` is terminal, but
   this session produced a wrongly-poisoned recipe via the (now fixed)
   pipe race — there is no operator path to un-poison a recipe whose
   poisoning was a host bug. Candidate: `scrub --rehabilitate` that
   re-replays Failed recipes and clears state on success.
6. **M1 NFS store benchmark + aggregation (D36) — INDEFINITELY
   DEFERRED** (ruled 2026-07-07). A local-SSD run cannot answer what
   the bench gates (NFS metadata round-trips are the whole case for
   aggregation and the fanout freeze), so no bench until the NFS
   machine exists. Accepted consequence: the 2-level×256 fanout is
   frozen-by-default at first real corpus; aggregation stays available
   later as an additive layer; recovery walk stays at 8 workers. A
   local scale-smoke (50k blobs, MAME-ish histogram) DID run to catch
   algorithmic pathologies in our own code: ingest is linear
   (~890 files/s, fsync-per-blob dominated, as designed); recovery was
   SQLite-autocommit-bound, fixed by batching the rebuild passes in
   transactions — fast recover 13.7s → 2.5s per 50k (~20k blobs/s ⇒
   DB side of a 10M-blob recovery ≈ 8 min; the NFS walk then dominates,
   which is the part the deferred bench would tune).
7. Rule the pended D49 affine carve-out no later than M4 views work.
8. M4 views (80-views.md): shares-as-projections, images-as-recipes.

## Resolved

See [decisions.md](decisions.md) (D1–D53).
