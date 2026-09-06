# Open questions & active research

[decisions.md](decisions.md) is the authoritative record (through
D115); the subsystem docs (see [README.md](README.md)) are the design
record. This file holds only what is genuinely open: flags awaiting a
ruling, deferred design work, watch items, and the pick-up-here
position note. Condensed 2026-07-17 — resolved entries were deleted
(decisions.md owns them) and orphaned reasoning moved into the D log
(D36 amendment, D104), transforms.md (7z/RAR rebuild verdicts), and
web-ui.md (the nav ruling). History lives in git.

## Flagged for ruling

Each of these wants its D entry before (or as) the code lands.

- **Recon ACL before any discovery/advertisement tier** (D100/D102
  residual): today the recon ALPN reveals the recipe/roots inventory
  to anyone holding the unlisted EndpointId — capability-addressed
  friends plane, fine behind `--p2p` opt-in, not fine once announced.
  Also owed with the D34 channel work: how manifests ride the channel.
- **Saves + writable overlays** ([saves.md](saves.md) is the design
  pass D62 reserved; save persistence is the loudest play-surface
  gap). Eleven rulings are enumerated at the end of that doc, none
  D-numbered — the savenode shape (ruling 1) is the
  expensive-to-change one and goes first. Two findings gate
  implementation: the state ring-buffer needs the store's FIRST
  byte-destroying code path (posture-change D-entry + drill owed — v1
  stays explicit-only states so it can ship sooner), and shared media
  (memcards / Controller Pak) needs its own `(media-instance, owner)`
  timelines when a memcard console lands. The wider overlay questions
  (live-share writes, dirty-image diff-back) stay owed before
  nbd/live-write serving; until then image-mode sync warns that
  reflashing clobbers on-device saves.
- **Curation distribution without byte distribution** ("moxfield for
  roms"). A curated view is a snapshot hash + manifest + recipes, so
  sharing it shares *curation, not content*: subscribers synthesize
  the view from bytes they already hold and gap-fill from their swarm
  (D34 curated channels + peer-availability). Design owed when
  curated channels land (M6): manifest-only subscription UX, gap-fill
  economics, curator update flow (D34's no-auto-promotion caveat).
- **Dat-aware residency, the unruled half** (D47 splits claims from
  scheduling; D91 ruled which-literal-holds-the-bytes for affine
  routes). Still open: materialize view-pinned absent members whose
  containers refused a preflate split (the serving case), and any
  dat-aware preference for opaque routes (where D91's
  keep-dat-named-resident instinct survives).
- **Acquisition provenance vs the rescan cache — trigger has FIRED.**
  `source_file` does two jobs — the rescan cache
  (`lookup_unchanged_source`) and byte-provenance display (orphan
  review, blob inspector) — with different lifecycles: a cache row
  for a renamed directory is garbage; "these bytes arrived as
  roms/pack.zip" should outlive any path. The named trigger was the
  first non-filesystem byte origin, and p2p fetch landed 2026-07-17
  (D100/D101): a peer arrival has no path and cannot live in a
  path-keyed table. Owed: split typed acquisition events
  (blob, origin, detail, actor, at) out of source_file, designing the
  origin vocabulary against the ACTUAL p2p shapes (peer endpoint?
  channel? ticket?), and decide batch-vs-accept-loss for snapshot
  batching then, with evidence (losing arrival names costs a
  review-card label, not days of CPU).
- **Authenticated WebDAV** (basic auth against D68 bearer tokens) so
  friends can mount views; NFS auth is likely never (protocol); both
  stay loopback-only meanwhile.
- **OTEL metrics (cross-cutting).** The daemon logs via `tracing`
  (D81); "OTEL metrics soon" is wanted — spans ingest, sweeps,
  eviction, serving, reconciliation. Correct-by-construction rule
  meanwhile (already followed by the D101 sync savings): emit
  observability as structured, named, NUMERIC `tracing` fields, never
  interpolated strings, so a `tracing-opentelemetry` layer lifts them
  into metrics with no re-instrumentation. Wants its own D-entry when
  the exporter lands (metrics-vs-traces subset, cardinality
  discipline, opt-in/endpoint config in the D95 NixOS surface).

## Open (design work)

- **Scrub-repair posture** (deferred from D105): a rotted obao
  section in a pack is recomputable byte-identically from the member
  bytes in the same file — an in-place rewrite would restore the pack
  to matching its own filename. Restoration-to-name vs the write-once
  posture wants its own ruling before any repair verb lands; until
  then repair is "repack it" like any other rot, and scrub localizes
  (whole-file mismatch + all members verifying clean ⇒ section/footer
  rot). Related in spirit to the saves ring-buffer's byte-destroying
  drill flag.
- **Quarantine review screen** was never designed (the wireframes
  link `review →` into nothing). Storage ships the count + list; the
  review/resolve flow needs design — the D73 orphan review card is
  the pattern to reuse. A storage treemap visualization also remains
  open.
- **View editor + eval report/diff screens.** View definitions are
  read-only on the web (definition fold + CLI hints); the eval report
  and snapshot diff have no API — no eval history or per-snapshot
  diff is stored; the D74 ledger is the place for eval rows when that
  screen gets built. Of the old eviction-planner spec only a plan
  PREVIEW surface remains open (dry-run over the API; the CLI's
  --dry-run is the only entry today).
- **Screen taxonomy naming pass**: "Audit" is a CAS-author name for
  what the user experiences as *the library list*, and
  Library/Browse/Shelves/Views overlap at the component level. No
  user-visible bug; wants a naming pass when the next screen gets
  added, not before.
- **Recipe deep-link page**: recipes are content-addressed meta
  blobs, so a dedicated page is *possible* — build it only if recipes
  grow multi-level structure worth deep-linking; another CAS-debugger
  surface is the failure mode.
- **NDS wasm lanes (deferred from D83).** Three future verbs, each
  needing a wasm component and a ruling: (1) secure-area KEY1
  normalization (Blowfish keyed from a BIOS-derived table — inherits
  the console-key-material policy question shared with
  NSZ/3DS/WiiU/PS3 decrypt); (2) DSi modcrypt (AES-CTR over
  ARM9i/ARM7i, console keys again); (3) interior decompression (LZ
  overlays + SDAT interiors, preflate-shaped corrections; verify the
  overlay-table +1Ch compressed-size/flag convention against real
  ROMs first — tool lore, not GBATEK). NARC recursion itself landed
  as D94 (builtin-affine); still wasm-shaped: SDAT audio and
  LZ-compressed NARC MEMBERS (the codec, not the archive). Sequencing
  vs CDC: these eat the archive-shaped near-misses exactly; CDC (the
  D59 rank-7 amendment, landed) takes the media-stream remainder.
- **Emulation deferred items** (split out of D84; design record in
  [emulation.md](emulation.md)):
  - *Save persistence* — the loudest gap (MKDS re-asks first-run
    setup every session); the saves.md flag above is the design.
  - *Friend-facing BIOS access* — blob bytes are owner-only today, so
    friends fall back to HLE BIOS, which won't boot the same games;
    view-scoped or grant-scoped blob access is the eventual answer.
  - *Control rebinding* — out of v1 AND in tension with D78
    zero-toggles; needs a ruling arguing per-device config ≠
    preference toggle.
  - *Second core* — tetanes-core (NES, headless) is the cheap test
    that the host contract generalizes; the contract stays unfrozen
    until it passes.
  - *dust upstream watch* — bus-factor-one; plan B is melonDS via
    emscripten (FreeBIOS included) at the cost of C++ glue.
  - *dust's homebrew heuristic* — WORSE since BIOS shipped: with key
    material present dust KEY1-"decrypts" the unencrypted secure area
    of modern homebrew (ndstool places ARM9 at exactly 4000h, so
    `is_homebrew` misclassifies hbmenu/ftpd as encrypted commercial),
    corrupting real code. A small local patch to dust's detection
    (melonDS-style) is the right move and would be the
    vendored-snapshot posture's first exercise.
  - *Unexercised on real devices*: haptics on Android, landscape deck
    feel, simultaneous stylus + button multitouch in a real game.
- **Job progress residuals** (D74 ledger shipped): intra-file/
  intra-item progress — the Ingester has no callbacks (the D71 Pulse
  trait is the natural hook when the tray wants it); SSE over the
  bounded-mpsc pattern is the upgrade from polling if per-byte
  progress ever lands (D104); scrub-run and eval-report rows are
  future D74 consumers (additive kind codes), each needing wiring
  when its surface wants history.

## Open (minor / deferred)

- **Ogg Vorbis/Opus recompression (balrogg)**: feasibility proven,
  design recorded in [transforms.md](transforms.md) §rebuild long-tail
  verdicts (2026-09-06). Deferred on corpus relevance. Triggers: a
  corpus census finds Ogg members (D114's ISO9660 split now exposes
  PS2/PSP/PC disc interiors, so the census is a provenance query
  away), or M7 LZMA param discovery starts (same
  forward-encode-in-wasm shape; balrogg is the pathfinder).
  Rulings owed at build time: GPL-3.0 component embedded in the MIT
  server binary; the analysis-time forward `pack` op through the
  stream host.
- **JPEG recompression (Lepton, Rust port)**: feasibility proven,
  design recorded beside balrogg's in [transforms.md](transforms.md)
  (2026-09-06). Pure-Rust preflate/ecm shape, Apache-2.0, nothing to
  rule — one upstream cfg patch for wasm32 (`Instant::now`) to land
  or carry. Deferred on corpus relevance; the ISO9660-analyzer
  trigger fired (D114, 2026-09-06) — the remaining triggers are a
  census finding JPEG members inside split discs, or an
  artwork/media-library scope ruling.
- **GBA save-type patching (`xf-sram-patch`)**: research pass done,
  design recorded in [transforms.md](transforms.md) §save-type
  patching verdict (2026-09-06). Shape ruled: offline/analysis-time
  offsets DB (gba-patch-gen's model, MAME `gba.xml` + FlashGBX DB by
  SHA-1 first, tag scan + signatures second) applied by a dumb
  fixed-offset transform, bank-switch flavour and batteryless layer
  on the VIEW PROFILE. GBA-only; GB/GBC + WonderSwan ride IPS-from-DB;
  no other system patches. Deferred until a supercard/repro/bootleg
  profile or the saves subsystem asks for GBA media kinds.
- Shard fanout + inline-outboard threshold: frozen-by-default; the
  gating NFS benchmark is indefinitely deferred (D36 amendment).
- Detector registry ordering + canonical-orientation preference:
  deliberately undesigned within D60 until a consumer exists.
- Auto-fill-gaps-from-peers policy (beyond the manual fetch action):
  later, per-view opt-in, after M6 holdings channels exist (post-D50).
- peer_have bitmap representation: deferred until mirror-scale peers
  are real.
- API contract imprecision (D69 residual): `WhoamiResponse` and
  `ImageStatus` describe invariants as independent optional fields
  rather than oneOf discriminated unions (screens guard defensively);
  `EntryRow.wanted_hash_algo` keeps its enum in prose. utoipa
  supports oneOf; upgrade when it next itches.
- Verify METHOD per blob: `verified_at` has no "how" column — wants
  one when scrub grows methods worth distinguishing.
- System ids are cache surrogates: `/v1/systems` keys on
  `dat_source.source_id`, re-minted by `recover`; if bookmarkable
  system URLs ever matter, the durable key is the provider/system
  pair.
- p2p remainders (D97–D103 tail): hash-seq requests (offset > 0); a
  shared wasm engine per CasProvider (per-connection today);
  partial/resumed ranges over-materialize (`open_stream` from 0,
  discard forward — `serve_range` on the window is the targeted fix
  if resumption traffic warrants); D98 staging is MemStore — FsStore
  when GB-scale wholesale fetch arrives; the disaster-restore verb
  (snapshot → want-list, no design risk).
- Fuzz targets for the in-house wild-byte parsers (zip walker, CHD
  header, cue, ECM splitter) in CI; a conformance test crate for
  shipped components stays a someday (D58 hygiene tail).
- D89 tail: crates.io publication of the guest crates (wit vendoring
  at publish time designed, not built — a git dep serves until
  someone external asks); world-level extractor params forwarding
  (recipe schema grows a forwarded subset when passwords become
  real).
- Web polish deliberately left CLI-only (D96 pass): dat revision
  history + revision picker (no API), scrub sample-% and per-family
  sweep limits, analyzer opaque params (no UI need yet).
- ECM EDC/ECC: validate against a real disc sector when the NAS
  corpus is reachable (carried caveat from M3).

## Watch items

Named so they aren't rediscovered as bugs; not slated for change.

- **preflate coverage gap on unmodeled compressors** (accepted with
  D53): preflate-rs 0.7.6 cleanly errors on deflate streams whose
  match-finder fits none of its modeled compressors — 7-Zip's deflate
  encoder fails at every level; those containers pay the D24
  stays-literal tax. Paths if it ever matters: upstream-patch the
  fixed 4096-chain ceiling in complevel_estimator, or a fallback
  corrections codec. D71's ambient refinement accumulates the hit-rate
  telemetry by itself; after a real corpus soaks, it's a provenance
  query away.
- **Sequential assemble over opaque children spills**: the executor
  opens assemble children random-access, so a sequential read of
  concat-of-derived spills each derived child. D72's watermark
  eviction produces this serving shape routinely
  (assemble-over-recreate), so the first real NFS workload after
  eviction kicks in tells us whether this is promoted from noted to
  needed.
- **Pack format (D91)**: first-packer-wins couples read locality to
  acquisition order (sharers read in the first-packer's order — fine
  for a variant pair, watch a base shared by many variants); repack
  write-amplification is worst where sharing is highest (a
  waste-threshold policy is the likely tuning, undesigned); packs are
  content-NAMED but not content-CONVERGENT (membership is
  history-dependent — harmless while the invariant holds: packs are
  local cache, never a p2p unit).
- **Swap guard-hold**: the swap phase's D72 guard hold spans its
  materialization — fine at DS scale, revisit if packs grow to
  disc-image size (GUARD_TTL is 15 min).
- **Grounded-set-aware enqueue cost at corpus scale** (D92 amendment
  landed the fixpoint dedup; the absolute cost at 10M-blob scale is
  unmeasured; sharing the fixpoint with the audit rollup stays a
  someday).
- **wuchale is pre-1.0** (D67, eyes-open). Catalogs are standard
  gettext PO, so worst case is swapping the compiler. Revisit at 1.0
  or a stall.
- **wkg publish**: the first main push after D89 publishes
  `datboi:{streams,transform,extractor}@1.0.0` to
  ghcr.io/schlarpc/datboi/* — confirm the job goes green and
  `cosign verify` works as documented in the workflow comment.

## Next session (pick up here)

**Position as of 2026-07-17 (later) — D105 BUILT**: pack format v2 —
the outboard section rides the pack (member-rooted obao4 trees,
layout fully DERIVED from the v1-shaped footer rows, zero new
fields), `blake3(footer)` in the trailer closes the open-time
footer-trust gap, and `put_pack` computes each member's tree as a
byproduct of the verification it already did (bless loops deleted
from both swap phases; the chunk phase drops loose `.data` AND
`.obao4`). Ships as `datboi/pack/1` outright — no deployed packs
existed, D103-amendment doctrine. Scrub-repair of a rotted section is
the one deferred flag (Open § design work). **Pick up here**: the
swarm tiers with the recon ACL (flagged above — the D103 envelope has
a place for an auth argument if the ACL design wants one); the D34
channel design (naming layer: entry→blake3 gap-fill, curated-view
discovery, `available-from-peer(X)`); smaller remainders in Open
(minor) above.

**Position as of 2026-07-18 — D108–D110 BUILT**: analyzer classes
claim-gate the sweep order (the drone fleet raced chunk past ecm on
the first real ingest — ~520 MB of pieces the D59 gate would have
declined; the class gate closes it at the claim layer), cache v7
drops the never-wired `blob.obao` column, and ex-7z ships at FULL
7zDec folder parity (streaming, dictionary-bounded; sevenz-rust2's
reader left the tree — see the D110 amendment). Watch items: both
extractor lanes still refuse multi-volume sets and encrypted
archives (policy cuts awaiting demand, not ABI); PPMd as a BCJ2
side-stream coder refuses (absurd-but-legal shape — revisit only if
a real archive surfaces).

**Position as of 2026-09-06 — D111 BUILT**: XDVDFS decomposition +
XGD1 filler regeneration. `xdvdfs-split/1` (family `xdvdfs`,
Structural) decomposes Xbox / Xbox 360 images — redump or bare XISO —
into per-file pieces; seed-era XGD1 filler is verified sector by
sector against the recovered stream and becomes ranges of a
ZERO-INPUT `xf-xgd1-prng fill` recipe (security ranges consume the
stream, pads and slack are fills, rc4-era filler stays gap pieces);
the executor serves seekable wasm children in place; the D91 swap
counts generated inputs as free and fires on the regeneration trigger
alone. PROVEN on both real discs: Halo v1.02 (seed era, 42.2%
regenerated, full ingest→sweep→swap→evict→rebuild round trip in ~2
minutes, see the D111 amendment) and Halo v1.09 (rc4 era: 21 residue
pieces, 44% filler literal, 16 security ranges as zero runs). The
opt-in real-image gates: `DATBOI_XDVDFS_IMAGE` with
`datboi-ingest --test xdvdfs real_image_from_env` (walk only) and
`datboi-exec --test xdvdfs_shrink real_image_swaps_and_serves` (the
whole pipeline; set `DATBOI_XDVDFS_WORKDIR` to a big disk). Watch
items: (1) only Halo has been through it — a disc with thousands of
files, and an XGD2/XGD3 image, are the next shapes worth a walk (the
piece cap and the 360 partition bases are exercised only by the
fixture); (2) the
security-range rule is "exactly 4096 zero sectors AND the next sector
predicts from the jumped position" — a disc whose last security range
is followed only by pad leaves those 4096 stream sectors unconsumed,
which changes nothing byte-wise (F is just shorter); (3) the video
partition classifies as residue pieces (dedupes across a wave by
identity) — a UDF-aware split is a someday; (4) `xdvdfs:max-pieces`
(4096) coalesces to extents past the cap — a 10k-file disc's per-file
dedupe is deferred to that knob.

**Position as of 2026-09-06 (later) — D112, D113 BUILT**: the swap
fires on reclaimed bytes (`swap:reclaim-min-bytes`, 4 MiB; both ratio
knobs gone), and the video partition walks as ISO9660 with the XISO
and video volume claimed as alias-bearing views. Measured on the Halo
pair before D112: 73.9% of raw resident (v1.09 refused by the 50%
sharing ratio); D112 lets it swap for 51.9%. Watch items: (1) the
XISO view length rides XboxKit's per-XGD table — the one piece of
redump geometry the image cannot tell us; a disc whose partition does
not fit the table claims no XISO view and says so in the verdict;
(2) the video volume view is the PVD-declared volume, not XboxKit's
layer-padded `.video.iso` — if a community dat ever names the padded
form, a second view is one table entry away; (3) DONE
2026-09-06 (later): `swap_candidates` excludes view routes (an input
at least as large as the output) per ROUTE — the 1,220 slice rows are
gone, and a bare XISO ingested after its redump image is a candidate
by its own decomposition, not the image's older view claim; (4) DONE
2026-09-06 (later): XGD2 and XGD3 redump images through the whole
pipeline — see the D111 amendment.

**Position as of 2026-09-06 (overnight) — D114 BUILT, corpus soak
DONE**: `iso9660-split/1` (family `iso9660`, Structural) decomposes
cooked disc images by their primary tree — the D113 walker one level
up — with uniform pad files as fills, multi-extent files one piece
per extent, and the residue gate (a quarter of the image outside the
tree, above a 4 MiB floor) declining redump Xbox images so xdvdfs
owns them in either sweep order. Proven on a PSP image and a 1.8 GB
Xbox 360 press-kit DVD-ROM (swap, bit-exact rebuild, verified
ranges; the press kit reclaimed 245 MB from duplicate members alone)
and Negative on three redump Xbox images. Gates:
`DATBOI_ISO9660_IMAGE` with `--test iso9660 real_image_from_env`
(walk) and `--test iso9660_shrink real_image_swaps_and_serves`
(pipeline, `DATBOI_ISO9660_WORKDIR`). The corpus soak walked XGD2,
XGD3 and seven bare XISOs (D111 amendment) and measured the video
partition across an XGD2 wave (D113 amendment). Watch items: (1)
**the residue gate's cost** — a Negative on a 7.8 GB Xbox image
costs 2–14 s of gap classification before the gate fires; a cheaper
pre-gate on declared coverage would need to know fills without
reading them, so it stays as is until the sweep queue shows it;
(2) **the XGD3 video view** is unclaimed (declared volume spans
both layers — see the D111 amendment); (3) **PS2 is unmeasured** —
every PS2 redump item on archive.org is access-locked; the PSP disc
and the DVD-ROM stand in, and a PS2 regional pair (the sharing case
the family was built for) is the first thing to run when one is
reachable; (4) **XGD3 fires the swap for 26 MB** — the D112 floor
as ruled, but 8.7 GB written once to reclaim 0.3% is the number to
look at if a cost term is ever reconsidered; (5) raw 2352 images
(bin/cue) are a later layering under ECM; (6) the system-area
classifier splits only a TRAILING uniform run, so a PSP disc's
non-zero system area becomes a 28 KB residue piece with its zero
tail attached — cosmetic. **Pick up here**: the D34 channel design
and the recon ACL remain the next architectural work; corpus-wise,
a PS2 pair when reachable, and a first look at what the split discs
expose (a JPEG/Ogg census inside PSP/PS2 interiors decides the
deferred media lanes).

**Position as of 2026-09-06 (overnight, later) — D115 BUILT**:
`gcm-split/1` (family `gcm`, Structural) decomposes GameCube images
into system + FST-file pieces with the mastering junk regenerated by
the zero-input `xf-gc-junk fill {id, disc, len}` stream over the
disc's address space (positional lagged-Fibonacci, reseeded per
32 KiB), every byte verified by equality; the swap's view filter now
lets generated inputs through (D112 tail amendment). Proven on four
real redump discs (two revisions of Doubutsu no Mori +, a USA/Europe
Rally Championship pair): 62–97% regenerated, 30–339 BYTES of residue
per disc — see the D115 entry for the pipeline numbers. Gates:
`DATBOI_GCM_IMAGE` with `--test gcm real_image_from_env` (walk),
`--test gcm_shrink real_image_swaps_and_serves` (pipeline) and
`real_pair_dedupe` (`DATBOI_GCM_IMAGE_B`), `DATBOI_GCM_WORKDIR`.
Corpus: `~/corpus/gc` (RVZ decoded offline with nodtool; the raw
redump GC set on archive.org ships RVZ inside zips). Watch items:
(1) **RVZ/NKit as ingest forms** — the corpus's dominant shape;
decoding is deterministic, a byte-exact RVZ rebuild needs a pinned
compressor (a component-frozen zstd, the TorrentZip row's shape) —
its own ruling; (2) **Wii** (`wii-split/1`) needs the three rulings
from the proposal: key discovery by known hash under D12 with a
missing key as a settled-Negative-that-re-enqueues, the swap packing
grounding LEAVES rather than direct inputs (so a decrypted partition
intermediate is never packed), and the re-encrypt component's seek
class at 2 MiB hash-group granularity; the junk generator inside a
partition is this crate verbatim; (3) walk cost is dominated by
generating and hashing J over 1.46 GB (~3 s) — a per-title cache of
J's hash keyed by (id, disc, len) would make second discs of a title
instant, deferred until a sweep queue shows it; (4) multi-disc titles
(disc 1/2) share nothing by junk (different disc number) but share
files by identity, unmeasured; (5) the piece cap is unexercised by
a real disc (the largest here has 228 files).

## Resolved

See [decisions.md](decisions.md) (D1–D115).
