# CAS core design

*Informed by research pass R2 (prior art: git, restic/borg/casync, OSTree,
iroh-blobs, RomVaultX). Decisions referenced as D-numbers in
[decisions.md](decisions.md).*

## Shape

Two stores with a sharp boundary:

1. **Literal store** — `blake3(x) → x`, whole blobs only, plus a bao outboard
   (blake3 hash tree) for blobs >16 KiB, enabling verified streaming and
   verified *range* reads (needed for filesystem views and p2p).
2. **Recipe store** — claims of the form
   `apply(transform, args, [input_blake3…]) = output_blake3`, where
   `transform` is itself a content-addressed wasm module (or builtin).
   Recipes are small canonically-encoded blobs living in the literal store;
   an index table makes them queryable by output hash.

Everything is one of these: roms, dats, chunk manifests, wasm modules,
recipes themselves.

### Addressing (D2)

- blake3 is the single native key. No multihash — agility we'd never
  exercise, and it fragments dedupe. If blake3 ever falls: linear re-hash +
  alias rewrite, a mechanism we already need.
- Dat hashes (crc32/md5/sha1/sha256) are **aliases**: computed in one
  streaming pass at ingest, stored as `(algo, digest) → blake3` in the
  metadata DB. sha1/md5/crc are lookup *hints*, blake3 is truth — sidesteps
  sha1-collision poisoning (colliding files are distinct blake3 objects both
  aliased to the same sha1; dat matching must tolerate multi-hit aliases and
  match on the dat's full hash set).

### Chunking (D3)

- **Not** in the base store. A chunked file is a recipe:
  `output = concat(chunk₁…chunkₙ)`. Same dedupe as restic, but per-object,
  optional, and the chunking policy is itself versioned + content-addressed
  (chunker identity pinned in the recipe — future tuning never invalidates
  old recipes).
- Strategy ladder at ingest:
  1. **Fixed-offset splits** for known headers (iNES etc. via header-skipper
     rules) — exact, free, better than CDC.
  2. **Format-aware decomposition** (split ISO into member files) — exact
     dedupe of shared assets across language variants.
  3. **FastCDC** (gear hash, NC level 2; ~min 64 KiB / target 256 KiB /
     max 1 MiB for disc images) as the opaque-format fallback.

### Residency

For any object the system may hold: literal bytes, one or more verified
recipes reproducing it, or both. A **residency planner** (policy) decides
what stays materialized; GC may drop literal bytes once a verified recipe
exists whose inputs are rooted. This one mechanism covers
"store decrypted, reconstruct encrypted on demand", "store chunks, drop the
solid file", and tiering generally.

### Verification & trust (D4)

Key insight: **recipes cannot lie about data, only waste resources** —
materialization hashes the output, which either matches the claim or the
recipe is garbage. Trust policy governs CPU spend and code sandboxing, not
integrity.

- Local ingest: verified eagerly (hashing is a byproduct of ingesting).
- Peer recipes: `unverified → verified(by, at) | failed`. Unverified claims
  may inform planning, but completeness reports distinguish
  `have(verified)` from `have(claimed)`.
- Determinism: a recipe pins `(wasm_module_blake3, canonical_args,
  input_hashes)`. Tool upgrades = new module hash = new recipe identity; old
  recipes remain valid forever because old wasm stays in CAS. There is no
  "current tool version" in a recipe, by construction.

### Compression at rest

Object identity is always the *uncompressed* bytes' blake3. At-rest
compression is a backend-internal encoding. Ruled (D90): delegate to the
filesystem — the store writes plain bytes, ZFS/btrfs zstd below it
compresses; store-level seekable-zstd encoding is rejected until a backend
without a filesystem (S3/HTTP) needs it, and retrofits without touching
identities if that day comes. Local stores on ext4/xfs: a loopback file
carrying btrfs/ZFS with zstd + discard/hole-punching is the documented
answer. Solid compression across similar roms (7z-style) is an *output
transform*, not a storage concern.

### GC

Root set = dat-driven pins + user tags + peer-serving tags. Recipes
contribute DAG edges (a rooted output with absent literal bytes roots its
recipe's inputs transitively). Mark-and-sweep over the metadata DB; no
refcount-only scheme (recipe DAGs make refcounts error-prone).

## Backend abstraction

```rust
pub trait BlobBackend {
    async fn has(&self, h: Blake3) -> Result<bool>;
    async fn len(&self, h: Blake3) -> Result<Option<u64>>;
    /// Verified range read: plain bytes + enough bao tree to verify.
    async fn read_ranges(&self, h: Blake3, ranges: RangeSet) -> Result<VerifiedStream>;
    /// Stream to temp, verify while streaming, atomic publish; reject on mismatch.
    async fn put(&self, expected: Blake3, data: impl Stream<Item = Bytes>) -> Result<()>;
    async fn delete(&self, h: Blake3) -> Result<()>; // may be Unsupported
    fn capabilities(&self) -> Caps;                  // ranges? write? list? cheap-has?
    async fn list(&self) -> Result<BoxStream<Blake3>>; // scrub/repair
}
```

- **local-fs / NFS+ZFS**: sharded dirs (`ab/cd/<hex>`), blob + `.obao`
  side-by-side; `tmp/<uuid>` → fsync → atomic `rename()`. Assume no
  reflinks, no reliable locks, no O_TMPFILE over NFS; single-writer daemon
  owns the tree. Scrub = periodic re-hash (ZFS handles bitrot below; this
  catches logical corruption).
- **S3**: one object per blob (+outboard), conditional PUT idempotency,
  encoding flag in object metadata.
- **HTTP**: read-only `Range` reads — also the "dumb mirror" story.
- **iroh**: both a backend (fetch by hash from peers) and a serving surface.

**Streaming discipline:** nothing may assume a blob fits in memory
(dual-layer DVD/BD images exist). All APIs are streams; no `Vec<u8>` paths.

## iroh-blobs store: deep-dive findings (R5, verified against 0.103)

Status correction: **iroh core is 1.0 (June 2026), iroh-blobs is still
0.x** (0.103; blobs 1.0 comes after, per their roadmap). 0.90 was a
ground-up rewrite (new db schema, new irpc API) with no documented store
migration — today the on-disk format is an implementation detail, *not* a
commitment.

What it offers (verified):

- Layout: redb `blobs.db` (metadata + inline blobs ≤16 KiB + inline
  outboards + tags) + **flat unsharded data dir** (`<hex>.data`,
  `.obao4`, `.bitfield`); `PathOptions` lets db, data, temp live on
  different filesystems — **db-on-local-disk + data-on-NFS is natively
  supported**. `obao4` outboards stay inline until data ~4 GiB, so most
  roms are one file (or one redb row) each.
- External/by-reference blobs: can index existing files in place (≤8 paths
  per blob) without copying; tampering is detectable on verified reads
  only.
- Partial blobs: chunk-range bitfield advanced only after covered data is
  durable — crash-consistent by construction; partials are servable and
  multi-peer-assemblable. Missing bitfield ⇒ startup revalidation scan.
- GC: `ProtectCb` receives the protected set before every sweep — exactly
  the residency-planner hook. Deletion is untag + async sweep (no direct
  delete).
- **Pluggability (the big one): since 0.90 there is no Store trait — the
  store interface is an irpc protocol.** FsStore/MemStore are actors
  speaking it; third-party stores are explicitly blessed; the p2p provider
  takes any store handle. "Their protocol, our store" is a supported
  architecture, not a fork.
- Impedance vs our trait: no put-with-expected-hash (adapter: add in a
  Batch under temp tag, compare hash, drop on mismatch); reader is
  `AsyncRead + AsyncSeek` (ideal for filesystem views); FsStore owns a
  tokio runtime and requires explicit `shutdown()`.
- Weak spots for us: redb-over-NFS is the risk concentrator (locking +
  COMMIT round-trips — hence db-local placement); flat data dir at ~10M
  MAME-scale blobs is hostile to READDIR/rsync and has **no sharding
  knob**; no fsck (buildable from `list`/`observe`/outboards); live rsync
  backup unsafe (ZFS snapshot fine only when db+data share a dataset —
  split placement needs quiesce or savepoint export).

**Decision (D14): our own store from day one.** iroh-blobs remains the p2p
plan — when p2p lands, our store speaks their irpc store protocol so the
provider/downloader ecosystem runs on top unchanged. Staging that keeps
day-one cheap: MVP is a complete-blobs-only store (local ingest always
completes; no partial-state machinery) with bao outboard sidecars for
verification and range reads; partial-blob bitfields + the irpc facade
arrive with p2p. Capabilities get added inside our format; we never
migrate off someone else's.

## Our store layout (D19)

- **Every blob is a loose sharded file** (fanout depth TBD — likely
  2 levels × 256 for ~10M-blob scale), `.obao` sidecar above the
  inline-outboard threshold; write `tmp/<uuid>` → fsync → atomic
  rename. No pack files: maximum boring, rsyncable, `ls`-comprehensible —
  the decades-scale format is literally "files named by hash."
- **Two top-level namespaces (D20)**: `data/ab/cd/<hex>` for opaque
  payloads, `meta/ab/cd/<hex>` for datboi structured objects (recipes,
  manifests, snapshots). Placement convention only — identity and serving
  are namespace-blind. Recovery parses the small `meta/` tree first (full
  graph + snapshot roots), then merely hash-verifies `data/` — no sniffing
  of millions of payload files; magic bytes retained as defense in depth.
- Accepted cost (explicit): full MAME scale ≈ 10M small files → NFS
  metadata churn on scans/scrubs. Mitigations: hot paths resolve via the
  local index (never READDIR), deep shard fanout, parallelized recovery
  scans. Packing remains retrofittable behind the trait as a pure
  optimization (identities unchanged) if it ever hurts.
- **Sealed packs (D91; format v2 per D105)** are that clause's first
  exercise: the affine piece-swap writes one immutable pack per
  decomposition (`packs/ab/cd/<hex>`): members back-to-back in
  coverage order, then an OUTBOARD SECTION of member-rooted obao4
  trees, then the self-describing footer, then a trailer carrying
  `blake3(footer)` + footer length + magic. The section's layout is
  fully DERIVED from the footer's `(hash, offset, len)` rows — trees
  in member order at the last member's end, each `outboard_size(len)`
  bytes, ≤ 16 KiB members contributing nothing (absence IS the empty
  sidecar) — and the parser enforces that data + section + footer +
  trailer tile the file exactly and that the footer matches its
  trailer hash, all in one small tail read: a plausible-but-wrong
  table is refused at open, never mis-sliced through the plain-read
  path. Resolution is store-internal — open() scans footers into a
  map; `get`/`has`/`len` serve packed members as bounded windows,
  indistinguishable from loose blobs to every consumer; `get_obao`
  serves a packed member's tree out of the section (a loose sidecar
  wins when both exist, mirroring `get`), so verified range reads and
  recovery need no loose `.obao4` for packed members — the trees are
  a byproduct of `put_pack`'s own member verification (bao root =
  blake3 identity), not a separate blessing pass. Identities
  unchanged; packs are write-once; a packed blob refuses eviction
  (`Blocked::Packed`) — reclaiming its bytes is TOMBSTONE-AND-REPACK,
  not an in-place edit. When orphan GC (D73) applies to a packed
  piece, it can't `remove_blob` (no loose file), so it groups the
  pack's dead members and `Store::repack` rewrites the pack WITHOUT
  them (survivors streamed and re-verified out of the old windows
  into a fresh sealed pack — trees recomputed on the way, the map
  flips, the old file unlinks) — or unlinks the pack outright if
  every member died. Inode growth is O(swapped decompositions), never
  O(pieces), and packed members carry no sidecar inodes at all.
  **Scrub covers packs**: the loose walk (`Store::list`) never sees
  packed members, so `scrub_pack` re-hashes each whole pack against
  its own identity (the filename) in one sequential read — a match
  certifies every member AND the section by construction (`put_pack`
  verified each member's bytes INTO the hashed file), a mismatch with
  all members verifying clean localizes rot to the section/footer,
  and the same pass re-derives per-member alias tuples for the
  fast-recovery back-fill. Packs are O(decompositions), so scrub
  reads them all rather than sampling. Section rot is fail-safe on
  use (obao is self-authenticating — validation fails, wrong bytes
  never verify); in-place repair is a deferred posture ruling
  (open-questions § scrub-repair).
- All embedded DBs on daemon-local disk (never NFS); NAS holds only
  authoritative bytes.

## Blob typing (D18)

- **Raw data blobs are untyped and unwrapped**: identity is exactly
  `blake3(bytes)` — required for dat alias resolution, iroh interop, and
  dedupe. Raw bytes have no intrinsic type; the same blob may be a rom in
  one dat, an ISO member in a recipe. **Type lives in edges (referencing
  objects), not nodes.**
- **datboi structured objects self-identify**: recipes, manifests, state
  snapshots are our formats, so their genuine content begins with
  magic + type + version (`datboi/<type>/<ver>` + strict canonical CBOR).
  Still plain blobs to the store; sniffable in recovery scans; DB carries
  typing in normal operation.
- *Rejected:* git-style type headers on all blobs (forks identity away
  from real-world hashes), per-blob sidecar metadata files (inode cost,
  drift).

## Recipe multiplicity & grounding (D21)

Recipes form an **OR-graph**: many recipes may claim the same output hash
(chunk-concat AND decrypt-from-variant AND peer-supplied alternative).
Index is `output_hash → {recipe…}`; verification status is per-recipe; the
residency planner is a cost-based optimizer choosing the cheapest verified
route given current residency. **Grounding rule**: mutually-inverse
recipes (`X from Y`, `Y from X` — e.g. headered↔headerless) are both
individually true, so GC must compute reconstructibility as a fixpoint
grounded in *retained literal bytes* — never allow circular coverage to
justify dropping both literals.

## Aliases as claims (D22)

Alias facts (`md5/sha1/crc/sha256 → blake3`) are outputs of pure functions
over bytes — locally they're derived cache (DB rows), batched into state
snapshots only to make recovery cheap. Across trust boundaries they become
**claims as CAS objects**: signed alias-batch blobs (the D8 p2p mapping
table is exactly this). Verification is free at fetch time: ingest
recomputes the full hash tuple anyway, confirming or refuting peer claims.
Strangers-without-bytes is the waddup ZKP slot. Aliases are recipes'
sibling: claims about hashes instead of claims about transformations.

## Rebuildability doctrine (D15)

Local databases are caches, never sole truth. Bare-NAS recovery:

1. Scan sharded files → hash→location index (doubles as fsck).
2. Sniff magic'd structured objects.
3. Load latest **signed state snapshot** (the recovery root): tags/pins,
   users/ACLs, config, dat-revision typing, and the **alias table**
   (crc/md5/sha1↔blake3 — including it makes recovery cheap; background
   scrub re-verifies by re-hashing later).
4. Re-run dat imports (deterministic functions of CAS blobs) → claims.

The daemon host is disposable; the only decades-scale format is
"hash-named files + packs on the NAS." Exception: the server identity
keypair is the one non-CAS secret — out-of-band backup (or
passphrase-encrypted in CAS).
