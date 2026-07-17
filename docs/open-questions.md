# Open questions & active research

Design passes R1–R8 complete; decisions ratified through D73. The
subsystem docs (see [README.md](README.md) for the reading order) are
the record.

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
  wild-corpus hit rates are measurable — and note that D71's ambient
  refinement now accumulates exactly that telemetry by itself (D48
  negatives with per-member failure details, no manual sweeps
  needed): after the first real corpus soaks, the hit rate is a
  provenance query away.
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
  noted, not designed. Update 2026-07-11: D72's automatic watermark
  eviction now produces exactly this serving shape ROUTINELY (evicted
  containers serve through assemble-over-recreate), so the first real
  NFS/serving workload after eviction kicks in will tell us whether
  this gets promoted from noted to needed.

## Open (minor / deferred to build-time)

- ~~Ingest-policy config vocabulary~~ shape ruled 2026-07-10 as
  **D60** (per-analyzer enable + opaque params in the config KV,
  lineage at registration — the lineage clause since dropped by
  **D65**, global dat-aware ordering). Detector
  registry ordering + canonical-orientation preference remain
  deliberately undesigned within D60 until a consumer exists.

- Shard fanout + inline-outboard threshold: frozen by the M1 NFS
  benchmark (spec in roadmap.md), not by discussion.
- ~~State snapshot cadence~~ ruled and shipped 2026-07-11 as
  **D75**: the maintenance cycle's ambient tick auto-mints when the
  authoritative triple (sources, tags, config) moved —
  content-derived dirtiness, no flags; mint extracted to
  datboi-catalog::statesnap, `datboi snapshot` stays as the manual
  trigger.
- ~~Browser-side wasm lane in the web UI~~ the concrete need arrived
  and was **ruled 2026-07-12 as D84**: emulator cores are the third
  wasm lane (web-bundle assets, not CAS components); design record
  in emulation.md.
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
- **NDS wasm lanes (deferred from D83)**: v1 decomposes .nds with
  builtins only; three future verbs each need a wasm component and
  a ruling. (1) Secure-area KEY1 normalization — the first 800h
  bytes of ARM9 differ between encrypted-cart and decrypted-scene
  dumps of the same game; Blowfish keyed from a BIOS-derived table,
  so it inherits the console-key-material policy question already
  open for the NSZ/3DS/WiiU/PS3 decrypt row. (2) DSi modcrypt —
  AES-CTR over ARM9i/ARM7i; rank-1 store-decrypted win, console
  keys again. (3) Interior decompression — LZ overlays and
  SDAT interiors, preflate-shaped (plaintext + corrections
  blob); before building, verify the overlay-table +1Ch
  compressed-size/flag convention against real ROMs (tool
  convention, not documented in GBATEK). **NARC recursion LANDED
  2026-07-16 as D94** (`narc-split/1`): builtin-affine, no wasm (same
  FNT/FAT format), recipe-volume gated (`narc:max-members`, default
  4096) — it eats the archive-shaped near-misses before CDC. Still
  wasm-shaped and open: SDAT audio + LZ-compressed NARC MEMBERS (the
  codec, not the archive) — a max FAT is 61440 files and NARCs
  multiply that.
- **Emulation deferred items (split out of the D84 ratification,
  see emulation.md)**: each is real design work owed after the
  spike, none gates it. (1) ~~BIOS slots from CAS~~ SHIPPED same
  session (see emulation.md §ROM and BIOS i/o): the HLE-BIOS wall
  below made it the unblock, and it cost one endpoint
  (`GET /v1/blobs/{hash}/bytes`, owner-only) — MKDS boots to its
  menus with real BIOS from the store. Still open inside it:
  friend-facing BIOS access (owner-only today; friends fall back to
  HLE, which won't boot the same games — view-scoped or
  grant-scoped blob access is the eventual answer). Original design
  note: core descriptors
  carry named slots with hard-coded accepted content hashes; at
  launch the host asks which exist in CAS and fetches; BIOS dumps
  stay ordinary ingested blobs, the hash list IS the verification.
  UPGRADED from nicety to unblock (observed post-M3): dust's HLE
  BIOS does not carry Mario Kart DS through boot — stuck pre-display
  in OUR harness and in dust's own web frontend alike, save chip
  correctly wired — so real BIOS bytes are what commercial coverage
  actually needs. Note /snap/{hash} already serves any blob to an
  owner session: the fetch half may need zero new API.
  (2) Control rebinding — out of v1 AND in tension with D78
  zero-toggles; when it arrives it needs a ruling arguing per-device
  config ≠ preference toggle. (3) ~~Friend-facing play ACL~~
  resolved for v1 by the D84 amendment: play rights ARE download
  rights (the ▶ sits beside the download anchor and fetches the same
  granted /view bytes — no new surface); reopens only if play ever
  grants more than bytes (server-side saves, netplay).
  (4) Save persistence — NOW THE LOUDEST GAP (post-spike: games
  have their save chips, so MKDS re-asks first-run setup every
  session as the in-memory save evaporates). dust already exposes
  export_save/load_save and the worker protocol can carry the bytes;
  the design owed is storage-side — saves as ordinary ingested blobs
  keyed by (game, user), the "writes are ingests" overlay design
  above, history for free. D-entry before code: ownership
  (per-user?), round-trip timing (periodic? on dispose?), and how a
  save finds its game again.
  (5) ~~Touch button overlay~~ SHIPPED as the touch deck (D86 —
  deliberately not an overlay): CSS-drawn clusters that never cover
  the pointer screen, owning the letterbox space instead, feeding
  the same absolute-input bitmask; per-pointer role latch, 8-way
  d-pad sectors, slide-to-roll buttons, `(pointer: coarse)`
  capability gate. Pure math in lib/emu/touch.ts, unit-tested.
  Fullscreen landed with it (D87: CSS takeover + native API where
  present). Original note: a phone can tap MKDS menus but cannot
  press A to drive. (Gamepad shipped with M3.) (6) Second core —
  tetanes-core (NES, MIT/Apache, headless) is the cheap test that
  the host contract generalizes; the contract stays unfrozen until
  it passes. (7) dust upstream watch — bus-factor-one; if it stalls
  hard, plan B is wrapping melonDS via emscripten (FreeBIOS
  included) at the cost of a C++ glue layer. (8) dust's homebrew
  heuristic — WORSE since BIOS shipped: with key material present,
  dust now KEY1-"decrypts" the UNENCRYPTED secure area of modern
  homebrew (corrupting real code — a crash or wrong behavior, where
  it used to be a clean error). A small local patch to dust's
  detection (melonDS-style) is now the right move, and would be the
  vendored-snapshot posture's first exercise. Original finding
  (milestone 1): `is_homebrew` = arm9 ROM offset outside
  [4000h, 8000h), but modern ndstool places homebrew ARM9 at exactly
  4000h (hbmenu's BOOT.NDS, ftpd), so dust misclassifies those as
  encrypted commercial carts. Commercial decrypted dumps carry the
  E7FFDEFFh dumper marker and boot fine (argvTest-era homebrew at
  200h too).
- **Rank-7 CDC over decomposed pieces (observed 2026-07-12, D83
  session)**: D59 gates chunking to route-less literals, so pieces
  minted by decomposition are never CDC'd — correct for
  evictability, but it leaves near-miss cross-variant dedupe on the
  table (MKDS USA↔EUR: 8 of 564 pieces differ, ~1.3 MiB — the
  localized archives CDC exists for). Small today (most differing
  pieces sit under the 4 MiB chunk threshold); if big near-miss
  pieces show up (region-variant movies, large localized archives),
  amend D59 to admit resident pieces whose only route derives from
  an evicted container, rather than building anything new. Update
  2026-07-15: **D91 creates exactly this population** (resident
  grounding-leaf pieces — routed on paper, route-less to the D21
  fixpoint, so the has-any-route gate mispredicts them). **LANDED
  2026-07-16** as the D59 rank-7 amendment: the gate is now
  `is_covered_by_others` (grounded without the blob's own literal) +
  a resident-only guard, so grounding-leaf pieces get chunked. Sequencing note:
  NARC/SDAT interior decomposition (wasm-lanes item above) should
  eat the archive-shaped near-misses exactly, before CDC takes the
  media-stream remainder (localized movies/audio — the pieces no
  format analyzer will ever help with).

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
- **Jobs tray backend**: the in-daemon jobs surface shipped 2026-07-11
  with web ingest; refine + gc maintenance joined the tray the same
  day (D71–D73). The durable half RULED AND SHIPPED same day as
  **D74**: state.db `job` ledger (session precedent —
  snapshot-excluded), terminal-snapshot persistence, db-assigned ids
  stable across restarts, interrupted-on-restart crash evidence.
  Still open, smaller now: intra-file/intra-item progress — the
  Ingester has no callbacks (the D71 Pulse trait carries bytes for
  analyzers and is the natural hook when the tray wants it), and SSE
  over the bounded-mpsc pattern remains the upgrade from the 2 s poll
  if per-byte progress ever lands; scrub-run and eval-report rows are
  future consumers of the D74 table (additive kind codes), each
  needing its own wiring when its surface wants history.
- **Dat-aware residency policy** (raised 2026-07-11): D47 splits
  claims (dat-blind, hard) from scheduling (dat-aware, allowed); D71
  exercises the scheduling half. The third knob — WHICH literal holds
  the bytes — is local policy and may also be dat-aware without
  fraying convergence: e.g. "keep dat-named blobs resident",
  "materialize members of view-pinned sets whose containers refused a
  preflate split" (the one case where container-literal carving
  leaves a dat-matched member absent + opaque-routed). Update
  2026-07-11: the planner DID grow its first preference — D72 orders
  candidates seek-class-first (still dat-blind; it fixed the
  mutually-inverse-pair stranding the e2e caught). The dat-AWARE half
  (keep dat-named blobs resident, materialize view-pinned absent
  members) remains open and still wants its ruling. Update
  2026-07-15: the which-literal-holds-the-bytes half RULED as
  **D91** for affine routes (pieces over container, sharing-gated,
  one sealed pack per decomposition). Keep-dat-named-resident was
  rejected there as a GENERAL rule (it would block the swap
  everywhere it pays) — the instinct survives only for opaque
  routes, which D91 never touches. Still open here: materialize
  view-pinned absent members whose containers refused a preflate
  split (the serving case), and any dat-aware preference for opaque
  routes.
- **GC-family concurrency preconditions** (raised 2026-07-11, D71
  session; RULED same day — D72 takes the singleton guard, D73 takes
  the grace-window/mark-clearing shape; kept for the reasoning): two
  degenerate cases that MUST hold in those implementations.
  1. *Orphan GC vs. in-flight analysis.* Today's evict cannot touch
     analysis intermediates (plaintext/corrections/skeleton enter the
     recipe graph as INPUTS; candidacy requires being a
     replay-licensed OUTPUT — the sound-by-construction claim in
     D71). But a sweep-unreferenced-blobs GC sees them as textbook
     garbage: the preflate analyzer stores every member's split
     products FIRST and mints recipes AFTER, so they sit
     reference-less for the whole multi-member split (minutes on a
     big container), plus an instant of bytes-in-CAS-with-no-index-row
     inside every `put` → `upsert_blob` pair. Killing them there
     leaves blob rows saying Resident for vanished bytes —
     index/store divergence, worse than re-paying the analysis. And
     reference counts CANNOT distinguish these from real garbage: a
     crashed split leaves identical orphans that at-least-once
     re-attaches on the next sweep — they are pending, not abandoned.
     Required mitigation: a creation-time grace window ("never sweep
     anything younger than N" — the cleanup_temp precedent), NOT
     GC-reads-leases (couples the storage plane to the scheduler
     plane; D71 deliberately kept leases dedup-only). Optional
     window-shrinker: mint each member's recreate recipe right after
     its split instead of batching, though the container assemble
     inherently waits for all members, so grace is the actual fix.
  2. *Evict racing evict.* Two eviction runs (CLI + CLI today;
     CLI + daemon tomorrow) can each compute the D21 grounding
     fixpoint, each individually approve dropping one half of a
     mutually-inverse recipe pair, and both commit — jointly
     circular, both literals gone, exactly what D21 forbids. The
     grounding check and the drop are not one cross-process atomic
     unit, and SQLite serializes statements, not reasoning. Eviction
     therefore needs a SINGLETON guard (one coarse lease or an
     exclusive-writer rule) — and unlike the sweep leases, this one
     IS a correctness gate, which is why it must not be conflated
     with them.
- **Acquisition provenance vs the rescan cache** (raised 2026-07-11,
  orphan-review session): `source_file` is doing two jobs — the
  rescan cache (`lookup_unchanged_source`, its real job) and the
  byte-provenance display (orphan review, blob inspector) — and they
  want different lifecycles: a cache row for a renamed directory is
  garbage; "these bytes arrived as roms/pack.zip" should outlive any
  path. Today the tension is harmless (both origins are
  filesystem-shaped; web rows carry the client name since the D73
  session). It becomes REAL the day a non-filesystem byte origin
  ships — p2p fetch is the obvious one; a peer arrival has no path
  and cannot live in a path-keyed table at all. Trigger condition:
  when that origin lands, split typed acquisition events
  (blob, origin, detail, actor, at) out of source_file and design the
  origin vocabulary against the ACTUAL p2p shapes (channel? ticket?
  policy?), not a speculated enum. Explicitly deferred WITH it:
  snapshot-batching acquisition history (D22/D48 pattern) — by the
  measured-need standard that machinery wants a real cost-of-loss,
  and losing arrival names costs a review-card label, not the days of
  CPU that justified batching analysis provenance. Decide
  batch-vs-accept-loss then, with evidence.
- **Authenticated WebDAV** (basic auth against D68 bearer tokens)
  so friends can mount views; NFS auth is likely never (protocol);
  both stay loopback-only meanwhile.
- **Quarantine review screen** was never designed (the wireframes
  link `review →` into nothing). Storage page ships the count +
  list; the review/resolve flow needs design. The storage breakdown
  + blob inspector shipped 2026-07-11 (`/v1/storage/breakdown`,
  `/v1/blobs`, `/v1/blobs/{hash}` → `/storage/blob/{hash}`):
  aggregates by class/source, one-hop recipe-DAG navigation,
  claims/pins provenance. A treemap visualization and the
  quarantine review itself remain open design work.
- ~~Shared API types~~ ruled 2026-07-11 (same day) as **D69**: the
  derive rule is scoped to identity bytes; a typed `datboi-api`
  crate owns every /v1 shape, emits checked-in OpenAPI behind a
  staleness gate, and the web build generates TS from it. The M5
  stopgap (hand-written TS pinned by integration tests) is dead.
  Browser hardening (CSP + Fetch-Metadata CSRF) ruled alongside as
  **D70**. Residual contract imprecision worth a later pass:
  `WhoamiResponse` and `ImageStatus` describe invariants
  (`authenticated ⇒ role`, `minted ⇒ hash`) as independent optional
  fields rather than oneOf discriminated unions — the generated TS
  lost precision the hand-written types had encoded, and screens
  now guard defensively; `EntryRow.wanted_hash_algo` keeps its enum
  in prose. utoipa supports oneOf; upgrade when it next itches.
- **Scrub runs and verify methods aren't recorded**: the index keeps
  per-blob `verified_at` only — no method, no scrub-run ledger — so
  `/v1/storage` cannot report last-scrub and the entry drawer's
  verify line shows a date without a "how". Closed except one
  residual, 2026-07-11: `datboi scrub` stamps a terminal ledger row
  (D74 amendment), and `/v1/storage.last_scrub` + the Scrub card now
  read it. Still unrecorded: the verify METHOD per blob (the entry
  drawer's "how" — wants a column when scrub grows methods worth
  distinguishing).
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
  eval history or per-snapshot diff is stored — the D74 ledger is the
  place for eval rows when that screen gets built. The
  eviction planner (§3.7/§3.8) shrank rather than shipped: D72 made
  watermark eviction automatic (the Storage card now tunes
  watermarks instead of promising a plan-approval flow), so what
  remains open is only a plan PREVIEW surface (dry-run over the API;
  the CLI's --dry-run is the only entry today).
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
  stay explicitly-future per the comps, UI reserves their slots
  (Play since ruled 2026-07-12 as D84, see emulation.md).
- **Dat import graduated from CLI-only** (2026-07-11, post-ship):
  the M5 "mutating actions stay CLI-only" ruling was really about
  long-running pipeline work wanting live progress and a job
  registry (ingest, eval, mint, evict, scrub) — dat import has
  neither problem: it is request-sized, bytes in / report out, and
  the CLI path buffers the whole file the same way. So it became the
  first (and so far only) mutating /v1 operation:
  `POST /v1/dats/import` (raw dat bytes as the body — no multipart,
  one file IS the request; provider/system overrides on the query
  string; 512 MiB route-level body cap clears MAME's listxml), and
  the Library screen's dashed empty-card became a real drop-zone +
  file-picker with a per-file receipt log. The other pipeline
  actions still wait on the job registry above.
- **ROM ingest graduated from CLI-only** (2026-07-11, post-ship):
  the hard sibling of dat import — files run to GBs and the pipeline
  outlives any sane request — so it shipped as two phases plus the
  minimal job registry the Jobs-tray entry above deferred to.
  `POST /v1/ingest/uploads?name=<relative path>` streams one file's
  raw bytes (no multipart, no body cap — a D56-style headroom guard
  instead) through a bounded channel into `<store>/tmp/` staging
  (same filesystem as the store; swept by the existing cleanup_temp;
  never fsynced — the durable publish is put_new's rename during
  ingest), answering an in-memory token. `POST /v1/ingest` spends
  tokens all-or-nothing and runs one Ingester per file on a plain
  background thread — the db lock releases between files, and
  progress is byte-weighted at file granularity (capped 99 while
  running). Report paths wear the client's original names; staging
  paths never leak. Transport ruling: REST + polling (tray 2 s while
  running only, screen 1 s on its job) — upload progress is the
  browser's own XHR meter, and server-side events are file-granular,
  so SSE/WS buys nothing today. Custody over HTTP is always copy
  (the browser cannot move originals); NAS-local ingest stays CLI.
  The web Ingest screen is the real spec §3.6 flow now: drop files /
  zips / folders (webkitGetAsEntry traversal, readEntries batching
  handled) or pick either, per-file upload bars, then the step-2
  report card (new blobs · dupes · archive members · refused).
  Detectors became daemon config too (`Config.detectors_dir` from
  the global `--detectors`/`DATBOI_DETECTORS`) so web ingest applies
  the same skipper set CLI ingest does. Follow-up fix: the job (and
  CLI ingest, same gap) now runs relink_all + refresh_rollups at the
  end, so freshly ingested content lights the shelf immediately —
  previously that pair only ran at dat import/view eval, leaving a
  matching upload dark until an unrelated eval happened by.
- **The drop surfaces unified** (2026-07-11, same day): users
  shouldn't need to know which upload box wants which bytes, and
  No-Intro/Redump ship dats ZIPPED — which nothing accepted. So the
  ingest job now classifies every staged file by content (the house
  philosophy: magic bytes and `datboi_formats::detect`, names never
  decide): a file whose head detects as a dat imports via
  `import_dat` (full-buffer, the dats.rs 512 MiB reasoning); a zip
  whose central directory names EXACTLY one member whose head
  detects as a dat imports that member (extraction bounded by the
  declared size — `zip::read_sole_member` in datboi-ingest, riding
  the D35 walker; a multi-member zip is a ROM container by
  construction and is never sniffed further); everything else runs
  the pipeline unchanged. The report gained a `dats_imported` lane
  (client name + resolved provider/system + entries) — pipeline
  counters stay pure, a dat import is not a `files_scanned`. The
  Library empty-card now rides the same staged flow (upload → job →
  poll → receipts from the dats lane), so zipped dats finally
  import from either screen; `POST /v1/dats/import` stays as the
  direct-API contract path.

## Flagged (raised 2026-07-12, usability review session)

The review ruled D78–D82 and wrote [web-ui.md](web-ui.md);
two things were seen and deliberately deferred:

- **Screen taxonomy naming**: "Audit" is a CAS-author name for what
  the user experiences as *the library list* — and Library / Browse
  / Shelves / Views overlap in ways the nav ruling (Library · Views
  · Ingest · Storage · Admin) papers over at the component level.
  No user-visible bug today; wants a naming pass when the next
  screen gets added, not before.
- **Recipe deep-link page**: long recipes now summarize + expand in
  place (web-ui.md, aggregates-before-enumerations). Recipes are
  content-addressed meta blobs, so a dedicated page is *possible* —
  build it only if recipes grow multi-level structure worth
  deep-linking; another CAS-debugger surface is the failure mode.

## Flagged for ruling (raised 2026-07-15, residency rulings session)

- **Grounded-not-resident sweeps** (the absent-blob analysis gap;
  RULED same day as **D92** — analyzers consume the logical CAS,
  eagerness/budget/sniff-admission stay molten, grounded-set-aware
  enqueue named as owed design work; kept for the reasoning):
  the refinement fixpoint silently stalls on claimed-but-absent
  identities — sweeps only claim Resident blobs and analyzers read
  via `store.get`, but the analyzer contract ("pure function of
  bytes", D45) only needs the LOGICAL layer, and the executor can
  already stream any grounded blob verified. Concrete stalls today:
  an .nds STORED (uncompressed) in a zip is claimed at ingest and
  never analyzed — preflate only materializes DEFLATE plaintext,
  stored members live in the skeleton — so no NitroFS claims, no
  trim alias, on a dat-matched ROM provably held; same stall for
  members of preflate-refused containers (the D24-tax zips); and
  every future nested interior (NARC-inside-NDS) gates on
  materialization events that happen for unrelated reasons. The
  happy path works by side effect (preflate and 7z/rar extraction
  happen to materialize what the next analyzer needs), not by
  mechanism. Proposed shape: sweeps claim GROUNDED, not resident;
  analyzers read through the executor (TickReader wraps an executor
  stream as happily as a file; identity, provenance rows, leases
  all unchanged); cost policy decides eagerness — absent blobs
  enter queues when dat-named or head-sniffed interesting, the D71
  dat-aware-scheduling lane. This changes an architectural posture
  (analyzers consume the logical CAS, not the literal store), so it
  wants its D-entry before code. Dependency note: D91 unblocks
  depth-2 analysis only incidentally (pieces happen to become
  resident); this is the general fix and the most foundational step
  of the decomposition arc in the position note below.

## Next sessions (pick up here)

**Position as of 2026-07-16, newest (D96 backend pass — on-demand
maintenance COMPLETE)**: punch-list item 4 is done in five commits.
The scrub corpus walk (store walk + `scrub_pack` + rehabilitation)
descended out of `cmds.rs` into `Executor::scrub` (new
`datboi-exec/src/scrub.rs`, returning a `ScrubReport`); the CLI keeps
only the printer. Then the four maintenance verbs reached serve, each
integration-tested over a live daemon (`crates/datboi-server/tests/api.rs`):
`POST /v1/scrub` (Scrub job, private connection so the walk never holds
the pipeline mutex; byte disproofs stay findings, not failures — D81),
`POST /v1/evict` (`dry_run` → the D27 plan preview synchronously;
real → D72-guarded Gc job, busy guard is a 503 not a stillborn job),
`POST /v1/sweep` (Refine job over the logical CAS; the name→analyzer
factory descended to `datboi_ingest::analyzers::analyzer_for` +
`SWEEP_ANALYZERS`, shared with the CLI), `POST /v1/snapshot`
(synchronous `statesnap::mint`, signed with the instance identity).
`App` gained `db_dir` for private-connection maintenance jobs. Workspace
green, clippy clean, web `npm run check` clean (schema.d.ts regenerated).
NEXT: **punch-list item 5 — the dat lifecycle** (fetch / diff /
clonelist / export). `dat fetch`'s ureq+redump+zip-unwrap logic descends
out of `cmds.rs` first, same descend-then-graduate discipline. After the
backend contract fully settles, the deferred web-UI pass (define form,
analyzer & gc-policy config panels, materialize/scrub/evict/sweep/
snapshot triggers) builds against it. Hermetic `nix build .#datboi` NOT
run this session (no flake/build.rs/deps changes; new source files were
git-added so cargo/flake see them) — run it before a release if paranoid.

**Position as of 2026-07-16, latest (D96 — serve+web is the complete
surface, CLI is convenience)**: parity audit done; posture INVERTED and
ruled as **D96**. Every capability must reach the HTTP+web surface;
both surfaces call one shared library fn per verb (correct-by-
construction), and stranded entrypoint logic descends into a library
crate before it graduates. **Backend-first pass underway** (web UI for
the new endpoints is deferred to a focused UI session — build against
the settled contract). Punch list with landed status:
  1. **Read-model de-dup** — DONE (core): the 4-state entry vocabulary
     (verified/claimed/missing/nodump) descended to
     `datboi_catalog::state` (`RollupState` + `STATE_CASE_SQL`, proven
     equal by a sqlite-vs-Rust test); server bridges to the wire enum.
     (The SUM-in-SQL per-source counts stay — a perf choice — now over
     the shared fragment. A fuller `source_counts()` shared fn is
     optional polish, not required.)
  2. **View authoring** — DONE: `GET /v1/view-profiles`,
     `PUT /v1/views/{name}` (define), `POST …/eval` and `POST …/image`
     (both jobs; new `JobKind::Eval`/`Mint` in the shared ledger). New
     server `views.rs` module.
  3. **Config surfaces** — DONE: `GET /v1/analyzers` +
     `PUT /v1/analyzers/{family}` (family list descended to
     `refine::FAMILIES`); `GET`/`PUT /v1/gc/config` (watermark
     parse/Display/setters descended to `policy`, CLI now shares them).
  4. **On-demand maintenance** — DONE. `POST /v1/blobs/{hash}/materialize`
     (synchronous), then the four verbs landed 2026-07-16 (below):
     `POST /v1/scrub` (Scrub job; corpus walk descended to
     `Executor::scrub`), `POST /v1/evict` (dry-run plan + guarded Gc job),
     `POST /v1/sweep` (Refine job; name→analyzer factory descended to
     `datboi_ingest::analyzers::analyzer_for`), `POST /v1/snapshot`
     (synchronous `statesnap::mint`). The verify endpoint's stale
     `datboi scrub` deep-link now points at materialize. No new
     `JobKind` (scrub→Scrub, evict→Gc, sweep→Refine), so the web
     exhaustive switch is untouched.
  5. **Dat lifecycle** — TODO (the remaining pick-up): fetch / diff /
     clonelist / export.
     `dat fetch`'s ureq+redump+zip-unwrap logic descends out of
     `cmds.rs` first.
  Explicit CLI-first exceptions (NOT gaps): `recover`, bootstrap
  identity/token minting. `view sync` stays local-fs CLI, but its
  verified-write primitive is shared library code.
  Web deferred: the Activity screen learned `eval`/`mint` kinds (forced
  by the exhaustive switch); everything else — define form, analyzer &
  gc-policy config panels, materialize button — awaits the UI session.

**Position as of 2026-07-16, later (loose-thread + decomposition-arc
sweep — all nine items landed, workspace green, clippy clean)**: the
D91/D92/D93 loose ends and the decomposition arc are DONE, in order.
D91: pack scrub (`scrub_pack` re-hashes whole packs against identity,
one read, certifies every member + back-fills aliases); obao blessed at
swap time over each packed piece's window (no first-serve stall — a
refinement of the pack-time-obao rejection); tombstone-and-repack
(`Store::repack` rewrites a pack without its dropped members; orphan GC
routes packed pieces there since `remove_blob` can't unlink pack bytes,
the one real correctness gap this closed); pack-per-chunking
(`pack_chunk_sets` maintenance phase consolidates the loose chunk flood,
policy-gated `chunk:pack`). D92: grounded-set-aware enqueue is fixpoint
DEDUP — `refresh_admission` runs the grounding fixpoint ONCE per wake
(was once per family); `sweep_absent_eligible` is the within-tick cache.
D93: the write audit split the request path into THREE lanes (pipeline
mutex / quick-write pool for auth+admin / read pool; gc/orphans GET
un-miscloseted); `refine:workers` live-reloads (prime resizes the
fungible drone fleet each ambient tick); the family-job lingering was
reviewed and KEPT (correct — the job stays open while a family has
in-flight leased items). DECOMPOSITION ARC: rank-7 D59 amendment
(`is_covered_by_others` — grounded WITHOUT the blob's own literal —
replaces has-any-recipe, so D91 grounding-leaf pieces get chunked; +
resident-only guard); NARC interior decomposition RULED+BUILT as D94
(`narc-split/1`, builtin-affine, shared `mint_decomposition` with
nds-split, recipe-volume gated, bit-faithful round-trip test). STILL
OWED (unchanged, genuinely wasm-shaped): SDAT audio + LZ-compressed
NARC MEMBERS (the codec, a separate wasm lane + ruling), the NDS
secure-area/modcrypt decrypt lanes (console keys), and — noted not
built — sharing the D92 fixpoint with the audit rollup (cross-cadence),
the swap guard-hold spanning materialization at disc-image scale, and
grounded-set-aware enqueue COST at corpus scale. Hermetic
`nix build .#datboi` NOT run this session (no flake/build.rs/deps
changes — new source files were git-added so cargo/flake see them);
run it if paranoid before a release.

**Position as of 2026-07-16 (D93 — fearless concurrency, ruled and
built same day)**: refine drains are multi-threaded by default (prime
+ drones, default ⌈n/2⌉ clamped to 6 after the formula challenge —
memory/IO-shaped, not CPU-shaped, see the D93 amendment; molten
`refine:workers`; leases ARE the dispatcher; one shared Sync
executor); request-path reads left the
write mutex for a READ-ONLY connection pool (flags-level fence — a
misclassified handler errors loudly); every read-write connection
defaults to IMMEDIATE transactions at open (mechanical — the
deferred-upgrade class is unrepresentable, not audited-for), with
`Db::*_write_tx` as the self-documenting spelling. The hunt that
followed the drone build caught and fixed, in order: the BUSY
upgrade, a maintenance-after-refinement ordering race (drone finishes
after the prime's drain → maintenance_due signal), a textbook
lost-wake (signal flag outside the condvar mutex → folded into the
prime's inbox), and lying tray notes (now fleet-wide provenance
deltas + a completion gate). Soaked: repeated full-workspace runs
green. OWED from D93: the per-surface write audit that would let
writes pool (row-guarded check-then-act; until then the write mutex
is the named argument), `refine:workers` is boot-time (live reload
if anyone cares), and drone bursts hold the prime's family job open
across families (cosmetic lingering, bounded by burst length).

**Position as of 2026-07-15, later (build session)**: **D91 AND D92
ARE BUILT** — the same-day landing of the morning's rulings, seven
commits, 378 tests green. D92: cache schema v6
(sweep_absent_eligible), `refresh_absent_eligibility` (grounding
fixpoint ∩ eagerness KV `refine:absent:mode`, default dat-named,
EvictedCovered admitted unconditionally), the `Logical` byte source
(resident → store file; grounded absent → executor spill, re-hashed,
never a residency flip), all four analyzers ported, both sweep
drivers wired. The flagship stall is a regression test: an .nds
STORED in a zip gets NitroFS-split with nothing ever materializing
it. D91: sealed packs live in the STORE (footer-scan resolution —
see the D91 amendment), `Store::get` returns the windowed `Blob`,
swap planner as an ambient maintenance phase (predicate → headroom →
pack → license → evict under the D72 guard), `Blocked::Packed`
eviction refusal. E2E: two synthetic variants sharing ~79% of piece
bytes swap into two packs and serve byte-exact + range-verified
afterward; the loner never trips; re-runs no-op. OWED from the
landing: pack scrub coverage (store `verify`/`list` are loose-only —
a scrub pass should re-hash pack members against footers);
`ensure_obao` over pack windows (packed pieces currently serve
ranges via the D4 literal plain-read default); tombstone-and-repack;
pack-per-chunking; grounded-set-aware enqueue cost at corpus scale
(unchanged from the morning); and the swap phase's guard hold spans
its materialization — fine at DS scale, revisit if packs grow to
disc-image size (GUARD_TTL is 15 min).

**Position as of 2026-07-15 (residency rulings session — docs only,
no code)**: **D90, D91, and D92 RULED.** D92 landed after the arc
was mapped: analyzers consume the LOGICAL CAS (sweep candidacy is
grounded-not-resident, analyzers read through the executor) — the
posture ruled, eagerness policy molten, grounded-set-aware enqueue
cost the named owed design work. That converts arc step (1) below
from "wants a ruling" to "wants a build," and its implementation
naturally precedes or rides the D91 build (same sweep-driver
surface). D90 closes at-rest compression:
delegate to the filesystem, loop-device advice for ext4/xfs,
store-level encoding rejected until a filesystem-less backend
(S3/HTTP) needs it — retrofittable by construction, so nothing is
foreclosed. D91 rules the affine piece-swap: pieces over container
when the rebuild route is affine, gated on a plan-time sharing
predicate (never eager — lone ROMs never trip it), materialization
writing one sealed pack per decomposition (D19's packing clause
first exercised), D56 disk-headroom guard as prerequisite, run as a
maintenance phase (D47 intact, sweeps untouched). Nothing
implemented yet — the build is a swap planner phase + pack
write/read paths + the headroom guard. The session also mapped the
DECOMPOSITION ARC these belong to, in dependency order: (1)
grounded-not-resident sweeps (RULED same day as D92 — build owed),
(2) D91
implementation, (3) NARC/SDAT interior decomposition (existing
wasm-lanes item), (4) the rank-7 D59 amendment (existing item —
D91 creates the population its gate mispredicts; NARC/SDAT eats the
archive-shaped near-misses exactly; CDC takes the media-stream
remainder). Each step's evidence trigger fires as a consequence of
the previous step landing, so re-check triggers after each landing
rather than re-litigating order.

**Position as of 2026-07-14 (ABI epoch, D89 — LANDED)**: the break
shipped the same day it was ruled, whole: new wit tree (three lanes,
CBOR vocabulary), vending crates (`datboi-guest-transform`/
`-extractor`, no_std+alloc, every in-tree guest consumes them),
runtime hosts re-cut (whole-buffer host deleted), exec/ingest ported
(batched extraction, cap 128/pass), fixtures re-blessed, goldens
re-pinned, dev stores wiped, wkg publish + keyless cosign in
container.yml. `nix flake check` green. Empirical answer recorded in
worlds.md §landed notes: wit doc edits DO churn component bytes —
wit text freezes with its version; the golden pins are the tripwire.
Deferred, deliberately: crates.io publication of the guest crates
(wit vendoring at publish time is designed, not built — a git dep
serves until someone external asks); world-level extractor params
forwarding (recipe schema grows a forwarded subset when passwords
become real). Watch item: the FIRST main push after this publishes
`datboi:{streams,transform,extractor}@1.0.0` to ghcr.io/schlarpc/datboi/{streams,transform,extractor}
— confirm the job goes green and `cosign verify` works as documented
in the workflow comment.

**Position as of 2026-07-13 (saves design pass)**: docs/saves.md
opened — the design pass D62 reserved ("writable overlays … save
history for free"), written from the emu-worker end because saves are
the loudest play-surface gap. The model: a lineage **forest** (append
+ fork, no *automatic* merge), the file/state cleave on interop +
keying axes, raw component blobs in `data/` + self-identifying
`savenode` meta objects written at flush time (git's objects/refs
split — statesnap carries only naming refs, so save durability is
store-grade, not snapshot-cadence), exact-rom structured anchor
(title is presentation + explicit-offer fork, never automatic
cross-rev sharing), offline-first capture (OPFS write-ahead queue —
the train scenario is exactly where the daemon is unreachable), and
import/export adapters as the third producer / day-one consumer. Two
findings gate implementation: the state ring-buffer needs the store's
FIRST byte-destroying code path (posture-change D-entry + drill owed
— v1 stays explicit-only states so it can ship later), and shared
media (memcards / Controller Pak / MK64's EEPROM+Pak split) needs its
own `(media-instance, owner)` timelines when a memcard console lands.
Eleven rulings enumerated at the end of the doc, none D-numbered yet
— next session either rules the savenode shape (ruling 1, the
expensive-to-change one) or starts v1 against the proposed scope.

**Position as of 2026-07-13 (play-surface session)**: D85–D87 ruled
and shipped. D85: the audit drawer plays — per-rom ▶ for any claim a
local blob satisfies and a core claims, over a second Play source
(`/play/blob/{hash}/{name}` → `GET /v1/blobs/{hash}/bytes`, zero new
API; friends bounce off it like any owner route). D86: the touch
deck — clusters that never overlay the pointer screen (portrait:
below the stacked screens; landscape: the gutters), `(pointer:
coarse)` capability gate, per-pointer role latch / 8-way sectors /
slide-to-roll in pure unit-tested `lib/emu/touch.ts` (16 tests), and
the cluster layout derives from the descriptor's button set so the
NES core reuses it. D87: fullscreen — one immersive flag, CSS
takeover everywhere + `requestFullscreen` where present, windowed
768px canvas cap lifted in immersive. Drive-bys: Browse's ▶ pill was
silently unstyled (scoped selector can't reach into <Link>; fixed
with `:global`), and Play's back-hover named a never-defined `--fg`
token. Verified: 217 vitest + svelte-check + production build green.
The live-device pass HAPPENED (same day, ios-webkit-debug-proxy
against the real iPhone — see the D86 amendment): it caught an iOS
26 grid-percentage bug no emulation showed (canvas under the whole
deck; fixed by the layout-inert-canvas posture), a cluster-sizing
collapse (fixed by measured ResizeObserver fit), and touch-triggered
text selection (disabled screen-wide). Still unexercised: haptics on
Android, landscape deck feel, simultaneous stylus + button
multitouch in a real game. Saves persistence (item 4) stays the
loudest backlog gap.

**Position as of 2026-07-12 (emulation session)**: D84 ruled +
emulation.md written, and **spike milestones 1 + 2 shipped**.
M1: `nix build .#emu-ds` builds dust (rev-pinned git deps, nightly
2025-12-20 — 2026-02 nightlies break dust's portable_simd use, so
the pin tracks upstream's last-green, not latest) into
wasm-bindgen wasm + glue with a synchronous in-instance 3D renderer
(no atomics/SAB/build-std — dust-web's threaded renderer replaced
by an eager rasterize in `start_rendering`); the bare test page
direct-boots homebrew with both screens rendering. M2: the worker
protocol ships inside the core asset (asset/worker.js +
descriptor.json; postMessage = the GPL boundary), test page rewired
through it — steady 60 fps + exactly 32768 audio samples/s, 1558
fps stress throughput. TWO hard-won lessons: (1) a js_sys::Function
passed into the wasm instance hangs create_emu_state inside a
Worker on Chromium 148 headless (main thread fine, debugger
attached fine) — audio became a pull API (take_audio rides the
frame message) so no JS value crosses into wasm; if a future core
wants callbacks, don't. (2) Headless verification: plain
`--screenshot` real-time runs throttle timers AND serve stale
paints (screenshots lie); `--virtual-time-budget` doesn't drive
worker clocks. The working harness is CDP: attach, navigate, wait
real seconds, `Page.captureScreenshot` (forces a compositor frame)
+ Runtime console capture — the page counts as active under CDP so
nothing throttles (script shape: /tmp-era cdp-verify.mjs, trivially
rewritable). Emu lane rides `checks.emu-ds` in CI. **M3 shipped the
same session**: /emu/nds served from the daemon (D66 embed),
`'wasm-unsafe-eval'` in CSP, web lib/emu host + /play route + ▶ in
the Browse entry panel (ungated — play≡download, D84 amendment;
/shelf became an owner-reachable deep link), e2e-verified with a
live daemon + CDP click-through (dat minted for the homebrew,
`view define/eval`, shelf → panel → ▶ → pixels). **M4 shipped too**:
COEP require-corp in the D70 set + vite dev parity, verified
`crossOriginIsolated === true` with the emulator running — the
whole D84 spike is code-complete. THE POST-SPIKE TAIL (same
session, live iPhone testing driving each fix): save chips
(gamecode-keyed in-memory devices from dust's game_db — games hang
at boot probing for them), BIOS-from-CAS shipped (deferred item 1;
`GET /v1/blobs/{hash}/bytes`; MKDS boots), touch fixed twice
(pixel→ADC ×16 for dust's TSC — pixel units put every tap in the
top-left corner — plus letterbox-aware mapping for narrow screens),
firmware nickname = session username (CRC-verified patch of both
user-settings blocks; loopback owners are "datboi"), and iOS audio
survives app switches (every gesture re-asserts unlockAudio; audio
promise rejections are expected answers, not banners). REMAINING:
spike acceptance is a human check (a commercial title at full
speed with sound, interactively); the deferred-items entry above is
the ordered backlog (saves persistence now loudest, touch overlay
now concrete, friend BIOS, heuristic patch — see items 1/2/4/5/8);
AND one hygiene debt: flake/build.rs/API all changed this session
— `nix build .#datboi` (hermetic proof) + a clippy pass have NOT
been run over the final state; do that first next session.

**Position as of 2026-07-11 (GC session, after the M5 web sessions)**:
**D71–D73 SHIPPED IN FULL.** Analysis, licensing, and eviction are
now ambient in serve mode: D71 (one niced worker thread, private Db
connection, fresh > dat-matched > ambient priority tiers,
progress-gated heartbeat leases claimed at execution granularity —
timer-heartbeat and upfront-batch-claim explicitly rejected), D72
(watermark eviction armed by default at 90/85%, eager storage-neutral
licensing of the verified-only pool, the gc_guard singleton — the ONE
correctness lease — shared by daemon/CLI/apply; candidate ordering is
seek-class-first after the e2e caught size-first stranding the
container⇄plaintext inverse pair backwards), D73 (orphan sweep:
reachability-only roots — custody is deliberately NOT a root, ruled —
mark→grace→review→apply with delete-time re-verification; keep-marks
are authoritative state KV; deletion is the one human-gated action,
via Storage-screen card / `datboi gc` / the /v1/gc API). Cache schema
v4 (leases, gc_guard, orphan_candidate). Web ingest provenance fixed
(source_file keys on the client name, not the staging path). E2E:
drop-a-zip → refine → license → container auto-evicts in one wake;
full orphan keep/apply lifecycle over the live API. NEXT candidates:
the durable job table (three entries below depend on it and the tray
is now busy enough to make restart amnesia visible), fuzz targets for
the wild-byte parsers (D58 hygiene tail), snapshot auto-cadence (see
the updated entry — keep-marks raised its stakes), quarantine-review
design (the orphan review card is the pattern to reuse).

**Position as of 2026-07-10 (third session of the day)**: **M4 IS
COMPLETE.** After the FAT32 session (below), the M4 tail shipped in
one sweep: D59 (chunking narrowed to route-less literals), D56
(disk-headroom guard in materialize, statvfs via rustix), D60
(analyzer config: family() on the trait, enable/params KV rows,
`datboi analyzer` CLI, sweep gate), D61 (verified already
implemented), name-fitting pipeline + alpha-bucketing +
ezflash-omega profile (views.md owed work), D57 (strict 1G1R as
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
(views.md); then the D58 unrar-wasm lane.

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
   dir-bucketing (views.md, recovered 2026-07-10 from the 2021
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

See [decisions.md](decisions.md) (D1–D73).
