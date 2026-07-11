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
- ~~Analyzer identity / coverage semantics~~ ruled 2026-07-10 as
  **D55**: exact-hash identity, lineage declared at registration,
  grandfathered coverage, migration explicit; native analyzers keep
  self-declared tags until they become components. Amended 2026-07-10
  as **D65**: lineage + grandfathering dropped (never implemented);
  the deploy is the policy (shipped components, explicit directives
  beyond that) and disagreement between rows is surfaced, per the
  forward-compat principle ruled as **D64**.
- ~~Chunking threshold + eligibility policy~~ ruled 2026-07-10 as
  **D59**: route-less literals ≥ 4 MiB only (threshold unchanged);
  work item to narrow the shipped analyzer.
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
- **xf- policy creep / unsandboxed discovery** (watch item 2026-07-07;
  censused + largely resolved 2026-07-10 as **D58**): the census found
  unrar_sys (vendored C++) was the only memory-unsafe wild-byte
  parser — D58 moves it inside the sandbox as the first extractor
  component and pulls the C-to-wasm lane forward from M7. Native
  *Rust* analyzers are acceptable permanently (ruled: the "moderately
  safe" bar). Remaining hygiene, still open: fuzz targets for the
  in-house wild-byte parsers (zip walker, CHD header, cue, ECM
  splitter) in CI; a conformance test crate for shipped components
  stays a someday.
- **Sequential assemble over opaque children spills today**: the
  executor opens assemble children random-access, so a sequential read
  of concat-of-derived (e.g. concat over decompressed members) spills
  each derived child even though pure sequential streaming would do.
  Chunk recipes are unaffected (children are literals). Optimization
  noted, not designed.

## Open (minor / deferred to build-time)

- ~~Ingest-policy config vocabulary~~ shape ruled 2026-07-10 as
  **D60** (per-analyzer enable + opaque params in the config KV,
  lineage at registration — the lineage clause since dropped by
  **D65**, global dat-aware ordering). Detector
  registry ordering + canonical-orientation preference remain
  deliberately undesigned within D60 until a consumer exists.

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

## Open (design work)

- ~~Reified views: shares as projections, images as recipes~~
  **ratified 2026-07-10 as D62** (M4 scope: read-only FAT32
  synthesis; pinned image params; fsck-in-CI mandatory); **shipped
  the same day** (see position note).
- **Writable overlays + dirty-image diff-back** (split out of the
  D62 ratification, still real unbuilt design): devices write saves
  into shares and into flashed images; the datboi-shaped answer is
  "writes are ingests" (per-device overlay, save history for free),
  but overlay semantics for live shares and diff-back for dirty
  images must be designed before nbd/live-write serving. Until then
  image-mode sync warns that reflashing clobbers on-device saves.
- **Curation distribution without byte distribution** ("moxfield for
  roms"). Because a curated view is a snapshot hash + manifest +
  recipes, sharing it shares *curation, not content*: a curator
  publishes a list; subscribers synthesize the view from bytes they
  already hold and gap-fill from their own swarm (D34 curated
  channels + peer-availability). Design work owed when curated
  channels land (M6): manifest-only subscription UX, gap-fill
  economics, and how curator updates flow (D34's
  no-auto-promotion caveat applies).
- ~~Pended D49 amendment — affine carve-out~~ **ruled 2026-07-10 as
  D63** (in-code predicate: locally-minted + pure-builtin +
  affine-only + verified inputs; wasm never qualifies;
  seek-equivalence gate extends to synthesized recipes; optional
  blessing pass promotes to full D49).

## Flagged for ruling (raised 2026-07-09, M4 serving session)

All four builder defaults ratified 2026-07-10: materialize-on-demand,
bind policy, and DAV read chunk as **D56** (rider: disk-headroom
guard before materializing is an owed work item); 1G1R as **D57**
(now a per-view mode {held-first, strict}, default held-first —
strict mode + retool clonelist consumption are M4 work items).

## Flagged for ruling (raised 2026-07-11, M5 web session)

- **wuchale is pre-1.0** (D67 accepted this eyes-open). Catalogs are
  standard gettext PO, so the worst case is swapping the compiler,
  not the translations. Revisit when it hits 1.0 or stalls.
- **Jobs tray backend**: the daemon has no persistent job registry —
  analyzer sweeps, ingest, scrub run as CLI-process work today. M5
  ships a minimal in-daemon jobs surface (`/v1/jobs`, in-memory) so
  the tray has something truthful to render; durable job reports
  (the design's "reachable from Jobs" eval/ingest reports) want a
  real job table — design item for M5 polish or M6.
- **Authenticated WebDAV** (basic auth against D68 bearer tokens)
  so friends can mount views; NFS auth is likely never (protocol);
  both stay loopback-only meanwhile.
- **Quarantine review screen** was never designed (the wireframes
  link `review →` into nothing). Storage page ships the count +
  list; the review/resolve flow needs design.
- ~~Shared API types~~ ruled 2026-07-11 (same day) as **D69**: the
  derive rule is scoped to identity bytes; a typed `datboi-api`
  crate owns every /v1 shape, emits checked-in OpenAPI behind a
  staleness gate, and the web build generates TS from it. The M5
  stopgap (hand-written TS pinned by integration tests) is dead.
  Browser hardening (CSP + Fetch-Metadata CSRF) ruled alongside as
  **D70**.
- **Scrub runs and verify methods aren't recorded**: the index keeps
  per-blob `verified_at` only — no method, no scrub-run ledger — so
  `/v1/storage` cannot report last-scrub and the entry drawer's
  verify line shows a date without a "how". A run ledger belongs to
  the same future job table as the Jobs tray backend above.
- **System ids are cache surrogates**: `/v1/systems` keys on
  `dat_source.source_id`, which `datboi recover` re-mints from
  scratch. UI deep-links survive a browsing session, not a cache
  rebuild — fine for M5; if bookmarkable system URLs ever matter,
  the durable key is the provider/system pair, not the integer.
- **View editor + eval report/diff screens deferred**: view
  definitions are CLI-authored in M5 (mutating actions stay
  CLI-only), so the editor (spec §3.4) shrinks to a read-only
  definition fold on the Views cards with redefine/eval CLI hints;
  the eval report and snapshot diff (§3.5) have no API at all — no
  eval history or per-snapshot diff is stored — and want the same
  durable job/report table as the Jobs tray backend above. The
  eviction planner (§3.7/§3.8) is deferred on the same grounds (no
  plan API; the dry-run CLI is the only entry).
- **Web rulings made during implementation** (recorded here, not
  D-numbered): nav = `Library · Views · Ingest · Storage · Admin`
  (audit is the drill-down under Library; the hi-fi "Dats" tab
  variant rejected as redundant with it); friend-facing surface
  ships in M5 (it is what invites+ACLs exist for; M6 "Friends" is
  the iroh daemon-to-daemon plane, a different thing) — shipped
  2026-07-11: shelves home, browse (flat full-path rows per the
  interactive prototype's canon; folder rows were only a wireframe
  sketch), entry panel, trust bar, SD-image modal, backed by
  `GET /v1/views/{name}/files` (paged, server-side q) and
  `GET /v1/views/{name}/image` (the minted blob through the same
  verified-range machinery as /view files — a clean reuse, so no
  CLI-hint fallback was needed; the modal's download is a plain
  anchor, so no client-side progress bar — the browser's own
  download UI is the truth); desktop-only
  layout for now (all comps are 1160px; responsive is design work);
  `▶ Play` (browser emulator cores) and box-art metadata provider
  stay explicitly-future per the comps, UI reserves their slots.

## Next sessions (pick up here)

**Position as of 2026-07-10 (third session of the day)**: **M4 IS
COMPLETE.** After the FAT32 session (below), the M4 tail shipped in
one sweep: D59 (chunking narrowed to route-less literals), D56
(disk-headroom guard in materialize, statvfs via rustix), D60
(analyzer config: family() on the trait, enable/params KV rows,
`datboi analyzer` CLI, sweep gate), D61 (verified already
implemented), name-fitting pipeline + alpha-bucketing +
ezflash-omega profile (80-views.md owed work), D57 (strict 1G1R as
selection-mode 2 + retool clonelists via `dat clonelist`,
content-addressed with a config pointer), and MAME merge-mode
rendering (catalog::mame — non-merged with transitive device_ref
closure, split, merged; ViewDef CBOR key 12; `--mame-mode`;
.chd extensions; dangling device_refs counted in EvalReport; D31's
deferred set closed, loadflag rebuild semantics stay M7). The **D58 unrar-wasm extractor lane SHIPPED AND MERGED** the same
day (background agent, 9 commits): `datboi:extractor@1` WIT world
(`ex-` prefix), vendored unrar 7.1.0 with trap-conversion edits only
(license clause honored), thin-Rust-over-C++-staticlib guest (the
ruled-preferred shape), wasi cross-toolchain lane in the flake,
ExtractorHost + conformance gate in datboi-runtime, exec
`OpImpl::Extractor`, rar ingest through the component with
container→member derive recipes (members now evictable; the test
evicts and rebuilds bit-exact). **The D46 empty-import contract
held** — zero WASI imports, no ruling owed. Notes: the stamped
component lives at `transforms/dist/ex_unrar.wasm` (rebuild + re-copy
if the crate changes); container bytes buffered in memory during
extraction (fixed later the same day: ingest now serves the container
to the component as a store-file `RangeRead` and streams each member
through a pipe straight into `put_new` — nothing whole in memory;
`pipe` + `FileRandom` moved from datboi-exec into datboi-runtime).
NEXT: M5 (axum API, invites + passwords D30, ACLs, Svelte web UI
D17 — a functional brief for the UI design pass was drafted this
session), and the carried caveat: validate ECM EDC/ECC against a
real disc sector when the NAS corpus is reachable.

**Previous position (2026-07-10, build session, after the decision
session below)**: **FAT32 IMAGE SYNTHESIS SHIPPED — D62 + D63
implemented in full.** Eight commits: (1) `fat32.rs` pure layout math
in datboi-catalog (MBR default, 32 reserved sectors, strictly
sequential chains so every file is one contiguous cluster-aligned
window, LFN + deterministic ~N tails, fixed 2000-01-01 timestamps,
serial/disk-signature from snapshot hash, golden-pinned skeletons);
(2) `image.rs` mint — one `assemble@1` recipe per image, skeleton
blobs + inline literal sectors + content windows + fill, output hash
AND obao computed in one streaming pass (blessed at mint by default,
ruled), `image/<name>` tag = D33 flip + GC root, idempotent; (3)
ViewDef image params (additive CBOR keys 8–11) + `view image` CLI
(--out exports through verified windows; always prints the
clobbers-saves warning); (4) the D63 carve-out in `serve_range` —
plan-then-sidecar, verify-when-sidecar-exists precedence, tight
in-code predicate (assemble-only, LocalIngest, Affine,
Verified/ReplayedLocal, resident store-verified leaves), leaves
served through per-read bao re-validation (`VerifiedRandom`),
`bless_output` promotion; (5) the seek-equivalence gate (boundary ±1
+ seeded random ranges vs sequential materialization) + predicate
refusal matrix (computed node / Peer source / unverified leaf /
non-resident leaf) + blessing test; (6) fsck-in-CI — dosfstools in
the nix test check with DATBOI_REQUIRE_FSCK=1 (CI can never skip),
independent `fatfs`-crate read-back tree-diff vs the manifest; (7)
the evictor pinned-root guard evict.rs promised (`image/*` inputs +
`view/*` opaque rows, `Blocked::PinnedByView`, strict on undecodable
pins); (8) docs. End-to-end CLI drive in cli.rs (define --image →
eval → image --out → fsck clean → idempotent re-mint). Session
rulings (AskUserQuestion): separate `view image` command (not inside
eval — eval stays residency-free), MBR on by default, obao stored at
mint by default (`--no-obao` opts into carve-out serving), GC guard
included now. NEXT (M4 remainder): MAME merge-mode rendering (D31
deferred set), retool clonelists + strict 1G1R (D57), ruled riders
(D56 headroom guard, D59 eligibility narrowing, D60 config shape,
D61 rehabilitate), name-fitting pipeline + dir bucketing
(80-views.md); then the D58 unrar-wasm lane.

**Previous position (2026-07-10, decision session)**: **DECISION
SESSION — every open ruling in the project resolved (D55–D63)**; no
unruled gates remain. Ruled:
D55 exact-hash identity/lineage/explicit-migration (note: component
attribution itself was ALREADY ruled as D54 on 07-07 — the
priority-list entry below it was stale; decisions.md is authoritative
over this file's flags); D56 serving defaults (+ owed disk-headroom
guard); D57 1G1R per-view {held-first, strict}; D58 unrar-to-wasm
extractor components, C-to-wasm lane pulled forward from M7 (rar
members gain derive recipes; WASI-shim fallback would amend D46 and
returns as a ruling if freestanding fails); D59 chunking narrows to
route-less literals; D60 minimal config shape; D61
`scrub --rehabilitate`; D62 reified views (M4 = read-only FAT32,
fsck-in-CI mandatory, overlays pended); D63 the D49 affine carve-out.
NEXT (all implementation, no discussion owed): FAT32 image synthesis
(D62/D63 — skeleton generator, image params, fsck-in-CI,
seek-equivalence extension); MAME merge-mode rendering (D31 deferred
set); retool clonelists + strict mode (D57); the D58 unrar-wasm lane
(~1.5–2 wk: wasi-sdk in flake, RAR_SMP off, ErrHandler→trap,
File-class reroute, extractor world, derive recipes); ruled riders —
headroom guard (D56), chunking eligibility narrowing (D59), config
shape (D60), rehabilitate (D61).

**Previous position (2026-07-09)**: **M4 serving core SHIPPED** — the
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

Priority order:

1. **M4 remainder**: ~~FAT32 image synthesis (D62/D63)~~ **shipped
   2026-07-10**; MAME merge-mode rendering (D31 deferred set),
   retool clonelists + strict 1G1R mode (D57), plus the ruled riders
   (D56 headroom guard, D59 eligibility narrowing, D60 config shape,
   D61 rehabilitate) and the profile name-fitting pipeline +
   dir-bucketing (80-views.md, recovered 2026-07-10 from the 2021
   prototype's EZ-Flash Omega mutator; adds an ezflash-omega profile:
   512 files/dir, 99-char names). Then the user's stated post-M4 directions: M6
   iroh, M7 formats/xf-s, ingest policies/background curing, deeper
   adversarial testing. Carried caveat from M3/ECM: validate EDC/ECC
   against a real disc sector when the NAS corpus is reachable.
1b. **unrar-wasm extractor lane (D58)** — own track, ~1.5–2 weeks;
   also the pathfinder for M7's C-to-wasm work (7-Zip SDK reuses the
   toolchain and the guest-glue design).
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
4. ~~Component attribution stamping~~ **was already ruled 2026-07-07
   as D54** (tree-hash revision, load refusal, one crate = one
   lockfile) — this entry was stale; the 2026-07-10 session added the
   coverage/lineage semantics as **D55**. Lesson recorded:
   decisions.md is authoritative; this file's flags can lag.
5. ~~Recipe rehabilitation~~ ruled 2026-07-10 as **D61**
   (`scrub --rehabilitate`); implementation is an M4-adjacent work
   item.
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
7. ~~D49 affine carve-out~~ ruled 2026-07-10 as **D63**.
8. ~~Reified views~~ ratified 2026-07-10 as **D62**; writable
   overlays remain the open design item (pre-nbd).

## Resolved

See [decisions.md](decisions.md) (D1–D53).
