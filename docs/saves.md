# datboi — save persistence, lineage & attribution

*Status: DESIGN PASS OPEN, no D-entry yet (2026-07-13). This is the
pass D62 reserved — "writable overlays (writes are ingests, per-device
overlay, **save history for free**) + dirty-image diff-back are pended
to a design pass before nbd/live-write serving." D62 also stands as
the current failure mode it names: "REFLASHING CLOBBERS ON-DEVICE
SAVES." This doc proposes the model and enumerates the rulings owed;
nothing here is ratified until it lands as a D-entry. It is written
from the emulator-worker end (the play surface D85–D87 shipped with no
persistence — the loudest backlog gap), but the store it designs is
the same one the overlay/nbd path eventually writes into. [emulation.md]
(emulation.md) governs the cores that produce saves; [cas.md]
(cas.md) owns the bytes; [views.md](views.md) owns the overlay
that is the second producer.*

## Why saves are categorically new

Everything datboi has stored until now is **re-derivable**: rom bytes
you can re-download or reconstruct from a recipe, indexes you can
rebuild from the store, snapshots that only need their latest. A save
is the first artifact in the system that is **genuinely lost if lost** —
no dat carries it, no recipe reconstructs it, it exists nowhere but
the one place you wrote it. That single property is what makes this
"a lot to get right": the durability bar is higher than anything the
CAS has had to clear, and it is why the timeline record (below) must
ride the recovery path rather than sit in a nukeable table.

## The core model: git-shaped, a forest of trees

A save at time *T* is just bytes → the CAS ingests it and stores it,
deduped, today, with no new machinery ("writes are ingests", D62).
The **only** genuinely new persistent structure is the mutable part:
a per-game **timeline** of snapshot references with parent links.

- **Snapshot** = a content-addressed blob (the save bytes) + metadata
  (created_at, producing surface, producing core+version, kind).
- **Timeline** = the parent-linked DAG of snapshots for one game.
- **Head(s)** = the mutable tip pointer(s) of a timeline.

Saves never merge **automatically** — you cannot 3-way-merge SRAM
bytes — so the structure is a **forest of trees**, append-and-fork
only, no merge nodes and no conflict-resolution algorithm in the data
model. This is strictly simpler than git. One honest caveat, so the
eventual D-entry doesn't get overturned by the first family sharing a
cart: at SUB-media granularity a merge can be well-defined — one SRAM
chip holds three Zelda file slots; a memcard holds many games' files —
and "the kid played file 2 on the couch while I played file 1 on the
train" is a merge a user will legitimately want. That is user-invoked,
structured, future work (§shared media); the byte-level model stays
merge-free. Consequences that fall out for free:

- **Lineage** is the parent chain. **Forking** is branching off any
  historical snapshot. The "scary" feature is the cheap part of the model.
- **Multi-surface divergence is not a conflict** — play offline on the
  phone and the desktop and you get two heads; that is an *unintended
  fork* the tree already represents natively. You name or prune it;
  you never (automatically) merge it. The feature you wanted (forking)
  is what de-scares the concurrency case.
- **Parent links are GLOBAL node references.** An explicit cross-rom
  fork (§attribution) gives a node a parent in another timeline —
  timelines are groupings, not containment boundaries.
- **Ordering is daemon-assigned.** Nodes carry a daemon-issued
  sequence; client timestamps are advisory metadata only (multi-device
  play guarantees skewed browser clocks).

## The master cleave: save FILES vs save STATES

One distinction resolves GC, attribution, and interop at once. Every
downstream section keys off it. The cleave is **not** "does hardware
do it" — capable flashcarts (EZ-Flash, EverDrive OSes) snapshot too, so
both kinds can be produced by the overlay/flashcart path and both can
land on a filesystem. The real axes are **interop** and **keying**:

| | **Save file** (SRAM / EEPROM / flash / memcard) | **Save state** (machine snapshot) |
|---|---|---|
| What | what a real cartridge/console persists | full emulated-machine freeze (emulator OR capable flashcart) |
| Interop | portable cartridge contents — **adapted** across wrappers | machine+format exact — **never adapted**, only version-gated |
| Attribution | anchored to the **exact rom** (title = presentation layer only, §attribution) | `(rom-hash, core-id, state-format-version)` |
| Size | KB–MB | MB, larger |
| Lineage worth | high — the durable thing | convenience/scrubbing |
| GC | conservative; never drop a live head | aggressive; ring-buffer + bookmarks |

Getting this cleave into the data model up front is what keeps the two
GC policies from blending into mush and keeps the *adapt* machinery
(wrappers, overlays) pointed only at files, where interop is defined —
never at states, whose bytes we move but never reinterpret.

## Storage model: raw payload, structured node

The sharpest question in this design — CBOR-wrap the save, or store raw
bytes? — resolves by **splitting the object from its payload**, the way
statesnap already splits its signed envelope from the alias batches it
references (D43).

- **Save bytes stay RAW content blobs in `data/`**, one blob **per media
  component**. This is non-negotiable, for a concrete reason: D62's
  "writes are ingests → save history for free" equivalence and D63's
  affine carve-out both reason about **content blobs** — the overlay
  windows raw save bytes (cluster-aligned) into a reified FAT32 image,
  and a flashcart's SD card holds the *raw* save. Wrap the bytes in CBOR
  and the canonical stored artifact **diverges** from the on-device
  artifact (every overlay/interop read must unwrap), and a varying
  envelope header wrecks any future delta stream (§dedup). So the store
  holds the clean cartridge bytes; the emulator/flashcart-specific
  on-disk shape is produced by the overlay or download endpoint at the
  edge, exactly the way a reified image (D62) is produced at eval time
  and not stored.
- **Structure + metadata live in the timeline NODE**, a **CBOR object in
  `meta/`** that references the component blobs by hash. This is where
  everything that isn't cartridge bytes goes — and there is real
  metadata to carry (below). CBOR wins for the node; raw wins for the
  payload. You get structure *and* keep the D62/D63 equivalence intact.

This split is not a new compromise — it is **already ruled, as D18**:
raw data blobs are untyped and unwrapped, "type lives in edges
(referencing objects), not nodes," and git-style headers on payload
blobs were explicitly *rejected* for exactly the identity-divergence
reasons above. Wrapping save bytes in CBOR would relitigate D18.

### Composite saves: a snapshot is a set, not a blob

A single save session can span **multiple media** — GameCube memcard
A + B, on-cart SRAM + a memory card, an EEPROM alongside battery RAM,
or an RTC rider (Pokémon G/S). So a snapshot node references a **map of
named components** `{ sram, eeprom, memcard-a, rtc, ... } → blob-hash`,
each deltable and GC'd independently. Metadata the node records per
component:

- **Media kind** (SRAM / EEPROM / FRAM / flash / memcard / RTC) — mostly
  a property of the game+core, so the **core declares it**; we record it
  because the overlay/flashcart path needs it to *place* the bytes.
- **State-format version** for state components (see keying, below).
- Producing core-id, producing surface, created_at.

The **descriptor.json** (shipped, M2) is where a core declares its save
surface: which components it exposes, their media kinds, whether it does
save-states, and its state-format version + minimum-readable version. A
core that declares no file components simply has a state-only timeline.
Extends existing machinery; no new registry.

Open sub-question: RTC — canonical component of the save, or edge
adapter? Leaning: its own `rtc` component when the core exports it
separately; folded into `sram` when the cart interleaves it. Ruled once
a core with RTC lands.

### Shared media: the memcard is not part of the game

Some media spans games BY DESIGN, and games exploit that: a PS1/GC
memory card is a filesystem holding many titles' saves, and cross-game
reads are *gameplay* (Psycho Mantis reads your other Konami saves;
Suikoden II imports Suikoden I data). Even a single game can straddle
private + shared media — Mario Kart 64 keeps records in cart EEPROM
and ghost data on the Controller Pak. So shared media cannot live
inside one game's `(rom, owner)` timeline, and a per-game virtual card
would silently break the games that peek across it.

Model: a **shared-media instance gets its own timeline**, keyed
`(media-instance, owner)` — matching physical reality, where the card
is a thing you own, not part of a cartridge — and a game's snapshot
node **cross-references** the card snapshot it played against. DS/GBA
v1 never touches this; the node format just must not assume every
component is game-private. File-level card merges (well-defined when
the changed files are disjoint) are the user-invoked future work
flagged in the core model. Ruled when the first memcard console lands.

## Attribution: linking a save to a game

"Which game does this save belong to?" — and the tempting wrong answer
is "the 1G1R title." **A save anchors to the EXACT rom, never to a
title.** There is no guarantee a save from rev A loads correctly on
rev B, or a US save on the EU dump; save-format changes across revisions
are real, and silently sharing a timeline across an incompatible rev
would corrupt the one artifact in the system that is lost-if-lost. That
is a direct violation of this doc's durability thesis, so we do not do
it automatically.

- **Anchor = the exact rom**, for both files and states. A save file
  additionally carries no core-version (portable); a save state
  additionally pins `(core-id, state-format-version)`.
- **The anchor is a STRUCTURED field, not a bare hash.** Single-file
  roms degenerate to one blob hash (v1 DS), but multi-file games (MAME
  sets, cue/bin discs) have no single hash, and a soft-patched rom
  (translation/hack applied at load, if the play surface ever does
  that) anchors to the *booted* content identity, not the stored blob.
  `anchor: {...}` from day one — this sits in the expensive-to-change
  bucket with the node shape itself.
- **Title is a PRESENTATION + EXPLICIT-OFFER layer, not an identity.**
  The UI groups timelines by dat-game / D57 1G1R so "your saves for
  Zelda" reads coherently across the dumps you own, and it may *offer*
  "you have a US-rev save — try it on the EU rev?" as a deliberate,
  user-confirmed **fork into the other rom's timeline**. It never merges
  or shares a head across roms behind the user's back. Compatibility is
  the user's call, made once, recorded as lineage — not a guess the
  system makes silently.
- **Homebrew / dat-uncovered roms** need no special case now — they were
  the fallback under the old title-anchor scheme, but with the anchor
  already at rom-hash they are simply the ordinary case. (The M2/M3 test
  path boots dat-uncovered devkitPro homebrew, so this is the first case
  we ship, and it needs zero extra machinery.)

Grouping durability: the *anchor* (rom-hash) is immutable and immune to
dat churn. Only the *grouping/offer* layer consults the dat and must
tolerate re-import and 1G1R re-canonicalization (D57) — but since it is
presentation, a stale grouping degrades to "shown separately," never to
a corrupted save. Coupling to dats.md and D55/D57 lives entirely in
that soft layer.

## Where the timeline lives: meta/ objects + statesnap refs

Because a save is the first non-re-derivable artifact, its timeline
must be **as durable as the store itself** — and that rules out the
obvious design (a statesnap payload key, the way the config KV rides).
Statesnap is periodic: timeline pointers living in daemon-local state
until the next snapshot would lose **every save since the last one**
when the daemon disk dies (D19: the DBs never live on the NAS — the
NAS holds only authoritative bytes). Losing an hour of config changes
is a shrug; losing an evening of play is the exact failure this doc
exists to prevent. The fix is git's objects/refs split, and the store
already has both halves:

- **Objects: a timeline node is a self-identifying
  `datboi/savenode/1` meta object** (magic + type + version + strict
  canonical CBOR — the D18 shape), written to `meta/` on the NAS **at
  flush time**: content-addressed, append-only, crash-safe,
  immediately as durable as the store. D19 recovery already parses the
  small meta/ tree first (full graph + snapshot roots) — savenodes
  ride that existing scan with zero new recovery machinery. Each node
  carries its parent hashes, so **the entire forest reconstructs from
  the nodes alone**; heads recover as leaves.
- **Refs: statesnap carries only the small mutable naming layer** —
  fork names, bookmarks, which-leaf-is-primary — as an additive
  payload key (D43). Refs are tiny, so no sharding. Losing the refs
  degrades to "unnamed leaves, pick one" — never to data loss.

The bare-NAS drill that proves a view survives the nuke extends to
prove a save does. Snapshot *bytes* are ordinary data/ blobs,
re-fetched by address; cache.db carries the working indexes and is
rebuildable from the meta/ scan like everything else it holds.

## Two producers, one store

The storage model is capture-mechanism-agnostic. Two producers feed
the identical timeline:

1. **Emu-worker capture (path 1 — ship now).** The core exports SRAM /
   state bytes through the worker protocol we already own; the host
   ingests them and appends a timeline node. Narrow, both ends ours, no
   filesystem involved. This closes the loud gap without any overlay work.
2. **Filesystem-overlay capture (path 2 — later, pre-nbd).** A game
   running against a synthesized FAT32 view (D62) writes its save; a
   write-overlay intercepts the dirty clusters and ingests them. General,
   hard, and the thing D62 pended. Saves-as-files is the *motivating
   driver* for write overlays (the first thing anyone wants to write into
   a synthesized view), but path 1 does not depend on it. One rider so
   it isn't forgotten: **diff-back needs a remembered parent** — the
   sync record must retain WHICH snapshot was placed on the card, so
   the ingested dirty save parents correctly. D62's param pinning
   (image identity derived from the snapshot hash) makes this nearly free.

Recognizing these as the same *store* with different *capture* is the
simplification: "I can't keep my web-player progress" is fully decoupled
from the entire nbd/overlay future.

## Import & export: the third producer, and the day-one consumer

- **Import is the onboarding feature.** Users arrive with decades of
  `.sav` / `.dsv` / `.srm` / flashcart saves — for them "bring my
  saves" is a bigger draw than history. Import = adapter-strip (unwrap
  the emulator format to raw cartridge bytes) → ingest → a
  **parentless root node** with provenance metadata. The hairy part is
  DETECTION, not storage: N64's byteswapped SRA/FLA variants across
  emulators, RetroArch padding conventions. The adapter layer earns
  its keep here — and a wrong guess corrupts, so uncertain imports
  ask instead of assuming.
- **Export-current-head is a cheap v1 win.** Download the head through
  a wrapper adapter (raw → .dsv / .srm / flashcart layout) and carry
  your save to a flashcart TODAY — half the overlay path's value with
  none of its machinery, and it exercises the same adapter code import
  needs.

## Capture cadence: change-driven, not exit-only

Exit-only capture is **unacceptable specifically because the play
surface is a phone** — tab death, OOM-kill, and battery loss eat the
session with no unload event. So capture is change-driven:

- **Save files** capture on dirty detection, **debounced** — a quiesce
  window after the last write coalesces a burst of in-game writes into
  one snapshot — **backstopped by a flush on blur / unload /
  fullscreen-exit** (D87), and by a **max-interval forced flush**:
  some games use SRAM as scratch memory or autosave continuously, so a
  pure wait-for-quiescence policy would never fire for them.
- **Dirty detection must not require core cooperation.** The robust
  default is glue-side polling — worker.js hashes the exported save
  region every few seconds and compares. That is also the D84 posture
  (pull, not push; no JS value crosses into wasm). A core that can
  report dirtiness cheaply gets a protocol message as an optimization,
  never a requirement.
- **Save states** capture on **explicit user action**. The periodic
  auto-state ring (the rewind/scrub affordance) is NOT v1 and is wrong
  for phones as sketched — a DS state is several MB, so periodic
  capture means holding N of them in a tab the OS already OOM-kills,
  or shipping MBs over cellular every 30 s. Opt-in later,
  desktop-first.

### Offline first: the flush must survive a dead connection

The marquee scenario — couch to train — is exactly where the daemon is
unreachable, and a capture design that flushes straight to the daemon
fails it: offline play + tab death = total loss. So the flush is
**durable locally first**: the play surface writes captures to an
OPFS/IndexedDB write-ahead queue and syncs when connectivity returns.
A delayed sync that lands after the timeline advanced elsewhere is
just an **offline fork** — the forest absorbs it natively, no special
reconciliation machinery. This is v1 scope, not a nicety.

A cadence distinction worth pinning: a churning **working head** can
advance cheaply on every debounced flush, while **retained** snapshots
— the coarser, kept lineage nodes — are minted at meaningful boundaries
(explicit save-point, session end, fork). Not every dirty-flush becomes
a permanent tree node, or the timeline drowns in noise and GC has
nothing to reclaim. Where exactly the working-head/retained line sits is
an open ruling.

## Identity & multi-surface

- **Owner-only now.** A timeline is owned by an identity; on your own
  instance that is the owner you already have. Everything a v1 needs is
  answerable locally.
- **Guest + cross-instance = M6/iroh, additive.** A guest playing on
  someone else's box, and "my save made on your box, synced back to
  mine," is the global-identity convergence — and it is exactly the M6
  problem already on the roadmap. Design the model identity-*parametric*
  now; bind real cross-instance identity in M6. Not a nervous deferral —
  it rides a train already scheduled.
- **Guest saves are personal data on someone else's disk.** Even with
  binding deferred to M6, name the trust posture now: the host owner
  can trivially read a guest's saves. Encryption-at-rest, or an
  acknowledged trusted-host posture, is an M6 design INPUT, not an
  afterthought.
- **Play surface is metadata, not a partition.** A save must *not* be
  split per surface (couch-to-train continuity is the whole point);
  "made on the phone" is attribution on a snapshot, not a ref namespace.
  Divergence across surfaces is handled by the tree (a fork), per the
  core model.

## GC

Falls straight out of the cleave:

- **Save files:** conservative. Reachable-from-any-head snapshots are
  live; forks keep their history alive (mark-and-sweep from the live
  ref set, git-gc-shaped). Squash/thin deep linear history if volume
  ever warrants, but never drop a live head. Volume is small per
  snapshot (KB–MB), but see the dedup caveat below — history is not free.
- **Save states:** aggressive. Ring-buffer the recent N per timeline;
  explicitly bookmarked states survive; the rest are collectable. Big
  and disposable, so this is where the reclaimable bytes are, and
  ring-buffer GC is the primary lever until delta encoding exists.

### Eviction can't touch saves — and deleting dead states is the store's first byte-destroying path

The eviction story inverts the usual worry, in both directions. Both
sides are D-entry-worthy:

- **Accidental destruction is structurally impossible today.** D25/D27
  eviction reasons about *recipe-covered* literals — evict what a
  verified recipe can reconstruct. Save blobs have no recipes and no
  dat aliases, so the residency engine cannot select them, ever. Saves
  are safe from eviction with zero integration work.
- **Deliberate destruction does not exist yet.** The store is
  additive-only by ruling — zero eviction of unreconstructible bytes,
  no byte-destroying code path. The state ring-buffer REQUIRES one:
  dead states are unreconstructible bytes we *want* gone. That is a
  posture change to the store's core safety invariant, not a GC
  detail. It gets its own D-entry and its own drill (prove the deleter
  can never reach a node referenced by any retained ref) before it
  ships — and until it ships, the honest v1 consequence is that states
  accumulate. Keeping v1 to explicit-only states (no auto ring) keeps
  that accumulation tolerable.
- **Future interaction:** when delta-recipes land (below), save blobs
  become recipe-covered and thus D27-*eligible* for eviction — at
  which point D27's "never evict a literal whose cheapest verified
  reconstruction needs a literal that itself needs reconstruction"
  rule is doing load-bearing work on save delta chains.

### Dedup is NOT free — the tree is the delta base

Chunk-level dedup does **not** save near-identical snapshots: FastCDC's
fragment size (~256k) means a KB–MB save is a single chunk, so two
versions of it are two disjoint chunks with zero overlap. Storing full
history therefore costs one blob per snapshot — fine for small files,
punishing for a long tail of MB save-states.

Real save-lineage compaction needs **delta encoding against the parent
snapshot** — undescribed machinery today. The structural gift: the
**lineage tree IS the delta-base graph** — each node's parent link is
its natural diff base, per component, for free. So when delta lands it
has its base graph already built. Candidate eventual home: express a
snapshot as a **delta-recipe over its parent component blob** (the
recipe system already does windowed/derived byte production), keeping
saves inside the one CAS/recipe model rather than bolting on a private
diff store. This is a **deferred optimization, not v1** — v1 stores
whole components and leans on explicit-only states for the bytes that
matter.

A related commitment: **state components store RAW, not
compressed-at-ingest.** Compression would poison the future delta
stream and buys little that state GC doesn't; if bytes-at-rest ever
hurt, compression is a store-level concern behind the trait — the
same posture as D19's retrofittable packing.

## Proposed v1 scope (closes the loud gap, paints no corner)

1. Worker protocol gains component export/import (path 1); dirty
   detection via glue-side polling (a dirtied message only where a
   core offers one cheaply); descriptor declares the save surface,
   per-component media kinds, and state-format version.
2. Change-driven capture: debounced dirty flush + max-interval force +
   blur / unload / fullscreen-exit backstop (D87), **durable locally
   first** (OPFS write-ahead queue, sync on reconnect). Working head
   advances on flush; retained nodes at meaningful boundaries.
3. One timeline per `(anchor, owner)` with the anchor structured (v1
   degenerates to a single blob hash); snapshot node = self-identifying
   `savenode` meta object written to `meta/` at flush; naming refs ride
   a statesnap payload key; history browsable; fork = branch from any
   node; title grouping/offer is a soft UI layer.
4. GC: keep the file tree. States are **explicit-only** (no auto ring),
   so the byte-destroying deleter can ship later behind its own
   D-entry. Whole components; delta deferred.
5. Identity = owner only. Guest + cross-instance explicitly M6.
6. Save-states pin `(anchor, core-id, state-format-version)`; load
   gated on the running core's minimum-readable version — a
   build/commit bump that preserves the format keeps every state
   loadable.
7. Import (`.sav` et al. → parentless root node, ask-don't-guess
   detection) and export-current-head adapters.

## Open rulings owed (future D-numbers)

1. **The savenode object shape** — the one expensive-to-change
   decision. CBOR fields, the structured anchor, the component map,
   global parent refs, daemon-assigned sequence; plus the statesnap
   refs key beside it.
2. **The file/state cleave as a data-model commitment** — two kinds
   (both filesystem-capable, incl. flashcart states), two keys, two GC
   policies; *adapt* machinery points only at files.
3. **Attribution = exact rom, never title** — title is presentation +
   explicit-offer fork only; no automatic cross-rev sharing. Confirms
   the soft grouping layer's coupling to dats.md / D55 / D57.
4. **Storage split: raw component blobs in `data/`, CBOR node in
   `meta/`** (D18 applied to saves) — and where the import/export
   wrapper adapters live.
5. **State keying on declared format-version, not build-version** —
   the descriptor field + the load-compat gate.
6. **Capture cadence + working-head/retained policy** — debounce
   window, max interval, what promotes a working head to a retained
   node, offline queue depth.
7. **The byte-destroying deleter** — the store's first deliberate
   destruction of unreconstructible bytes. Posture-change D-entry +
   its own drill before any state ring-buffer ships.
8. **Shared-media timelines** — `(media-instance, owner)` keying,
   game↔card cross-references, user-invoked sub-media merge. Ruled
   when the first memcard console lands.
9. **Delta encoding over the lineage tree** — deferred; likely a
   delta-recipe over the parent component; note the D27 eligibility
   flip it causes.
10. **Identity parametricity now, M6 binding later** — the interface
    seam so the local model doesn't hard-code owner-is-everything,
    plus the guest-save privacy posture as an M6 input.
11. **RTC disposition** (deferred until an RTC core lands).
