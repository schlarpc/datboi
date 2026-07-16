# Decisions

Lightweight ADR log. Each entry: what we decided, why, what we rejected.

## D1 — MVP slice: ingest + verify vertical (2026-07-03)

First milestone: point at a directory (zips/raw files), stream into CAS,
match against a couple of loaded dats, report have/missing/unknown. No
output transforms yet. Proves the storage + hashing + dat-matching spine.
*Rejected:* storage-library-first (too long without something usable),
dat-pipeline-first, thin-full-vertical (breadth risk).

## D2 — Native CAS address: blake3 (2026-07-03)

blake3 is the single storage key for all objects. Dat hashes
(crc32/md5/sha1/sha256) are indexed aliases computed at ingest, never keys.
Why: tree hash → verified streaming + verified range reads (bao); iroh-blobs
alignment for free; fastest available. *Rejected:* sha256 (no verified
streaming, no p2p alignment), multihash (unused agility, fragments dedupe).

## D3 — Chunking lives in the recipe layer, not the base store (2026-07-03)

Base store holds whole blobs only. Chunked/dedup'd representations are
recipes (`concat(chunks…)`), with the chunker identity content-addressed and
pinned per-recipe. Why: dats verify whole files (natural unit); keeps base
store invariants trivial; chunking policy stays evolvable; iroh/dat
interop needs whole-file hashes as identity. *Rejected:* restic/casync-style
chunk-tree base store (freezes chunk policy into storage format, reassembly
on every read); dual-notion hybrid as a *storage* concept (bao's 16 KiB tree
is a transfer/verify detail, not dedupe).

## D4 — Recipe verification: verify on ingest, trust after (2026-07-03)

Locally-created claims are verified at creation (hashing is a byproduct of
ingest). Peer claims are lazily verified (on first materialize or background
scrub); completeness reporting distinguishes have(verified) from
have(claimed). Rationale: content-addressing means recipes can't corrupt
data, only waste CPU — so eager global verification buys little.
*Rejected:* always-verify-on-materialize as mandatory (fine as a cheap
default for streamed reads, but not required by integrity), heavyweight
tiered trust machinery (the verified/unverified distinction already covers
it).

## D5 — Storage recipes are deterministically replayable, forever (2026-07-03)

Any recipe used for residency (i.e. that permits dropping literal bytes) or
shared p2p must replay bit-exact across versions and architectures: exact
wasm component pinned by hash, deterministic wasmtime config (NaN
canonicalization, no threads, no clock/random/fs imports for pure
transforms). Why: "drop the literal, keep the recipe" is the storage
thesis; it's unsound without replay. *Rejected:* verified-at-creation-only
semantics (would demote recipes to provenance records and forbid residency
dropping).

## D6 — Native fast-paths; wasm for semantics (2026-07-03)

blake3/alias hashing, bao, and baseline zstd are native daemon code. All
format-aware transforms and all peer-supplied code are wasm. Why: wasm is
~1.5–2.5× native — irrelevant for the long tail, wasteful for bytes-level
hot paths that run on every object; peer code never runs native.

## D7 — Own WIT world, wasip2 now, wasip3 streams later (2026-07-03)

ABI is `datboi:transform@1.x`, a WIT world we own. Implemented via chunked
pull/push streaming on wasip2 today; WASI 0.3 native `stream<u8>` is
adopted later as an internal migration (our world, not a format break).
Nothing in the ABI may assume whole-blob buffering. *Rejected:* betting on
wasip3 immediately (rustc target still tier 3), raw core-wasm C-style ABI
(loses typed interfaces + semver'd WIT packages). *Amended by
D41/D42 at the M1 freeze: `@1` is a whole-buffer profile (streaming —
wasip2-chunked or wasip3-native — becomes the future `@2` world), and the
build target is wasm32-unknown-unknown, not wasip2.*

## D8 — P2P trust sequencing: friends first, ZKP later (2026-07-03)

v1 p2p is friends-tenancy: NodeId ACLs + instance-signed
`(dat_hash → blake3)` mapping tables. waddup-style ZK proofs
(sha256↔blake3 binding) are the later upgrade enabling trustless public
swarms; same mechanism slot can eventually cover recipe claims.

## D9 — Adopt community metadata artifacts; rar is ingest-only (2026-07-03)

clrmamepro header-skipper XMLs and retool clonelists are consumed as
first-class inputs (interpreter for skippers; clonelists augment
parent/clone). Source dats are never mutated; filtering happens at
query/output time. rar: extract-only (license — no free compressor);
never an output format.

## D10 — Metadata DB: SQLite (2026-07-03)

Embedded, zero-ops, WAL, single-writer daemon fit, ad-hoc SQL for
reporting/completeness math. *Rejected:* Postgres (external service
contradicts single-binary homelab model; p2p — not a shared DB — is the
multi-instance story), pure KV (loses queryability). No premature
repository-trait abstraction for a hypothetical Postgres.

## D11 — MAME from day one (2026-07-03)

Arcade MAME dats are in the MVP scope, not deferred. Rationale (user call):
MAME is the hardest case (parent/romof/device_ref closure, bios sets, CHD
disk claims, merge modes, monthly churn) — exercising it immediately keeps
the schema honest; deferring it risks a model that quietly can't absorb it.
Amends D1: the ingest+verify vertical includes MAME set auditing alongside
No-Intro/Redump. Merge-mode *rendering* (split/merged/non-merged output
layouts) remains output-transform work.

## D12 — Console keys are CAS assets (2026-07-03)

Keys (prod.keys, boot9, WiiU common key, …) are ordinary content-addressed
blobs, referenced by decrypt/encrypt recipes as inputs:
`apply(decrypt, args, [encrypted_blob, keys_blob])`. Determinism and
provenance hold with no special-case key machinery; sharing keys with
friends is just blob ACLs. We never *distribute* keys with the software.
Flagged for the future public-swarm mode: a "sensitive" blob marking so
keys aren't advertised to strangers by default.

## D13 — Every dat kind, losslessly; software lists day-one (2026-07-03)

The schema accommodates all dat families from the start: Logiqx XML,
clrmamepro text, RomCenter (import-only), MAME listxml, and MAME software
lists with their full part/dataarea/loadflag structure modeled (attrs-map
escape hatch for the long tail), plus No-Intro P/C extensions. *Rejected:*
flattening software lists to plain rom lists (audits would work but
rebuild fidelity for merged/softlist flows would be lost, contradicting
the losslessness principle).

## D14 — Own literal store from day one (2026-07-03)

We implement our own on-disk store; iroh-blobs is the p2p layer only (our
store will speak its irpc store protocol when p2p lands, keeping their
provider/downloader on top). Why (user call, backed by R5): the at-rest
format is decades-scale and must be a commitment — iroh-blobs is 0.x with
a history of no-migration rewrites (0.90); no dir sharding; and
inline-blobs-inside-redb directly contradicts the rebuildability doctrine
(bytes must live on the NAS, DBs are disposable caches). Cost controlled
by staging: MVP = complete-blobs-only + bao outboard sidecars; partial
bitfields + irpc facade arrive with p2p. *Rejected:* FsStore-as-scaffolding
(double format migration), all-custom p2p protocol (never — bao/iroh
downloader ecosystem is the p2p value).

## D15 — Rebuildability doctrine (2026-07-03)

Embedded DBs (SQLite + any KV) live on daemon-local disk and are pure
caches. NAS holds authoritative bytes; small authoritative state
(tags/pins, users, ACLs, config, dat-revision typing, alias table) is
periodically snapshotted into CAS as a signed structured object. Recovery
from bare NAS: scan → sniff structured objects → latest snapshot →
deterministic dat re-import. Server identity keypair is the single
non-CAS secret. Corollary: no feature may keep sole truth in a local DB.
*Rejected:* DBs-on-NFS (embedded-DB locking/fsync dragons), authoritative
SQLite with conventional backups (host stops being disposable).

## D16 — No-Intro sourcing: manual drop + gentle fetcher (2026-07-03)

First-class manual daily-pack drop (file/watch-dir/upload) plus a polite
opt-in fetcher (backoff, conditional requests) that degrades to asking for
a manual drop when challenged. Redump/MAME/libretro/retool auto-fetch
normally. *Rejected:* scraping past CAPTCHAs (etiquette/bans), bundling
third-party mirrors as default trust.

## D17 — Web UI: Svelte (2026-07-03)

Matches the rof-gui vite + importNpmLock nix pattern; light runtime;
emulator-core embedding is framework-agnostic. *Rejected:* React (heavier,
diverges from existing tooling), Solid (ecosystem size).

## D18 — Blob typing: edges, not nodes (2026-07-03)

Raw data blobs are unwrapped; identity is exactly blake3(bytes) (dat
aliasing, iroh interop, dedupe all require it). A blob's meaning derives
from what references it. datboi structured objects (recipes, manifests,
snapshots) self-identify via magic+type+version at the head of their
canonical encoding — plain blobs to the store, sniffable during recovery;
the DB carries typing in normal operation. *Rejected:* git-style type
headers on everything (forks identity from real-world hashes), per-blob
metadata sidecars (inodes, drift).

## D19 — Store layout: loose files only (2026-07-03)

Every blob is a sharded hash-named file; no pack files. Why (user call):
maximum format boringness and tooling transparency (rsync, ls, ZFS-native)
outweighs inode/metadata cost; hot paths never list directories (local
index), and packing can be retrofitted behind the trait as a pure
optimization later since identities never change. Accepted cost: ~10M
small files at full MAME scale → slow NFS metadata scans (parallelize;
deep fanout). *Rejected:* append-only packs for small blobs (compaction
complexity, less transparent), everything-packed (range reads/partial
fetch complexity).

## D20 — Store namespaces: data/ vs meta/ (2026-07-03)

Opaque payloads under `data/`, datboi structured objects under `meta/`.
Placement convention only (identity and serving are namespace-blind);
magic bytes retained inside structured objects as defense in depth.
Why: recovery parses the small meta/ tree fully, then only hash-verifies
data/ — no content-sniffing millions of payload files. *Rejected:* single
namespace + sniffing (slow recovery), storing recipes only in the DB
(violates D15 — DB is a cache).

## D21 — Recipes are an OR-graph; grounded GC (2026-07-03)

Multiple recipes per output hash are first-class (index many-to-one;
per-recipe verification state; residency planner picks cheapest verified
route). GC computes reconstructibility as a fixpoint grounded in retained
literal bytes — mutually-inverse recipe pairs must never circularly
justify dropping both literals.

## D22 — Aliases: derived cache locally, signed claim objects when shared (2026-07-03)

Alias facts are recomputable, so locally they live as DB rows (snapshotted
for recovery speed only). Shared aliases are signed batched CAS objects
(the D8 mapping table); peer alias claims auto-verify at ingest (full hash
tuple recomputed anyway); trustless verification without bytes is the
waddup ZKP slot. *Rejected:* per-alias micro-blobs (40M+ tiny objects),
authoritative alias storage (it's a pure function of data).

## D23 — Policy layer: config + wasm components, no embedded scripting (2026-07-03)

Recipes contain zero policy; policies (ingest strategy, 1G1R selection,
view layout) are declarative config for the common case plus
`datboi:policy@1` wasm components for the rest, and they *emit* recipes.
*Rejected:* embedded lua/rhai/starlark (a second plugin system with a
second sandbox story that can do nothing wasm can't).

## D24 — Bit-exact rebuilds guaranteed only for canonical formats (2026-07-03)

TorrentZip/RVZSTD and formats we control: rebuild guaranteed by
construction. Wild containers: ingest attempts parameter discovery; on
failure the container stays literal (members still extracted/deduped, no
rebuild recipe minted). *Rejected:* chasing bit-exactness for every
compressor variant ever shipped (unbounded reverse-engineering tax; most
scene zips are already TorrentZip'd).

## D25 — Drop safety: local replay required; zero nondeterminism (2026-07-03)

Literal bytes of X may be dropped only after X's rebuild recipe has
successfully replayed **on this host** (verified-at-creation or
peer-verification is insufficient). The entire drop/evict mechanism must
be fully deterministic. Composes with the D27 opaque-eviction rule and the
D21 grounding fixpoint.

## D26 — Keys remain ordinary blobs, no special handling (2026-07-03)

Challenge raised (legal posture of default-shareable keys) and overruled
by user: datboi does nothing special with keys — no extraction features,
no distribution; they are content like any other blob under the same ACLs.
Public-swarm-era default-advertisement policy can be revisited when public
swarms exist.

## D27 — Residency: keep-both under high-water; seekability-aware eviction (2026-07-03)

Default policy per storage class: literals stay until a high-water mark,
then recipe-covered literals evict (LRU-ish, D21 grounding + D25 replay
rules). Additional rule: **never evict a literal whose cheapest verified
recipe is opaque (non-seekable) while any pinned view snapshot references
it** — eviction cost is reconstruction class, not just recency.
*Rejected:* eager-drop (reconstruction latency cliffs), never-drop
(no storage benefit).

## D28 — At-rest compression: ZFS-delegate locally, seekable zstd in S3/HTTP backends (2026-07-03)

NAS backend stores plain bytes (ZFS zstd below, scrubbed, format stays
boring). S3/HTTP backends apply backend-internal seekable zstd (framed
~256 KiB; frame index alongside; identity and .obao always over plain
bytes). Compression-as-recipe remains available where it wins
independently.

## D29 — dir2dat early (2026-07-03)

"Export collection as dat" ships early: cheap given the claims model,
forces losslessness honesty, and is the p2p library-advertisement
primitive (signed dat of holdings).

## D30 — Auth v1: invites + passwords (2026-07-03)

Admin-minted invite URLs → local accounts (argon2) + session cookies.
Passkeys/OIDC/proxy-header modes are later add-ons. Why: passkeys are
origin-brittle in homelab deployments (IP churn strands credentials);
passwords are understood by everyone. iroh NodeId remains the
daemon↔daemon plane.

## D31 — MAME MVP guardrail (2026-07-03)

MAME-in-MVP means: parse listxml + software lists, audit non-merged sets,
CHD data-sha1 claims. It does NOT mean merge-mode rendering,
device_ref-closure set building, or softlist rebuild fidelity — those are
schema-accommodated (D13) but implemented post-MVP. Tripwire: implementing
loadflag semantics before the first No-Intro audit works = milestone
failure.

## D32 — Serving is userspace-only, cross-platform (2026-07-03)

All serving surfaces run in userspace with no kernel-module dependencies:
in-process NFSv3 as primary mount, HTTP/WebDAV day one, FUSE optional
where present, SMB via sidecar Samba initially. A from-scratch read-only
memory-safe SMB1 server for retro clients (OPL/OG-Xbox) is an accepted
future workstream (narrow, well-documented op subset; safer than enabling
NT1 in Samba).

## D33 — Local serving surfaces auto-flip to new view snapshots (2026-07-03)

When a view re-evaluates (dat update, new ingest), local surfaces switch
atomically to the new snapshot; in-flight reads on the old snapshot stay
valid until quiesced (it remains pinned).

## D34 — P2P sharing: tickets + channels; holdings-first (2026-07-03)

Immutable shares are tickets to snapshot hashes (no refresh semantics).
Mutable shares are signed monotonic channels with pull-based subscribers
(subscriber-side residency policy: metadata-only | on-demand | mirror).
v1 ships holdings channels only (dir2dat inventory, auto-promoted);
curated channels (manual promotion) are a later feature. Peer
availability becomes a completeness state
(`available-from-peer(X)`). *Rejected:* push-based publication (peers own
their storage decisions), auto-promoting curated shares (propagates
curation mistakes into friends' pinned storage).

## D35 — MVP cutline ratified (2026-07-03)

MVP is additive-only (zero eviction; no byte-destroying code path),
CLI-only (localhost daemon, no auth/UI until M4), containers-stay-literal
with members-as-claims (≈1.0× storage), zero load-bearing wasm (builtins
only; wasmtime ships with reference transform + determinism CI gate).
Milestone order M2 (shrink) → M3 (views/serving) → M4 (UI/auth) →
M5 (p2p) → M6+ (frontier). Full definition in roadmap.md. *Rejected:*
early storage wins in MVP (adds drop-adjacent paths to v1), status-page
scope leak, p2p-before-UI reordering.

## D40 — Ingest custody: copy default, move for bulk adoption, no by-reference blobs (2026-07-03)

`ingest --copy` is the default (source untouched); `--move` renames into
the store for collections already on the NAS dataset (zero data
movement, layout intentionally destroyed — loud docs). By-reference
storage is rejected: every blob-index row must be backed by bytes in
`data/` (rebuildability + no mutable-under-us files). The
try-before-custody use case is served by an audit-only mode
(`datboi audit --against <dir>`: hash, claim, report; store nothing but
the rescan cache).

## D36 — Aggregation ratified, lands M2 (2026-07-03)

Content-defined aggregation as designed: aggregate = plain blob = concat
of a complete game/machine's member set sorted by member blake3; members
become affine `assemble` slice recipes; both directions in the OR-graph;
boundary derives from dat revisions (instances converge — no pack-file
combinatorics); incomplete games stay loose; stale aggregates from
revision churn are re-aggregated lazily and GC'd. D19 store format
unchanged (aggregates are ordinary blobs). The M1 NFS benchmark decides
default-on vs opt-in.

## D37 — Two-file DB split (2026-07-03)

`state.db` (authoritative-until-snapshotted, tiny, synchronous=FULL,
real migrations) + `cache.db` (derivable, nukeable, cavalier
migrations). Makes D15 mechanically checkable: sole truth only in
state.db, which must round-trip the snapshot encoder. Accepted:
cross-file consistency is eventual. *Rejected:* single file (doctrine by
convention only).

## D38 — Revision materialization: current + previous (2026-07-03)

Full entry/claim rows for the current and previous revision per source;
older revisions demote to header-only (rows deleted, re-importable on
demand from the CAS dat blob). Bounded growth with out-of-the-box update
diffs. *Rejected:* current-only (every diff re-imports), keep-everything
(unbounded).

## D39 — 'Probable' is a distinct audit state (2026-07-03)

crc32+size-only matches report as `probable`, never folded into
have(claimed). Six states: have-verified / have-claimed / probable /
available-from-peer / missing / unknown. Same honesty principle as the
verified/claimed split; UIs may collapse visually.

## D41 — WIT world frozen at @1: whole-buffer profile (2026-07-06)

`datboi:transform@1.0.0` (transforms/wit/transform.wit) is frozen:
`describe(op) -> descriptor` + `run(op, params, inputs: list<list<u8>>)
-> result<list<list<u8>>, string>`. Whole-buffer by-value blobs; the
world imports NOTHING except its own `types` interface, so ambient
nondeterminism (clock/random/fs) is unrepresentable — the import surface
is the sandbox. Seekability (D27) rides along as `describe` metadata even
though @1 can't stream. A streaming profile is a deliberate future
`@2` world, not a revision: per D7 old worlds stay executable forever,
and which world a component targets is recipe metadata. The determinism
gate (crates/datboi-runtime/tests/determinism.rs) pins the committed
reference component by blake3 plus a golden output hash as the
cross-architecture anchor; updating the fixture is a format event.
*Rejected:* shipping streaming in @1 (host-backed stream resources drag
in wasi:io and its pollables — see D42 — and M1's bounded transforms
don't need it).

## D42 — Transforms build for wasm32-unknown-unknown, not wasip2 (2026-07-06)

Discovered by the determinism PoC before the freeze: Rust's
`wasm32-wasip2` std links WASI shims (wasi:io, wasi:cli, …) into every
component even when unused, so a "pure" transform demands ambient
imports the empty linker must refuse — the D5 contract and the target
were incompatible. Transforms therefore compile to core modules for
`wasm32-unknown-unknown` (std available, zero host imports; panics
become traps) and are componentized with `wasm-tools component new` (no
adapter). Enforced two ways: the runtime's linker is empty, and the gate
test instantiates a WASI-importing component and asserts refusal.
*Rejected:* linking deterministic WASI stubs (weakens
"unrepresentable" to "stubbed", and pulls wasmtime-wasi into the
minimal engine build).

## D43 — Snapshot format: signed envelope + sharded alias batches (2026-07-06)

`datboi/statesnap/1` is an ed25519-signed envelope (signature over
`header || payload`, key + sig embedded; recovery additionally PINS the
key to the local identity — an attacker who can write meta/ can mint
self-consistent snapshots under their own key). Payload: sequence,
created_at, dat-source refs (provider/system/dat-blob/imported_at —
enough to replay `dat import` bit-identically), and references to
`alias_fanout` sharded `datboi/aliases/1` batch blobs. Ratified over
inline aliases: additive-only MVP (D35) never deletes, and an inline
table re-writes ~100 MB per snapshot at MAME scale; sharded batches
(fanout 256, rows strictly sorted by blake3) make snapshot cost
proportional to what changed — unchanged shards dedupe by content
address. Shard *assignment* is encoder policy, not format. Alias rows
cover data/ only: meta objects never appear in dats, and including them
would let every snapshot dirty its own shards. Sequence monotonicity is
authoritative state: `recover` re-seeds the snapshot log from the
snapshot it consumed. Unlike recipes, snapshot identity stability is not
sacred (only the latest matters), but the codec still gets golden-vector
coverage. *Deferred to M2:* a HEAD pointer for total-disk-loss discovery
(M1 recovery scans meta/ and takes the max verified sequence);
snapshot-driven fast recovery that skips the full re-hash pass (recover
still re-hashes everything today, so batches are written but not yet
consumed).

*Rejected:* inline alias table (fat permanent garbage), no aliases in
snapshot (amends D22, makes future fast-recovery impossible), unsigned
snapshots (recovery root must be authenticated).

## D44 — CHD header matches grade as `probable` (2026-07-06)

M1 reads CHD v5 headers only (no decompression): the internal sha1 that
MAME disk claims reference is a *self-attestation* by whatever wrote the
file. Ruled: header matches surface as **probable**, the same bucket as
crc+size-only evidence — audits over disk-bearing sets stay "incomplete"
until a decompressing verify exists (M2 chdman-port component upgrades
matches to have-verified). Mechanism: declared sha1s live in a separate
alias namespace (`AliasAlgo::ChdSha1` — they must never answer real sha1
lookups), and unification links them at `BASIS_DECLARED`, below
crc+size. *Overruled objection (assistant recommended have-claimed):*
treating the embedded chdman attestation as claim-grade would let CHD
sets reach "complete" in M1 and matches what other rom managers do; the
ruling favors strictness — a truncated CHD with an intact header must
not audit as have. Unsupported CHD versions (v1–v4) are stored as opaque
bytes and reported.

## D45 — Ingest is custody; analysis is a refinement fixpoint (2026-07-06)

Ingest = custody + identity (single-pass full alias tuple) + only the
cheap inline structural claims audit needs immediately (container
members, skipper identities). Everything expensive — trial
recompression, ECM, chunking, decrypt derivations — runs as background
refinement sweeps over the corpus. Corollary: "new analyzer ships" and
"keys arrive after the NSPs did" are the same event; the fixpoint
advances. Requires analyzer provenance (which analyzer versions ran on
which blobs) including negative results — extends D24: failed rebuild
discovery is recorded, never silently retried each sweep. *Rejected:*
inline-everything (M2 analyzers crater ingest throughput),
defer-the-seam (ingest crate ossifies around inline assumptions).

## D46 — transform@2 streaming world lands with M2 (2026-07-06)

Amends D41's expectation that streaming is far-future. M2's headline is
container recipes for disc-era content, and those replays are
unbounded: a single-member Redump zip is ~4 GB of DEFLATE that D25
requires replaying locally before the literal drops — whole-buffer @1
means ~8 GB of guest memory per replay, and deflate can't be chunked
without breaking bit-exactness. So the streaming world is designed
alongside M2, not deferred. Binding constraint carried from D41/D42:
streams are resources in `datboi:transform@2`'s own `types` interface,
host-implemented — NOT wasi:io/pollables — so the empty-linker "import
surface is the sandbox" property survives, and the determinism gate
extends to @2. @1 stays frozen and executable forever (D7/D41); the
target world remains recipe metadata. *Rejected:* RAM cap with
containers-stay-literal above it (guts M2's shrink win exactly where
the bytes are — disc imagery), per-member framing (doesn't bound
single-member containers).

## D47 — Claims are dat-blind; scheduling may be dat-aware (2026-07-06)

Hard rule: catalog contents never influence *what* gets claimed —
claims are facts about bytes, and instances holding the same bytes must
converge on the same claim set (p2p claim sharing, reproducibility).
The refinement scheduler MAY consult dats to order work
(complete-a-set-first). M1's dat-blind ingest is thereby ratified as
principle, not accident. *Rejected:* dat-aware analysis (claim sets
become a function of which dats happen to be loaded; cross-instance
convergence frays), fully-blind scheduling (queue burns days on
unmatched junk before touching near-complete sets).

## D48 — Analysis provenance: cache rows + snapshot batches (2026-07-06)

Analyzer provenance and negative results are pure functions of
bytes × analyzer hash → cache.db rows (D37), batched into signed
snapshots alias-style (D22/D43 precedent; own sharded batch type) so
bare-NAS recovery doesn't re-pay expensive negatives — trial
recompression across a MAME-scale corpus is days of CPU. *Rejected:*
authoritative state.db rows (derivable data erodes the D37 boundary
that makes the doctrine checkable), cache-only (doctrine-pure, but the
first real recovery pays the full re-analysis bill).

## D49 — Seekable-route verification: output bao, mandatory, forever (2026-07-06)

Claim-level verification (one full materialization + tee, D4/D25) never
exercises a component's *seek* path — sequential and seeked replay are
different code, and boundary off-by-ones live exactly where a
start-to-finish check can't see them. This isn't only adversarial
(lying peer recipes); our own and community wasm will have seek-point /
window-arithmetic bugs. Three rules, all corollaries of accepted
machinery:

1. **Outboards survive eviction.** Dropping a literal (D25/D27) deletes
   bytes, never the `.obao` — the tree is what makes every future
   recipe-backed range read verifiable without rematerialization. D25
   guarantees it exists at drop time (the licensing replay is a full
   materialization). Outboards are self-authenticating against the
   root, so peer-supplied outboards need no trust machinery.
2. **Recipe-served range reads always verify against the *output*
   outboard** — mandatory for seekable-transform routes, tightening
   D4's "cheap default" stance for this class. Input-side bao proves
   sources honest, not segment maps or seek arithmetic; derived reads
   face rot + claims + unverified seek code, and get *stronger*
   per-read checks than literals, not weaker. Mismatch ⇒ EIO to the
   serving surface, never bad bytes.
3. **Seek-path mismatch on a verified recipe is its own failure class**
   — not "claim false" (sequential replay proves the claim), not D-late
   nondeterminism. Response: quarantine *seekability* for the
   implicated component hash; planner reclassifies its recipes as
   opaque so the spill rule serves reads through the known-good
   sequential path. Fix ships as a new component hash / new recipes.
   Literal re-pinning only if sequential replay also fails.

Companion (rides with D46's @2 work): the conformance gate gains a
seek-equivalence property test — random range reads over
declared-seekable components must equal slices of a full
materialization, with ranges placed at ±1 of every declared boundary.
*Rejected:* input-side verification for affine routes (blind to lying
segment maps), verify-optional streamed reads for derived outputs
(leaves the never-verified seek path in the serving hot path).

## D50 — M2 split: engine platform before shrink features (2026-07-06)

D45–D49 grew M2 into four workstreams with an internal serialization:
the shrink (planner/eviction/aggregation) depends on the streaming
engine (D46 — the byte win is disc-era, and D25 replay of a 4 GB
DEFLATE member needs @2), and rebuild discovery depends on the
refinement fixpoint (D45/D48 — trial recompression without provenance
re-burns days of CPU per sweep). One "milestone" would have meant
months of platform work with no user-visible win. Ratified split:

- **M2 — "The engine streams"** (platform): transform@2 design+freeze,
  streaming executor + spill, bao outboard machinery + mandatory
  output-bao verify on seekable routes + seek-quarantine (D49),
  determinism/seek-equivalence gates, fixpoint skeleton (sweep queue,
  analyzer provenance incl. negatives, provenance snapshot batches).
  Exit: ~4 GB member replays bounded-memory verified (sequential and
  seeked); a no-op analyzer sweep survives the recovery drill.
- **M3 — "The NAS gets smaller"** (features): analyzers in anger
  (TorrentZip/wild-zip discovery, ECM, 7z/rar), residency planner +
  eviction, aggregation (NFS-bench-gated), FastCDC chunking.

Downstream milestones shift one: views M4, API/UI M5, p2p M6,
frontier M7+. **Numbering note**: decision entries D1–D49 predate the
split — read their "M2 (shrink)" as M3, "M3 (views)" as M4, and so on;
historical entries are records and are not rewritten.

*Rejected:* cart-era-only shrink first (small byte win — carts are
small, aggregation wins file-count not bytes — and the eviction path
would ship twice: once @1-only, once streaming), keeping the fat M2
(a platform milestone wearing a feature milestone's name).

## D51 — transform@2 interaction model: guest pulls, guest pushes (2026-07-06)

The streaming world's shape, ruled after adversarial review:

1. **Pull-in / push-out**: `run` executes to completion, calling
   `source.read` / `file.read-at` and `sink.write` on host-implemented
   resources. Chosen over a host-driven `update/finish` pump because
   multi-input transforms (zip rebuild: skeleton + N members) must
   decide which input they need next — a pump can't know. Cost
   accepted: composing two streaming guests in one operator tree needs
   host-side fiber suspension (executor work, still ahead).
2. **Exact stream contract** (the determinism linchpin): `read(n)`
   returns exactly `n` bytes, short only at end-of-stream; `write`
   accepts every chunk unconditionally. Anything weaker lets the
   guest-visible byte sequence depend on host buffering, and outputs
   could legally vary. Enforced host-side; the reference guest carries
   a `read-contract-probe` op that verifies it from inside the sandbox.
3. **`serve-range` ships in @2** (not deferred to @3): the range path
   for non-opaque transforms, property-tested for seek-equivalence
   (D49) with boundary-straddling ranges. All serve-range inputs are
   random-access by contract.
4. **`MAX_READ` guard** (16 MiB per read): oversized reads trap
   deterministically — the resource-abuse guard that doesn't break the
   exact-read contract with a clamp.
5. **Compile/run split in the host API**: components compile once
   (`load`) and instantiate per run (~µs) — the executor replays
   thousands of recipes against a handful of pinned components.

Status: **FROZEN 2026-07-07.** The streaming executor landed
(datboi-exec: operator trees, spill rule, threads+pipes composition per
item 1's accepted cost) and the M2 exit test passed at full size: a
3.9 GiB zip member (zip32 ceiling; zip64 is deliberately out of M1
ingest scope) replayed in bounded memory (<512 MiB peak RSS asserted
via VmHWM), hash-verified with the bao outboard built in the same pass,
then served by seeked recipe-route range reads under mandatory
output-bao verification (D49). The runtime gate stays green (13 tests);
the reference guest additionally carries `byteswap-lying-range`, a
planted seek-path bug that the D49 quarantine machinery is
integration-tested against. As with @1, the fixture hash is pinned and
updating it is now a format event.

*Rejected:* host-driven update/finish (single-implicit-input shape),
"read up to n" semantics (nondeterminism by buffering), wasi:io streams
(ambient surface, D46).

## D52 — Outboard sidecar format: headerless pre-order obao4 (2026-07-07)

The `.obao` sidecar is the pre-order bao outboard over 16 KiB chunk
groups (`BlockSize(4)`), hash pairs only — no header, no size prefix
(blob length comes from the data file or the index). Byte-identical to
what iroh-blobs writes, so the M6 p2p layer serves our sidecars
unchanged (D2/D14 alignment); the tree root IS the blob's blake3, so
sidecars are self-authenticating and peer-supplied ones need no trust
machinery (D49). Implementation rides the `bao-tree` crate (n0's, the
same code iroh uses); a golden-vector test pins the encoding — this is
an at-rest format commitment on the same tier as the store layout.
Small blobs (≤ one chunk group) have an empty outboard by construction:
no sidecar file exists below 16 KiB, and absence-of-file is the
canonical encoding of "empty". *Rejected:* post-order layout (writes
stream nicely but iroh can't serve it), the original bao crate's 1 KiB
tree (4× sidecar bytes for verification granularity nothing needs),
inline-in-DB outboards (violates D15 — the tree must survive DB loss
with the bytes it protects).

## D53 — Wild-zip rebuild rides preflate splitting, streaming @2 (2026-07-07)

Ruled after the preflate spike. The deflate-rebuild path is
**preflate**, not compressor matching: `preflate-rs` 0.7.6 (Microsoft,
Apache-2.0, pure Rust) reconstructs a deflate stream bit-exactly from
its plaintext plus a small corrections blob — no compressor
identification, no level search. Spike evidence: compiles for
wasm32-unknown-unknown with ZERO imports (D42 empty-linker holds; 293
KiB core module, componentizes, and runs under wasmtime with
native-identical corrections output); TorrentZip-faithful (zlib -9)
streams reconstruct 100% bit-exact, corrections ≈0.002% of plaintext at
20 MiB with a ~0.5 KiB fixed floor per stream (irrelevant at CAS
granularity — corrections are ordinary blobs); Info-ZIP works at every
level. Deps (bitcode, cabac, byteorder, default-boxed, deranged) are
pure Rust; version churn cannot break old recipes because the component
hash is pinned in the recipe (D5 by construction).

`xf-preflate` targets the **@2 streaming world** (members are big;
`RecreateStreamProcessor` carries only the 32 KiB dictionary between
chunks, so memory is bounded). Recipe shape: per-member `recreate` —
inputs corrections `{role: skeleton}` + member plaintext → the member's
raw deflate stream, **opaque** seek class; the container is an ordinary
`assemble@1` over literal zip-structure segments + rebuilt streams, so
range serving of the *container* still works through assemble's affine
math (materializing only the members a range touches).

**Coverage gap, accepted as an optimization issue**: preflate-rs 0.7.6
hard-errors (`NoCompressionCandidates`, complevel_estimator's fixed
4096-chain ceiling) on streams whose match-finding fits none of its
modeled compressors — reproduced deterministically with 7-Zip's deflate
encoder at every level; one real firmware zip failed on 3 of 7 members.
The failure is a clean error, so the analyzer records a D48 negative
and the container stays literal (the D24 tax persists exactly there).
Tracked in open-questions.md; TorrentZip — the curated standard — is
zlib and fully covered. *Rejected:* zlib-exact compressor components
(zlib-rs has had output-determinism bugs; zlib-ng guarantees
reproducibility only within one identical build), miniz trial
recompression as the primary path (near-zero hit rate on scene zips;
subsumed by preflate).

## D54 — Component attribution: stamped at build, enforced at load; one crate = one lockfile (2026-07-07)

Two rulings in one format event (the pre-corpus window where hash churn
is free). **Attribution**: every component carries its identity IN-BAND
as execution-inert custom sections — name, description, authors,
license, source URL, and a content-scoped revision — stamped by the
flake's install phase (`wasm-tools metadata add`), and the hosts REFUSE
to load a component missing the minimal set {name, description, source,
revision}: an anonymous func is opaque and hard to reason about, and a
pinned hash must always be traceable to what it is and where it came
from. The `revision` is the GIT TREE HASH of the crate source
(`tree:…`, computed in-derivation with `git write-tree` — no .git
needed), NOT a commit rev: content-scoped, so unrelated repo commits
cannot churn component bytes, and — unlike a nix store hash —
verifiable by anyone with git alone:
`git rev-parse <commit>:transforms/<crate>` equals the stamp for every
commit where the crate is unchanged. **Isolation**: each transform is a standalone
cargo workspace with its own lockfile, built as its own nix derivation
from exactly {crate dir + frozen ../wit} — ruled after observing a
sibling's bytes shift through shared dependency resolution (adding
xf-preflate re-ordered function indices in xf-reference-stream via a
lockfile `syn` disambiguation). The reproducibility boundary of a
component is now one directory. Enforcement lives in
`datboi-runtime::attribution` (hand-rolled ~60-line section walk — the
required fields are four known custom sections; no wasm-metadata
dependency). All four dist/fixture components re-minted and re-pinned;
the pre-D54 reference-stream build is kept as `unstamped.wasm` for the
refusal gate. *Rejected:* commit-rev stamping (per-commit churn breaks
reproduce-from-any-commit), nix-store-hash stamping (opaque and
recomputable only with nix — the first cut of this ruling, replaced
same-day), warning instead of refusing (a warning is
policy nobody reads; the corpus lives forever), one shared workspace
with canonical-at-mint bytes (tolerable but makes "reproducible"
mean "from one blessed commit only").

## D55 — Identity is the exact component hash; coverage inherits by declared lineage; migration is explicit (2026-07-10)

Provenance and analyzer coverage key on the EXACT component hash —
never on stamped name/version. D54 stamps are read only to enforce
presence at load; nothing ever infers "same analyzer" from a label
(a label is self-declared and unverifiable — a dirty build lies
silently). A new component revision invalidates NOTHING: at
registration the binary declares the revision's predecessor hashes
(label-guided, but a policy statement, not an engine inference);
blobs covered by a declared predecessor count as covered
(grandfathered) by default; running the new revision over the old
corpus is an EXPLICIT migration (background sweep queue, dat-aware
ordering) — never automatic. Consequences: deploys are free (no
re-sweep tax, no version-string trust); the D53-era framing
"deferred analyzers re-cover the corpus structurally free" becomes
"re-cover is one explicit command"; version-bump discipline is
hygiene, not load-bearing (D54's tree-hash revision already scopes
identity to content). Native analyzers keep self-declared
`datboi-analyzer:<name>/<version>` tags — no component hash exists
for binary-embedded code; accepted asymmetry that shrinks as
analyzers become components (D58). *Rejected:* coverage keyed on
stamped family+version (auto re-sweep on version bump; trusts an
unverifiable label), coverage keyed on raw hash with mandatory
backfill (full-corpus re-analysis per deploy). *Amended by D65:
predecessor declarations and grandfathered coverage are dropped
(never implemented); exact-hash identity, append-only facts, and
explicit migration stand. See D64 for the principle that vetoed the
lineage machinery.*

## D56 — M4 serving defaults ratified (2026-07-10)

Three of the four builder defaults from the 07-09 session stand (the
fourth, 1G1R, is D57): (1) **materialize-on-demand** for opaque long
streams — one verified replay into the store (evictable again later)
instead of O(n²) re-spill-per-window; follow-up owed: a disk-headroom
guard before materializing; the residency planner's
materialize-at-snapshot-activation remains the systematic successor.
(2) **Bind policy**: 127.0.0.1:2352 default; any other bind is an
explicit flag with a loud no-auth warning until M5 auth — real LAN
deployments run in warning mode deliberately. (3) **DAV reads**:
1 MiB serve_range calls with per-read route planning,
default-until-profiled. *Rejected:* spill-per-window (makes large
opaque blobs effectively unservable), preemptive route-handle caching
(invalidation complexity before evidence it matters).

## D57 — 1G1R is a per-view mode: {held-first, strict}, default held-first (2026-07-10)

Both scoring modes exist per-view. **held-first** (default): a
held-and-verified clone outranks the preferred-but-absent region;
re-eval upgrades picks as holdings improve. Right for the serving
NAS — the Japan copy beats no copy — and converges to strict as the
collection completes. **strict** (retool semantics): selection is a
pure function of (dat, preferences), independent of holdings; empty
slots render as absent. Strict is the designated mode for M6 curation
distribution (a published view must be recomputable from public
inputs — held-first bakes the curator's collection accidents into the
selection) and for gap-fill want-lists (a strict view's missing slots
ARE the fetch list). Retool clonelists ride as an additive
family/region input (D16 acquisition pattern: auto-fetch + manual
drop), improving family construction in both modes; dat cloneof and
base-name inference stay the fallbacks. *Rejected:* held-first-only
(retrofits publication semantics later), strict-only
(consumer-hostile on incomplete collections).

## D58 — unrar goes to wasm: extractor components; the C-to-wasm lane pulls forward from M7 (2026-07-10)

Census (2026-07-10): unrar_sys — 83 vendored C++ files — is the ONLY
memory-unsafe code parsing wild bytes; every other wild-byte parser
is pure Rust (preflate-rs 2 unsafe, sevenz-rust2 3, lzma-rust2 16,
miniz_oxide 4, fastcdc 0; libbz2-rs-sys is the Trifecta Tech
pure-Rust bzip2 rewrite despite the name). Ruling: native Rust
analyzers are acceptable permanently (the "moderately safe" bar); the
one C++ parser moves INSIDE the sandbox. unrar compiles via wasi-sdk
into an **extractor component** (new world: seekable archive stream
in → member streams + metadata out), with guest-side C++ glue driving
unrar's own dll.cpp API; the unrar/unrar_sys crates drop from the
tree entirely. Consequences: the C-to-wasm toolchain lane (planned
for M7's 7-Zip SDK / CHD / RVZ work) lands now with the simplest
possible pathfinder (one-way decode); extraction becomes
deterministic-by-construction, so rar members can carry DERIVE
RECIPES (container→member through the component) and become evictable
— "permanently literal" was only ever about the rebuild direction;
wasmtime's memory cap turns RAR5 big-dictionary bombs into clean
refusals. Build plan: RAR_SMP off; ErrHandler→trap (archive fails
whole, matching the refuse-suspicious-archives posture); File-class
reroute onto stream imports preferred — a deterministic-WASI-shim
fallback would amend D46's empty-linker posture and RETURNS AS A
RULING if freestanding proves impractical. v1 scope cuts: no
encrypted archives, no multi-volume (VolumeCall), links/NTFS streams
ignored. Naming (ruled 2026-07-10): component prefix encodes the WIT
world — `xf-` = transform@2, `ex-` = extractor — so this lands as
`transforms/ex-unrar`; build/stamp/gate globs widen from `xf-*` to
both prefixes. Guest shape (spike decides, not doctrine — component
hash, world, and tree location are identical either way): the
component is ~30k lines of C++ plus a thin interface layer, and the
layer has two viable forms. Preferred: **thin Rust guest crate over a
C++ staticlib** — unrar's dll.hpp API is already extern "C", so the
guest is wit-bindgen rust for the world (pleasant resource bindings,
uniform with xf- siblings' pipeline) + unsafe FFI into the dll API +
a callback trampoline, with build.rs cross-compiling the vendored
C++ via wasi-sdk (the unrar-rs pattern relocated inside the guest).
Fallback if the build.rs sysroot/libc++ wrangling turns hostile:
**pure C++, no cargo** — wit-bindgen's C generator + clang++ +
`wasm-tools component new`; dead-simple build, worse interface
ergonomics (hand-managed canonical-ABI resource handles). RarVM note: modern unrar already amputated the
bytecode interpreter (rarvm.cpp is ExecuteStandardFilter only —
embedded RAR3 VM programs are signature-matched to the seven standard
filters or not executed at all, failing CRC → archive refused whole,
matching our posture); RAR5 dropped the VM entirely. The historical
#1 unrar exploit surface is thus already gone upstream, and what
remains runs under wasmtime fuel/epoch bounds — containment native
unrar cannot offer. Standard filters are pure functions, so derive
recipes through the extractor stay deterministic. *Rejected:*
subprocess jail (Landlock/seccomp — cheaper but keeps C++ outside the
model and buys no derive routes), accept-in-process (wild archives,
daemon privileges, real CVE history), dropping rar ingest.

## D59 — Chunking eligibility: route-less literals only (2026-07-10)

ChunkAnalyzer eligibility narrows from "every data blob ≥ 4 MiB" to
"literal blobs ≥ 4 MiB with NO existing covering route" (threshold
unchanged). Chunking's job is making big route-less literals
evictable via cross-image dedup; routed blobs are already evictable,
and identical content already dedups at the blob level. Containers
remain eligible — they are literals, and that is where archive-corpus
dedup actually lives. *Rejected:* chunk-everything (sweep I/O +
recipe metadata for no marginal dedup).

## D60 — Ingest-policy config: the minimal shape (2026-07-10)

The D45-era "molten" config surface freezes at its minimal shape now
that four analyzers exist to generalize from: per-analyzer
**enable/disable** + **analyzer-owned opaque params** in the state.db
config KV (rides the statesnap via the 07-09 payload keys), lineage
declared at registration (D55), and sweep ordering stays a single
global dat-aware policy — no per-analyzer ordering knobs.
Deliberately NOT designed (no consumer exists): detector-registry
confidence ordering, canonical-orientation preference. *Rejected:*
designing the full vocabulary now (speculative config calcifies).

## D61 — `scrub --rehabilitate`: an operator path out of Failed (2026-07-10)

Failed stays terminal for the SYSTEM, but the operator gets an
explicit door: `scrub --rehabilitate` re-replays Failed recipes with
full verification; success clears the state and records a
rehabilitation event in provenance; failure returns to Failed
(self-limiting). Motivated by the pipe-race incident: a host bug
wrongly poisoned a recipe and no un-poison path existed — a false
verdict was as permanent as a true one. *Rejected:* purity
(terminal-means-terminal — falsified by the incident),
auto-rehabilitation (flapping must never mask corruption).

## D62 — Reified views ratified: images are assemble recipes (M4 scope: read-only FAT32) (2026-07-10)

The views model is ratified as scoped: a reified image is a plain
`assemble@1` recipe — skeleton blobs (boot sector, FATs, directory
clusters) + windowed segments over content blobs (cluster-aligned) +
fill for slack — minted by filesystem-layout math running in the
policy tier at view-eval time (D23: policies emit recipes). Image
params pin identity: volume serial derived from the snapshot hash,
fixed timestamps, deterministic ordering. Skeleton correctness is a
MINTING property no runtime verification can catch (a wrong FAT chain
serves faithfully-wrong bytes), so **fsck-in-CI is mandatory**: parse
the synthesized image and diff its tree against the view manifest,
same rank as the golden tests. M4 scope is READ-ONLY synthesis;
writable overlays ("writes are ingests", per-device overlay, save
history for free) + dirty-image diff-back are pended to a design pass
before nbd/live-write serving; until then, image-mode sync documents
that REFLASHING CLOBBERS ON-DEVICE SAVES. *Rejected:* imperative
image builder (an unmanaged artifact — no dedup/verify/evict, same
layout math anyway), overlays-in-M4 (unproven design on the
milestone's critical path).

## D63 — D49 amendment: the affine carve-out (2026-07-10)

Routes that are **locally-minted + pure-builtin (assemble/slice/fill)
+ affine-only + over verified inputs** may serve ranges WITHOUT an
output bao: every served byte is either verified input bytes
(windowed segments carry input-side bao; small skeleton blobs are
fully hash-verified) or executor-generated fill. D49's threat was
seekable TRANSFORM CODE whose seek path diverges from its sequential
path — not the executor's own affine arithmetic, which is the same
trust as the read path and the hash computation themselves. The
carve-out trades D49's runtime check for test-time coverage of that
arithmetic: the seek-equivalence property gate (random ranges ≡
slices of full materialization) extends to synthesized assemble
recipes. The predicate lives IN CODE, tight: wasm components never
qualify (xf-ecm's manifest-seekable serving stays full D49); nothing
computed qualifies. An optional background **blessing pass**
(materialize-to-null, tee, cache the obao4) promotes a carved-out
route to full D49 when residency allows — the carve-out is a floor,
not a ceiling. This unblocks never-fully-materialized giant images
(nbd-served OPL disks, TB-scale FAT32 exports). *Rejected:* universal
D49 (giant reified images unservable), mandatory blessing (one full
pass over TB-scale images for no additional served-byte guarantee).

## D64 — Forward compatibility is the point: core and components evolve independently (2026-07-10)

The unstated thesis behind D5/D6, ruled now because it just vetoed
machinery (D65): the component population and the core binary are
INDEPENDENT axes of evolution. Future analyzers, transforms, and
extractors arrive as components — from our own repo or from peers
(D6: peer code is wasm, never native) — and run under an existing
core without a core update. "Latest" is not a privileged concept
anywhere in the system: recipes pin exact component hashes, so new
components can never break old recipes (D5 by construction) and old
components' facts are never invalidated by new arrivals (D55/D65).
REPLAY of a peer recipe with a peer component requires no trust
decision at all — sandboxed, deterministic, fuel-limited, output
hash-verified (D5/D6); the construction is trustless. The only
trust decision is what runs over YOUR corpus to produce facts:
what you deployed, or what you explicitly directed (D65) — never
anything inherited from a publisher's claim of version, ancestry,
or recency, which are labels and therefore unverifiable (D54/D55
energy). Litmus test: any design that assumes a single operator
linearly ordering component revisions is wrong-shaped and gets
rejected on sight — the component population is unordered; a node
runs its deployed slice of it and replays anything.
*Rejected:* leaving this emergent from D5/D6 without a ruling (it
silently contradicted D55's registration lineage until challenged).

## D65 — D55 amendment: no lineage — the deploy is the policy; disagreement is surfaced (2026-07-10)

D55's core stands: identity is the exact component hash, labels are
never trusted, analysis rows are append-only facts and are never
invalidated. The middle DIES, unimplemented: predecessor
declarations and grandfathered coverage are dropped. The "re-sweep
tax" that motivated grandfathering conflated eager-and-blocking with
background: re-covering is opportunistic idle-time sweep work (the
pending-sweep table's existing shape) or a manual directive, so
deploys still block on nothing and the corpus converges to genuinely
FRESH coverage instead of inherited claims — the failure mode where
a bugfixed analyzer silently trusts its buggy predecessor's rows
structurally cannot happen, because nothing inherits. Lineage was
also wrong-shaped for D64: peer-arriving components have no linear
order, and inheriting coverage across a publisher's ancestry claim
is the trust-an-unverifiable-label failure D55 rejected, one level
up. The replacement is smaller: (1) **the deploy is the policy** —
datboi runs the components it SHIPPED with. They are seeded into
the CAS (ingest already does this) and referenced by hash in
recipes and facts, so a recipe that travels p2p carries its
component as an ordinary blob under ordinary ACLs. The sweep
target is "blobs missing a row for a shipped analyzer hash" (× the
D60 per-analyzer enable); anything beyond the shipped slice — e.g.
a peer-published analyzer — runs by EXPLICIT DIRECTIVE and
produces ordinary per-hash facts. No registration, no adoption
list. Superseded components stop chasing new blobs by no longer
shipping; their rows stay forever (dozens of analyzer hashes are
dozens of CAS blobs plus cheap index rows — nobody cares). (2) a
**conflict rule** — rows from different hashes may disagree about
the same bytes; both are facts. Reports and gates prefer the
shipped hash's row; a contradiction between rows is a surfaced
anomaly, never silently resolved (D39 energy: disagreement is
signal; distinct states don't collapse). Native analyzers'
self-declared tags remain the accepted asymmetry (D55/D58).
*Rejected:* predecessor-declaration registration (a trust
statement dressed as metadata; assumes operator-ordered linear
revisions, wrong-shaped per D64), grandfathered coverage (fails
cheap and quiet — inherited green until someone remembers to
migrate), newest-wins conflict resolution (no "newest" without
lineage, and disagreement is worth seeing), a standing mutable
"active set" registry (first cut of this amendment, replaced
same-day: a config surface with no consumer — the deploy already
is the policy, and if per-hash selection ever needs config, D60's
per-analyzer enable is its ruled home).

## D66 — Single binary: components embed at build, nix-built, never hand-copied; dist/ dies (2026-07-10)

Datboi is ONE BINARY (D10/D14 ethos; M5 web assets will embed the
same way). The shipped component slice embeds via `include_bytes!`
— but the bytes come from the NIX-BUILT transform derivations
(build.rs reads `DATBOI_COMPONENTS_DIR`, set by the flake and the
dev shell), never from a hand-copied checked-in artifact: the
committed `transforms/dist/` and its rebuild-and-re-copy step are
DELETED. Dependent rebuild falls out — transform source change →
derivation → host rebuild with fresh bytes; the D65 seeding path
(embedded components published into CAS at startup, recipes pin
hashes) is unchanged, and replay loads components by hash from CAS,
so embedding is packaging, not capability (D64 intact: peer/newer
components run under an old core as recipe replay). Layout rulings
in the same breath: transform crates move to
`crates/datboi-xf-*` / `crates/datboi-ex-*` (standalone workspaces
with their own lockfiles — the lockfile boundary, not the
directory, is what keeps sibling changes from churning component
bytes, D54); the WIT tree moves to `./wit`; stamped names stay
`datboi:xf-*` / `datboi:ex-*`. Accepted trade, eyes open: a commit
no longer carries the exact component bytes it shipped —
reproducing a historical artifact needs nix + that commit's
flake.lock; SOURCE traceability stays git-only via the D54
tree-hash stamp, and identity was never the artifact's location
(D55: the hash in the recipe). Small blessed fixtures (the refusal
gate's `unstamped.wasm`, determinism-gate pins) remain in git —
they are test vectors, not deploy artifacts. *Rejected:* committed
dist/ + staleness check (drift-prone hand step that a build
dependency does better; its one virtue — git-only artifact
reproduction — is the accepted trade above), components as a
deploy-time payload directory (a second distribution artifact
contradicting single-binary for no D64 gain).

*Amendment (2026-07-13):* the WIT tree adopts package-named
directories — `wit/transform/v1`, `wit/transform/v2`,
`wit/extractor/v1` — replacing the positional `wit/v1`/`v2`/`ex1`
(the D88 rule applied to this tree: names cite, and `ex1` encoded
"extractor@1" only by convention). Repo paths only; the worlds'
contents stay frozen (D51), and one-package-per-directory is the
layout `wkg`/wit-deps tooling expects if the WIT is ever published
for external component authors.

## D67 — M5 web stack: Svelte 5 + Vite in web/, wuchale i18n, dist embeds like D66 (2026-07-11)

The web UI (D17) lives in `web/` as a standalone npm project with its
own `package-lock.json` — the lockfile boundary again (D54/D66):
`web/` is NOT part of the host cargo source set, and the flake builds
it as its own derivation whose source is a `lib.fileset` over `web/`
alone, so rust edits never invalidate the web build and web edits
never invalidate `cargoArtifacts`. Build pattern is rof-gui's
(importNpmLock, no vendored-hash churn: `importNpmLock.buildNodeModules`
+ a `mkDerivation` running `vite build`), modernized where nixpkgs
allows. The built dist embeds into the datboi binary exactly the way
components do (D66): the flake sets `DATBOI_WEB_DIST` on the final
build/test/clippy args (not `buildDepsOnly`), a
`crates/datboi-server/build.rs` re-exports it with the same
dev-checkout fallback (`nix build .#web --print-out-paths`, with
rerun-if-changed watches on `web/`), and the server serves the
embedded tree at `/` with an SPA fallback to `index.html` and
immutable caching on Vite's content-hashed assets. Existing surfaces
(`/view`, `/snap`, `/dav`, `/v1`) are untouched; the old plaintext
root listing dies — its content moves into the UI and stays available
as `/v1` JSON.

i18n is FIRST-CLASS from the first commit: every user-facing string
flows through **wuchale** (compile-time gettext-style catalogs,
Svelte-5-native vite plugin), and strings whose English collides
across meanings carry an explicit disambiguation context at the call
site (`@wc-context`, real msgctxt in the PO catalog) — "claimed"
(storage state, not a person's claim), "verified" (hash-checked, not
human-approved), "view" (compiled shelf, not UI view) and friends are
contexts, not comments. English is the source catalog and ships
compiled; adding a locale is adding a PO file. wuchale is pre-1.0 —
accepted eyes-open (catalogs are standard PO; the escape hatch to any
gettext toolchain is the format itself), flagged in open-questions.

*Rejected:* React/Solid (D17 stands); Paraglide (no per-string
translator context in its message format — disambiguation only by key
naming); Lingui (first-class context but no first-party Svelte
extraction; the community bridge is a slow-moving single-maintainer
package); committed `web/dist/` (same drift argument that killed
`transforms/dist/` in D66); rust-embed (include_dir is smaller and
takes the env-var path directly).

## D68 — Auth v1 enforcement: sessions for browsers, tokens for tools, loopback stays owner (2026-07-11)

Implements D30 with these rulings. Identities: `user` rows with
argon2id password hashes; `role ∈ {owner, friend}`. Bootstrap and
minting stay in the CLI (`datboi user invite [--owner]` prints a
one-time invite URL; local shell access = admin, so the CLI needs no
auth). Invites carry the role (state.db migration adds the column),
expire (default 7 d), and are single-use; the browser accepts the
invite by choosing username + password. Tokens (invite, session,
bearer) are 32 random bytes, URL-safe; the DB stores only
`blake3(token)` — a stolen state.db mints nothing. Browser sessions
are the `datboi_session` cookie (HttpOnly, SameSite=Lax, Path=/,
30 d); non-browser clients send the same token as
`Authorization: Bearer`, minted by `datboi token`.

Enforcement: **loopback connections are implicitly owner** — the
existing CLI, tests, and single-user workflows keep working with zero
ceremony, and a local shell already owns the daemon's files, so
cookie-auth on 127.0.0.1 would be theater. Non-loopback: `/healthz`,
the static UI, and the auth endpoints are open; everything else
requires a valid session/bearer. ACLs are a `view_grant (user_id,
view_name)` state table: owners see everything; friends see exactly
their granted views (list, browse, download — the friend surface).
WebDAV and NFS remain loopback-only-by-default serving surfaces in
M5; authenticated DAV (basic auth against bearer tokens) is recorded
as an open question rather than half-shipped. The non-loopback
no-auth warning from M4 dies; binding wide now means "auth required",
not "everyone is owner".

*Rejected:* first-registered-user-becomes-owner (magic; an explicit
`--owner` flag on the mint is one word); passkeys/OIDC now (D30
already deferred them); storing raw tokens (hash costs nothing);
per-entry ACLs (views are the sharing unit — D33's snapshots are what
friends consume); loopback requiring auth (breaks every existing
workflow to defend against an attacker who already has the disk).

## D69 — API contract: typed rust-first, OpenAPI emitted, TS generated; derive rule scoped to identity bytes (2026-07-11)

The no-serde-derive rule exists because CAS object encodings ARE
identities (D18) — a macro must never own load-bearing bytes. The
HTTP API is not that: it's a versioned, negotiable surface. Ruled:
the derive ban is SCOPED to canonical/content-addressed encodings;
the API boundary gets real types. A new host crate `datboi-api`
owns a typed struct for every /v1 request and response (serde +
utoipa derives live in this crate and nowhere else). Handlers stop
building `json!` literals and consume/produce these types; the CLI's
daemon-facing calls use the same structs. The crate emits OpenAPI
3.1; the spec is CHECKED IN and a test regenerates + compares it
(stale spec = red suite). The web build generates TS from the
checked-in spec (openapi-typescript, prebuild like wuchale's
loaders) — hand-written `types.ts` dies. One artifact, three
consumers, all mechanically pinned.

Why a checked-in spec when D66 killed checked-in dist: the D66
artifact was compiled bytes regenerated by a hand step; this is a
reviewable text file regenerated by the test suite you cannot skip,
and the alternative — web deriving from a rust-built derivation —
recouples the D67 cache boundary (any workspace edit → new spec
drv → web rebuild). The spec file is the deliberate, diff-visible
seam between the two build graphs. *Rejected:* spec-first YAML
(user call: rust owns the shapes; hand-maintained YAML is a second
place to be wrong); ts-rs/specta (types without operations);
validating `json!` output against a schema (keeps handlers
stringly-typed — the point is to kill arbitrary payloads, not
audit them).

## D70 — Browser hardening: strict CSP + Fetch-Metadata CSRF (no tokens) (2026-07-11)

All non-API responses (the embedded UI) and API responses carry a
strict CSP: `default-src 'self'`, `script-src 'self'`,
`style-src 'self' 'unsafe-inline'` (inline style *attributes* drive
bar widths/band colors; Svelte's compiled CSS is external),
`img-src 'self' data:`, `font-src 'self'`, `connect-src 'self'`,
`frame-ancestors 'none'`, `base-uri 'none'`, `form-action 'self'`;
plus `X-Content-Type-Options: nosniff`,
`Referrer-Policy: no-referrer`,
`Cross-Origin-Opener-Policy: same-origin`, and
`Cross-Origin-Resource-Policy: same-origin`. No HSTS (plain-HTTP
LAN is the deployment). No `__Host-` cookie prefix (requires
Secure; same reason). *Amended by D76:* the `'unsafe-inline'`
premise was wrong (Svelte `style:` directives are CSSOM writes,
which style-src does not govern) — dropped, plus additional
headers and API cache hygiene.

CSRF: token-less, header-based — the Fetch-Metadata design Go
ships as `http.CrossOriginProtection` (Valsorda). Middleware
rejects state-changing methods (non-GET/HEAD/OPTIONS) when
`Sec-Fetch-Site` says `cross-site` (or `same-site`, which is still
another origin); when the header is absent (pre-2023 browser or
non-browser client), fall back to comparing `Origin` against
`Host`; absent both → allow (curl/ureq/CLI are not browsers and
carry no ambient cookie). SameSite=Lax remains as belt. This
matters MORE here than in a normal app: loopback-is-owner (D68) is
ambient authority, and DNS rebinding hands a hostile page a
loopback origin — Fetch-Metadata + Origin/Host checks are what
close that class, so the gate also applies to loopback callers.
Bearer-token requests are exempt by construction (no ambient
credential). *Rejected:* synchronizer/double-submit tokens (state
+ plumbing a header-check makes redundant in 2026 browsers);
CORS-allowlist theater (we serve one origin; nothing legitimate is
cross-origin).

## D71 — Ambient refinement in serve mode: fresh tier, sweep leases, one niced worker (2026-07-11)

Analysis must not be a CLI errand while the daemon runs: the D45
fixpoint now advances by itself. `datboi serve` spawns ONE
daemon-lifetime worker thread (niced to 19 — optimization never
competes with serving; on Linux niceness is per-task and bfq derives
io priority from it, so an unsafe `ioprio_set` waits for a measured
need) that drains the sweep queues of the auto families in dependency
order (preflate → ecm → chunk, so the D59 "route-less?" question is
asked AFTER routes get minted). Two triggers: ingest completion feeds
the just-stored blob ids into a new fresh priority tier
(fresh > dat-matched > ambient — D47 intact, tiers order work,
membership stays dat-blind) and wakes the worker; a slow ambient
clock (30 min) re-runs the dat-blind candidate scan for everything
else. `--no-refine` / `DATBOI_NO_REFINE` opts out wholesale; D60
per-family gates keep working (checked per item, so a disable lands
mid-drain).

The worker owns a PRIVATE Db connection pair: a minutes-long preflate
split must never hold the request path's `Mutex<Db>`. SQLite WAL +
`busy_timeout` (now set on every connection) arbitrate; every index
write in the sweep path is a short transaction between long
byte-crunching stretches. Deconfliction across workers (daemon +
concurrent CLI sweeps) is a `leased_until` column on sweep_queue:
claim-then-analyze, at EXECUTION granularity — the driver claims one
item at a time, so a lease's clock starts when its work starts, never
when a batch was planned. The TTL is short (15 min) because renewal
is a PROGRESS-GATED heartbeat: analyzers pulse as bytes move through
their streaming loops (a `TickReader` wrapping the long read), and
the pulse re-stamps the lease every ~5 min over a second connection
(the main one is mutably borrowed mid-analysis). Liveness is
progress, not a timer — a wedged worker (dead NFS mount) stops
pulsing and its item frees in ≤ TTL, while a slow-but-alive split of
a disc-sized member renews indefinitely. Leases are DEDUP, never a
correctness gate — analyzers are pure functions and completion is
at-least-once, so a lapsed lease costs a duplicated pure function at
worst (renewal failures are swallowed for the same reason); the
daemon clears all leases at startup (one daemon per db-dir), and a
failed item KEEPS its lease as retry backoff (no hot-spinning on a
poisoned blob). *Rejected here:* a timer heartbeat thread (renews
while wedged — exactly the case the lease should lapse in), and
upfront batch claiming (a late batch item's lease aged before its
work began). Refine drains
report as first-class jobs in the tray (`JobKind::Refine`, item
counts, per-item current hash, closing outcome note).

Eviction needs no coordination with this worker by construction, and
that's the load-bearing observation: analysis is additive (mints
recipes, never destroys bytes), evict drops only replay-licensed
literals (D25), and an analyzer losing a race to eviction sees "blob
not resident" — a retryable error the queue absorbs. *Rejected:*
inline analysis at ingest (re-litigating D45; preflate at ingest
craters throughput exactly when the user is watching), a worker pool
(one writer beside the request path is honest for SQLite; parallel
splits are a measured-need change), coarse GC/analyzer locking
(nothing to protect — see above), durable refine jobs (rides the
existing open question; provenance rows D48 already persist the part
that matters).

## D72 — Background eviction: armed watermark, eager licensing, singleton guard (2026-07-11)

Eviction joins the daemon's background maintenance (the D71 worker
thread — ONE background writer beside the request path, so heavy
maintenance IO never runs concurrently with analyzer IO). Three
rulings:

**Armed by default.** High-water = 90% of the store filesystem
(statvfs), evict down to 85%; molten config
(`evict:high-water`/`evict:low-water`, absolute-bytes variants
accepted, `off` disarms). Eviction is reversible by construction
(D25: every drop has a locally-replayed route), so autonomy is safe;
the reconstruction-latency tradeoff is D27's, already ruled.

**Licensing is eager and ambient.** The worker replays Verified
routes in the background so literals are evictable BEFORE pressure,
and evictable-bytes reporting is always live. Scope is the
load-bearing constraint: only recipes covering CURRENTLY RESIDENT
blobs (the evict.rs verified-only pool) — replaying those is
storage-neutral (outputs already resident; content-addressed put
no-ops). Blanket-replaying every Verified recipe would materialize
every member CLAIM into the store — the exact bytes D35 ruled we
never store. *Rejected:* lazy license-at-pressure (a burst of heavy
replays exactly when the disk is full; speculative reclaim
reporting).

**The singleton guard — the ONE correctness lease.** Two concurrent
eviction runs can each compute the D21 grounding fixpoint, each
approve dropping one half of a mutually-inverse recipe pair, and
jointly strand both (the open-questions "evict racing evict" entry).
The drop critical section (plan → is_evictable → unlink) therefore
runs under a cross-process singleton lease (single-row cache.db
claim, TTL + renewal between drops, atomic UPDATE-claim under WAL);
`datboi evict` takes the same guard and reports "maintenance busy"
rather than waiting. Licensing replays run OUTSIDE the guard — they
are additive and need no exclusivity. Unlike D71's sweep leases
(dedup), this lease IS load-bearing for correctness and the two must
never be conflated or merged.

Candidate ORDERING is policy: best-licensed-route seek class first
(affine before opaque — D27's reconstruction-cost model), then size.
Found the hard way (the D72 e2e test): size-first eviction of a
mutually-inverse pair (container ⇄ preflate plaintext) drops the
plaintext and strands the container as a permanent literal — the
exact inverse of D53's plaintext-stays posture. Seek-class-first
evicts the affine-routed container, grounding then refuses the
opaque-routed plaintext, and the residual is the D53 promise with no
special-casing.

Crash safety is inherited, not added: drop order is unlink → flip
residency (recovery's store scan reconciles bytes-as-truth), replay
licensing commits per recipe, and a lapsed guard mid-run leaves a
half-finished eviction round that the next holder simply re-plans.

## D73 — Orphan sweep: reachability roots, mark→review→apply, delete stays human (2026-07-11)

The counterpart to D72 for bytes with NO rebuild route — the only
irreversible operation in the system, so it gets the only human gate.

**Reachability-only roots** (ruled over custody-as-root): a data
blob is a root-reachable non-orphan iff any of — referenced by any
recipe row, input or output, ANY verify state including Failed
(poisoned provenance still names real bytes); a dat revision or
detector blob; catalog-named (identity_blob ∩ rom_claim); reachable
from any tag (view/* snapshot row closure, image/* via their
recipes); pinned (blob.pinned_reason). Custody (source_file) is
deliberately NOT a root — an ingested blob nothing names is exactly
the junk the operator should see surfaced, and the review gate is
the protection. Meta-namespace lifecycle (old snapshots, alias
batches) is OUT of scope here — separate ruling when it matters.

**Mark → age → review → apply.** The ambient sweep MARKS candidates
(cache-grade `orphan_candidate` rows, derivable by re-sweep):
unreferenced AND not awaiting any enabled analyzer (a queued blob's
references may not exist YET — deleting it forecloses discovery) AND
re-verified each sweep (a mark clears the moment anything roots the
blob). A candidate becomes REVIEWABLE after a grace window from
first mark (default 24 h, molten) — no created_at column needed;
first-observed-unreferenced IS the clock, and the analyzer-queue
filter plus mark-clearing make the window self-healing for fresh
ingests. DELETION never happens ambiently: an operator applies the
reviewed set (Storage UI / CLI / API), each deletion re-verifies
unreferenced + grace + keep-mark AT DELETE TIME under the D72
singleton guard, then unlinks bytes and removes the cache rows
(children first; a crash between unlink and row-delete reconciles
bytes-as-truth like eviction).

**Keep-marks are authoritative.** "This is not junk" must survive a
cache rebuild: keeps live in state.db config KV (`gc:keep:<hash>`,
by hash not blob_id), riding the existing snapshot codec; a
dedicated table when keeps outgrow KV. *Rejected:* fully-autonomous
deletion (a root-set bug eats unrecoverable bytes before anyone
looks — revisit only after the root set has soaked), custody-as-root
(shrinks the reclaim surface the operator explicitly wanted),
cache-grade keeps (operator intent lost on rebuild = data loss on
the next apply).

## D74 — Durable job ledger: state.db by the session precedent, terminal snapshots only (2026-07-11)

The jobs tray's restart amnesia (recorded open question since the M5
web session, made user-visible by D71–D73's background jobs) closes
with a `job` table. Three rulings folded in:

**Placement.** state.db, by the `session` table's precedent:
authoritative but truncatable, EXCLUDED from CAS snapshots. Not
cache.db — job history is not derivable, and cache placement would
erase it on exactly the rebuilds it should survive; not
snapshot-carried — history is worth surviving a restart, not worth
carrying in the recovery root (the acquisition-provenance
measured-need reasoning applies verbatim).

**Terminal snapshots, not live rows.** The in-memory registry stays
the live surface; the ledger gets three writes per job — insert at
create (state running), finalize once at finish/fail (terminal state
+ the wire JobDetail JSON frozen as `detail`), prune to a bounded
tail (500). No per-file write amplification, and the frozen JSON
means a future JobDetail shape change degrades old rows to
column-stub rendering, never errors. Ids are db-assigned and thereby
unique across restarts — the in-memory counter would have collided
with history.

**Crash evidence is the point, not a bonus.** Registry construction
sweeps rows still `running` into an `interrupted` state (one daemon
per db-dir: any running row belonged to a dead process), surfaced in
the tray as failed-with-"interrupted" — a crashed 40-minute eviction
leaves a tombstone. Ledger failures never fail the job they describe
(best-effort persistence, loud on stderr). The scrub-run ledger and
eval report history remain future consumers of the same table
(additive kind codes). *Rejected:* per-progress-update persistence
(write amplification for a poll surface), cache.db placement
(derivability lie), a separate history service (the registry already
owns the vocabulary).

Amended same day — CLI wiring, structurally-can't-forget: every
mutating CLI command records a TERMINAL-ONLY ledger row (never
`running` — a live CLI legitimately violates the interruption sweep's
one-daemon-per-db-dir assumption, and a running row would be falsely
tombstoned; CLI crash evidence is worthless anyway, the human watched
it die). The enforcement device is `ledger_stamp` in the CLI
dispatcher: an EXHAUSTIVE match on `Command` with no wildcard arm, so
adding a command refuses to compile until its author decides
Some(kind)/None right there — the compiler asks the question. Kind
codes live once in datboi-index (`KIND_*`); scrub gained its own kind
(the scrub-run ledger's data half). The daemon's registry merges
recent ledger rows into `/v1/jobs` at poll time, so CLI history
reaches the tray live, not after a restart. View eval/image and
recover/snapshot are deliberately UNstamped: real byte-level work
that deserves its own kinds when its history surfaces exist, not a
shoehorn into Gc.

## D75 — Snapshot auto-cadence: content-derived dirtiness, authoritative-only trigger (2026-07-11)

Snapshots stop being operator-remembered: the D71 worker's ambient
tick runs `maybe_mint` — mint iff the AUTHORITATIVE TRIPLE (sources,
tags, config) differs from the newest logged snapshot's payload.
D72/D73 raised the stakes that forced this: keep-marks and watermark
policy are config rows now, and "crashed before I ever ran
`datboi snapshot`" would erase operator intent from the recovery
root. A fresh install auto-mints its first snapshot on the first
ambient tick.

Two deliberate scope cuts. **Dirtiness is content-derived, never
tracked**: no dirty flags or counters to desync — the check decodes
the last snapshot (a missing, undecodable, or foreign-keyed object
answers dirty; re-minting under our key is the fix for all three)
and compares the triple the next mint would record. **The trigger is
authoritative-only**: alias/analysis batches are derivable rows
whose loss costs recovery TIME, not truth, and detecting their drift
would mean re-encoding every shard per tick — the expensive way to
learn what `datboi snapshot` already offers. When intent DOES move,
the fired mint refreshes the batches anyway.

Mechanically: the mint moved verbatim from the CLI into
datboi-catalog::statesnap (one definition; `datboi snapshot` is now
the manual trigger + printer), identity-file helpers moved with it,
and the rider runs LAST in the maintenance cycle so the cycle's own
keep-marks ride the same tick. *Rejected:* per-wake checks (config
churn deserves minutes-latency durability, not per-ingest snapshot
objects), full-payload dirtiness (shard re-encoding per tick),
dirty-flag tracking (a flag that can lie replaces a comparison that
cannot).

## D76 — Hardening tightened: no style unsafe-inline, plugin/feature denies, no-store JSON (2026-07-12)

Three corrections to D70, prompted by an adversarially-verified
review. **CSP loses `'unsafe-inline'` in style-src**: the D70
justification was factually wrong — the SPA's dynamic styles are
Svelte `style:` directives, which compile to CSSOM writes
(`el.style.setProperty`/`cssText`), and CSP style-src governs parsed
`style=` attributes and `<style>` blocks, not CSSOM. The built
bundle carries zero `style=` attributes and no
`setAttribute("style")`, so the relaxation bought nothing and stood
as a standing CSS-injection grant if an HTML sink ever appeared.
**`object-src 'none'`, `X-Frame-Options: DENY`, and a deny-all
`Permissions-Policy`** (camera/geolocation/microphone/payment/usb)
join the header set: the UI uses no plugin content and no powerful
feature, so denying them is free insurance against future injection
or legacy-UA embedding. **`json_response` stamps
`Cache-Control: no-store`**: every /v1 JSON body is live
per-identity state (whoami, admin listings, Set-Cookie-bearing
session responses); a Set-Cookie response without no-store was a
caching-hygiene defect. The byte surfaces keep their own explicit
policies (immutable for content-addressed, no-cache+ETag for
tag-resolved). *Rejected:* per-route cache directives (the
serializer is the single seam every /v1 JSON passes through —
correct by construction beats a per-handler courtesy); CSP
hash-sources for a theme-flash inline script (deferred until that
fix is taken up).

## D77 — Error surfacing: closed ErrorCode union, translated by construction (2026-07-12)

Server error messages were hardcoded English rendered verbatim by the
UI — untranslatable by design of the envelope. The envelope becomes
`{"error": msg, "code": code}`: `code` is a CLOSED enum in datboi-api
(bad_request, upload_expired, unauthorized, invalid_credentials,
owner_only, invalid_invite, csrf_rejected, not_found, username_taken,
busy, store_full, internal), and the HTTP status derives FROM the code
(`ErrorCode::http_status`) so a handler cannot pair them wrong —
`err()` takes a code, not a StatusCode. The web maps codes to catalog
copy through a `Record<ErrorCode, …>` (errors.svelte.ts): adding a
variant fails `svelte-check` until the variant has a translated
message. `error` stays on the wire as diagnostic detail for CLI/log
consumers; the UI appends it parenthetically only for the codes where
it helps (bad_request, store_full, internal). The auth gate's
plain-text 401 — the one non-envelope /v1 holdout — now wears the
envelope too. Wire strings and statuses are pinned by a datboi-api
test; unknown future codes fall back to the raw message client-side.
*Rejected:* translating on the server (the daemon would need the
user's locale and a catalog per consumer); fine-grained per-message
codes (the UI context already knows what it asked for — categories
carry the user-meaningful distinction, detail carries the rest);
assertNever switch in the client (a Record is exhaustive at the type
level AND total at runtime for unknown codes).

*D76 amendment (same day):* the deferred theme-flash fix landed — one
minified inline script in index.html applies the forced theme before
first paint, admitted into script-src by sha256 hash-source; a server
test recomputes the hash from the embedded dist so the pin and the
script cannot drift.

## D78 — Web UI ships zero preferences: system theme, one density (2026-07-12)

The header's sys/☀/☾ theme toggle and the audit rail's
comfortable/compact density pill are DELETED — with their
localStorage keys, the forced-palette `data-theme` blocks in
tokens.css, and the theme-flash inline script plus its CSP
hash-source (retiring the D76 amendment; the script existed only to
serve the toggle). Color follows `prefers-color-scheme`, full stop;
rows ship comfortable, which also hands the library list the fixed
row height its virtualization math wants. Why: the vision's anti-goal
is config-screen explosion; pre-alpha software has zero demonstrated
preference needs, and each toggle was already apologizing for itself
(the density comment admitted it was "never given a home in the
comps"; the flash guard was a whole inline-script CSP carve-out for
a control nobody asked for). Preferences return one at a time when a
real user need forces them. *Rejected:* hiding the toggles behind
Admin (dead code with extra steps); keeping forced-theme CSS without
a toggle (unreachable states).

## D79 — Blob meaning is computed from edges at query time, never stored (2026-07-12)

The UI's three hardest questions about a blob — what IS it, whose
bytes are these, where did they come from — get one answer: walk the
recipe DAG from the blob to its claimed root(s), then read the
root's claims and source_file rows. Consumers: (1) the storage
by-source breakdown attributes derived blobs (containers, preflate
streams, chunks) to the library content they serve, narrowing
"(unattributed)" to truly UNATTACHED blobs — connected to nothing
claimed — which makes that bucket actionable instead of alarming;
(2) the blob page headline derives identity by priority: direct
claim name → role relative to a claimed root ("chunk 3/74 of X",
"container holding X") → ingest-sniff fallback; (3) derived blobs
display provenance *via* their root ("via roms/pack.zip, ingested
2026-07-11"). This reaffirms recipes (provenance is history in
the DB, never in recipes — recipes are timeless) and D18 (blobs
untyped; type lives in the edges) while closing the display gap they
left. *Rejected:* copying claims/provenance onto derived blobs at
mint time (denormalizes history into timeless objects); a mime/type
column on blob (D18 stands — sniff results are display hints, not
identity).

*Amendment (same day):* the sniff fallback is libmagic (the `magic`
crate) over nixpkgs' compiled magic.mgc, embedded at build time
(`packages.magicdb` → `DATBOI_MAGIC_DB` → `include_bytes!`, the D66
wiring) so the binary stays self-contained and the database moves
only with the nixpkgs pin. Eyes open: libmagic is a native C parser
of wild bytes, the exact species D58 banishes to wasm — admitted
because this surface is owner-only DISPLAY over bytes the owner
ingested, bounded to a 64 KiB head, and load-bearing for nothing.
If a peer-facing consumer ever wants the sniff, it moves behind the
sandbox first. *Rejected:* the hand-rolled four-entry magic table it
replaces (a naive user's "what IS this 716-byte blob?" deserves the
real answer — "Nintendo DS ROM image" beats silence).

## D80 — Per-blob verify graduates to the API (2026-07-12)

`POST /v1/blobs/{hash}/verify` mints a verify-one job (additive D74
kind), and the blob page's "never verified" badge becomes the button
that fires it and watches it land. This overturns the M5
"mutating pipeline actions stay CLI-only" ruling for this one verb,
by the same graduation test dat import and ROM ingest already
passed: that ruling's real rationale was long-running work wanting a
job registry, and D74 built the registry. Why this verb first:
verification is the product's core promise, and the moment of doubt
— "when was this last checked?" — is exactly when the user must be
able to act in place. Scope stays narrow: eviction, GC apply, and
view eval remain CLI. *Rejected:* keeping all pipeline mutations
CLI-only (the rationale expired with D74); a verify-everything
button (that's scrub, which has its own CLI + ledger story).

## D81 — Analyzer verdicts: parse failures are conclusions, Err is environmental, the index heals against the store (2026-07-12)

Rule for every analyzer, present and future: a deterministic
conclusion about the bytes — INCLUDING parse failures, e.g. a
zip-magic blob with no end-of-central-directory record — returns
`Negative` with detail (settled, never retried); `Err` is reserved
for environmental failures (I/O, not-resident) where a retry can
succeed. Before this, EOCD failures propagated as `Err`, so one
truncated zip re-errored on every 30-minute ambient sweep forever.
Second half: when an analyzer's store read finds nothing for a blob
the index calls Resident, the index is wrong — demote the row to
Absent on the spot (warn once) instead of erroring every sweep. A
`datboi doctor` bulk walk (index ↔ store reconciliation) is the owed
companion. Logging rides along: `eprintln!` is replaced by `tracing`
(INFO job boundaries, WARN self-heals, DEBUG per-item verdicts).
*Rejected:* retry-forever for deterministic failures (a permanent
noise generator); trusting the residency column over the store (the
store is the truth — a column that can't be corrected invites
exactly the split-brain a wiped store dir already demonstrated).

## D82 — The jobs tray dies: ambient indicator + activity page over the D74 ledger (2026-07-12)

The footer tray (strip + overlay panel) is deleted. Replacements: a
header activity indicator that exists only while jobs run (count +
spinner, quiet when idle — management by exception), and an
`/activity` page that finally reads what D74 already persists —
kind/state filters, relative timestamps from started_at/finished_at,
expandable per-item `report.errors`. Ingest keeps its inline
feedback on the screen that started the job: feedback belongs where
the action happened; the activity page is history. Transport stays
REST + poll (2 s while running; the SSE upgrade stays deferred per
open-questions). Why: jobs are system activity, not a primary
object — a persistent tray put non-ambient information in ambient
chrome, wrapped badly at every width, and threw away timestamps and
error detail the API already served. *Rejected:* keeping the tray
(all of the above); SSE now (poll cadence is fine at this scale).

## D83 — NDS: NitroFS decomposition + trim ride assemble@1; wasm deferred to three named lanes (2026-07-12)

An NTR-era .nds ROM is a pure concatenation — header, ARM9/ARM7,
FNT/FAT/overlay tables/banner, then NitroFS files at absolute FAT
offsets, pad bytes in the gaps, nothing compressed or encrypted at
the container level. So the whole lane is builtins: `nds-split/1`
is a native analyzer in datboi-ingest (zip precedent; D81 verdict
rules) that parses header + FAT into a coverage map over [0, len),
claims piece identities (binaries, tables, each FAT file,
non-uniform gap residue — absent rows, never member copies) and
three recipe shapes, every one assemble@1. Rebuild = segment walk
in physical storage order (Literal header, Fill pads, BlobRange
pieces — files are not guaranteed FAT-ID order, the recipe records
actual order); derive = one BlobRange slice per member; trim = a
prefix slice whose identity is claimed at analysis time WITH a
full alias tuple (trimmed dumps circulate; dat aliases must hit
the claimed identity — serving stays view-time). All-affine
means the D63 carve-out serves rebuilt ROMs, members, and trimmed
views without materializing, and bit-faithfulness is enforced by D4
replay, not parser perfection — a wrong coverage map fails
verification and the ROM stays literal. Trim rules bake in at
analysis time: DSi/hybrid (unitcode != 0) trims at [210h], never
[80h] (cuts the TWL region, hangs the game); NTR trims at [80h]
plus 88h bytes when the "ac" magic sits at that offset (the DS
Download Play / cloneboot RSA signature naive trimmers strip); trim
is offered only when the size clears every declared section and FAT
entry AND the discarded tail is uniform pad (fake-header ROMs;
translation patches append data past header size). Trimmed-in is
lossy: a ROM someone else already trimmed may lack the RSA block,
so the full dump is unrecoverable — store as-is, identify via dat
aliases. Anomalies (overlapping FAT entries, unparseable tables,
excess residue) → Negative, settled. Wasm enters later on three
named lanes, carried as catalog rows + an open-questions item:
secure-area KEY1 normalization (BIOS-derived key material), DSi
modcrypt (console keys — joins the existing key-policy question),
and interior/overlay decompression (preflate-shaped). NARC
recursion is not one of them (same FNT/FAT format, IMG-relative
offsets — still pure assemble) but is policy-gated on recipe
volume. *Rejected:* an ex-nds extractor component (nothing to
sandbox — no container compression; builtins beat component pinning
and an opaque seek class); trusting the header trim size
unconditionally (known fake-size ROMs trim to 512 bytes); storing
trimmed variants as blobs (trim is a view-time slice over the same
pieces).

## D84 — Browser emulator cores are web-bundle assets, not CAS components; DS first via dust-core (2026-07-12)

Emulator cores are a **third wasm lane**: built like unrar (D58 —
standalone `datboi-emu-*` crate, own lockfile, upstream fetched +
pinned + patched via nix, wasm32 target) but consumed like the web
dist (D66/D67 — flake package → `DATBOI_*` env var → served as a
lazy-loaded static asset), and exempt from the component doctrine
entirely: no WIT world, no wasmtime, no recipe pinning, no
determinism gate — `wasm32-unknown-unknown` + wasm-bindgen, because
they run in the *browser* and nothing downstream depends on their
byte-exactness. Design record: [emulation.md](emulation.md)
(pulled forward from the roadmap M7+ frontier; the M5 web surface
reserved the ▶ Play slot). First console is DS via `dust-core` — the
only accuracy-credible library-shaped Rust DS core, browser-proven
(worker + transferable frames + scheduled AudioContext, no
SharedArrayBuffer needed), HLE-BIOS direct boot so no Nintendo files
ship or are required. Its costs are accepted and named: nightly +
`-Zbuild-std` + git deps (pin and vendor — spike milestone 1),
bus-factor-one upstream (vendored-snapshot posture, as unrar), and
GPL-3.0 in an MIT workspace (per-crate license, the
`LicenseRef-unRAR` precedent; source-offer satisfied by the in-repo
fetch recipe + patches). The host contract (core descriptor + worker
protocol) is codified but deliberately unfrozen until a second core
(tetanes-core) exercises it. Headers ride along: COEP `require-corp`
joins the D70 set now while it is free, and CSP script-src gains
`'wasm-unsafe-eval'` (Chromium blocks `WebAssembly.compile` without
it). *Rejected:* cores as CAS components in the transform/extractor
lane (the determinism contract is wrong on every clause);
wasmtime-side execution with streamed frames (a remote-play product,
not an embedded emulator); libretro as the host ABI (a C ABI built
on process-global callback statics — prior art for wrapping cores,
an anti-pattern to adopt); melonDS or DeSmuME first (no
library-shaped wasm path: emscripten fork or dormant port); NES
first (proves nothing DS doesn't — single screen, no pointer, no
perf pressure); shipping or requiring Nintendo BIOS/firmware (HLE
direct-boot covers v1; the later BIOS story is
known-hashes-from-CAS, see emulation.md).

*Amendment (same day):* the spike shipped through milestone 3 and
two details moved under it. (1) Play is NOT owner-only: play rights
are download rights. The ▶ lives in the Browse entry panel beside
the download anchor, the ROM bytes come from the same granted
`/view` surface, so a session that can download can play and one
that can't gets the same 404 — the deferred friend-play-ACL
question collapses for v1 with zero new surface (it reopens only if
play ever grants more than bytes, e.g. server-side saves). (2)
`/shelf/{view}` and `/play/…` became owner-reachable deep links so
the owner has the same entry panel — NOT nav tabs; the screen-
taxonomy naming pass (open-questions) keeps ownership of any bigger
move. Also locked in by M2's testing: audio crosses the worker
boundary as a pull (take_audio riding the frame message), never a
wasm-held JS callback — a Function passed into the instance hangs
create inside a Worker on Chromium 148 headless.

## D85 — The library plays: audit-drawer ▶ via raw blob bytes (2026-07-13)

The entry drawer under Library (the audit drill-down) gains the ▶
Play the M5 comps reserved: for each rom claim satisfied by a local
blob whose filename a shipped core claims, the drawer links to
`/play/blob/{hash}/{rom-name}` — a second Play source alongside
`/play/{view}/{path}`, fetching ROM bytes from
`GET /v1/blobs/{hash}/bytes` (the endpoint BIOS-from-CAS already
added; the URL is the content hash, serving rides the same verified
windows). Zero new API. Rights stay coherent with the D84 amendment
(play rights are byte rights): the audit surface and the raw-blob
surface are both owner-only, so the drawer ▶ is exactly as reachable
as the bytes behind it, and friends keep the view-path route — a
friend deep-linking a blob-play URL bounces home like any other
owner route. The rom name rides the URL tail so core gating stays
extension-based (registry) and the screen keeps an honest title.
*Rejected:* resolving an entry to a view path via pins (a playable
blob may be pinned by zero views, and pins don't carry paths); the
"playable payload resolver" endpoint emulation.md reserved (the
blob route makes it unnecessary); gating ▶ on verified-only (claimed
bytes serve and play the same; the state line already tells the
truth about trust).

## D86 — Touch controls: spatial separation, capability-gated, press-intent semantics (2026-07-13)

Phones get CSS-drawn touch controls on the Play screen
(open-questions emulation item 5: a phone could tap MKDS menus but
never press A to drive). Three rulings. **(1) The deck never
overlays the pointer screen.** A DS bottom screen is itself a touch
input; an overlay would force a buttons-vs-stylus mode switch.
Instead the controls own the space letterboxing wastes — below the
stacked screens in portrait, flanking gutters in landscape — so the
bottom screen stays a pure stylus surface and buttons + stylus work
simultaneously (Mario 64 DS needs both at once). When space is tight
the canvas shrinks, the deck doesn't: playable beats big.
**(2) Gate on capability, never preference (D78-safe).** The deck
renders while `(pointer: coarse)` matches — the primary input is a
finger — and follows the media query live. Touchscreen laptops keep
the desktop layout (their primary pointer is fine;
`any-pointer: coarse` would catch them). Nothing persisted, no
toggle. **(3) Press semantics from the virtual-gamepad state of the
art**, in a pure unit-tested module (`lib/emu/touch.ts`): press on
pointerdown, never click (intent-of-press — no synthesized-click
latency); per-pointer role latch — a pointer that lands on the d-pad
IS the d-pad until it lifts, steering by vector from the pad center
(8-way, 45° sectors, center dead zone) even after sliding past the
pad edge; button pointers re-hit-test as they move, so rolling B→A
never needs a lift; hit zones are larger than the visuals
(nearest-within-slop); a rising press edge ticks the vibration motor
where the platform has one. Layouts are declared per side in an
abstract unit space and filtered by the descriptor's button set, so
a second core (NES: no X/Y/L/R) reuses everything unchanged. The
deck is aria-hidden: it duplicates the keyboard map, which remains
the accessible input. *Rejected:* overlay + mode toggle (modal input
breaks simultaneity and is a toggle); overlaying only the top screen
(thumbs live at the bottom); gating by user-agent sniff or viewport
width (capability is what matters, and the media query is the
capability).

*Amendment (same day):* live-iPhone debugging (ios-webkit-debug-proxy
against the shipping phone, after Chromium AND Linux-WebKit emulation
both showed correct layout) found iOS 26 Safari resolving a grid
item's percentage height against the grid CONTAINER, not the item's
grid area — the canvas computed the stage's height and painted under
the entire deck. Posture locked in: **the canvas is layout-inert.**
A plain div (the frame) owns the grid area via stretch alignment —
the mechanism that always sized the pads correctly on every engine —
and the canvas hangs inside it absolutely positioned; the deck grid
carries no percentage-sized items and no tracks sized from item
intrinsics (engines then disagree only about the frame's aspect-ratio
under single-axis stretch, which object-fit makes invisible). Two
smaller same-session rulings: cluster boxes are sized by a measured
ResizeObserver fit, not CSS aspect-ratio auto-sizing (collapses to
0×0 when every child is absolutely positioned), and the whole play
screen disables text selection + the long-press callout (touch play
kept triggering both — it's a game surface, not a document).

## D87 — Fullscreen play: one immersive flag, native API where the platform has it (2026-07-13)

The Play screen gains fullscreen: one `immersive` flag with two
mechanisms. The flag always applies a CSS takeover (fixed, inset 0,
app chrome gone, safe-area padded); where element fullscreen exists,
`requestFullscreen()` rides along for true browser-chrome removal —
iPhone Safari has no element fullscreen, so the takeover IS the
fallback and the flag never lies about state. Exit: a small ✕ (the
only chrome immersive keeps), Escape in the takeover, and the
`fullscreenchange` event keeps the flag honest when the browser
exits natively on its own. Touch controls are deliberately NOT
coupled to fullscreen — a phone without the deck is unplayable, so
the deck follows the D86 pointer gate in both modes; fullscreen just
buys the canvas more pixels. *Rejected:* touch-controls-only-in-
fullscreen (couples playability to a mode switch); auto-immersive on
touch devices (stealing the browser UI on arrival is hostile — one
tap opts in); orientation locking (the stacked DS layout is
portrait-native; nothing to force).

## D88 — Doc filenames drop positional numbers: names cite, an index orders (2026-07-13)

The `NN-name.md` scheme is retired; subsystem docs are bare stable
names (`cas.md`, `views.md`, …) and `docs/README.md` is the single
place that encodes reading order. The numbering failed for a
diagnosable reason: it was POSITIONAL — each number claimed a slot in
an ordering, and growth invalidates slots. Growth here is lopsided
(new subsystems land at the surface layer: cli, web-ui, emulation,
saves all crowded the 80s until 89 was the last slot), so any gap
scheme re-crunches; meanwhile the citation graph only densifies
(house style mandates liberal doc citation in code comments — ~180
references at rename time), so every future renumber costs more than
the last. Contrast the numbering in this repo that works: D-numbers
are APPEND-ONLY IDENTIFIERS, position-free, so citations never rot.
Filenames now follow the same principle — the name is the identifier;
order lives in exactly one file (the index) where changing it breaks
nothing. The one real benefit numbers delivered (vision-first,
roadmap-last in a cold directory listing) moves into README.md.
*Rejected:* re-spacing the tail / moving roadmap to 99 (buys nine
slots, then re-crunches at doc ~25); three-digit renumber (same
positional failure, bigger blast radius per crunch); keeping token
sentinels like `00-vision` (a half-scheme reads as drift, not
design); dropping the reading order entirely (alphabetical listing
buries vision and roadmap — the order is worth keeping, just not in
filenames).

## D89 — The ABI epoch break: named lanes, semver with teeth, CBOR vocabulary, extractor reshaped (2026-07-14)

The world numbering was D88's disease in the ABI namespace: the major
version was a PROFILE REGISTRY (@1 = whole-buffer, @2 = streaming,
"@3 reserved for wasip3"), one integer doing two jobs — profile
identity and contract revision — so a whole-buffer fix would have
become @3, shape-incompatible with the @2 "below" it, and crate
vending would need a decoder ring. Ruled, with a CLEAN BREAK
authorized on the finding that no non-dev stores exist (last cheap
moment; epoch reuses the clean names, nothing was ever published):
profile identity moves into the package NAME (a *lane*), versions do
only semver within one shape, and every published version is
immutable forever — D51's freeze restated per-version. Lanes:
`datboi:streams@1` (the shared source/file/sink contract, one home
for doctrine previously copy-pasted between worlds), a streaming-
shaped `datboi:transform@1` (whole-buffer world DIES — the host never
consumed its "definitely not streaming" signal; buffered authoring
becomes guest-crate sugar), and a reshaped `datboi:extractor@1`
(containers become `list<file>` — the recipe side was already plural;
`extract` takes a request BATCH, killing the O(n²) solid-archive
ingest the single-member signature forced, with a new gate-tested
clause that member bytes are pure in (containers, ix) regardless of
batch; both exports gain a `params` bstr the recipe layer was already
smuggling around the wit). Vocabulary surfaces (`describe`,
`enumerate`) return canonical-CBOR `result`s like params — record/
enum growth becomes schema evolution, not ABI breaks — under the
advisory-keys rule (D64: old hosts meet new keys; anything a host
must understand is a real version, never a key). Semver policy has
wasmtime enforcement (semver-aware import resolution + instance
subtyping): additive host imports and probed exports are minors,
any shape change is a major, host linkers are append-only forever.
Vending: one crate per lane, `datboi-guest-<lane>`, crate major.minor
mirrors the world it binds. Publishing: wkg-encoded wit packages as
flake outputs, `nix run .#publish-wit` to GHCR (check-then-refuse:
the publish gate enforces immutability) as a job in the existing
container workflow, keyless-cosign signing both wit packages and the
container image. wasip3/component-model async DECLINED: guests
observing readiness imports host scheduling into guest-visible state
— the nondeterminism class D5 makes unrepresentable — and freezing on
an in-flux encoding contradicts freeze-forever; it buys host cost,
not capability, and waits for a future streams@2. Full design:
docs/worlds.md (the canonical home for the ABI; runtime.md §ABI
retires to a pointer when the break lands). *Rejected:* integer
profile registry (the disease); grandfathering the old worlds beside
named lanes (correct only if real stores existed — they don't, and
the wart would be permanent); keeping the whole-buffer lane (its one
consumer was author ergonomics, which a ten-line adapter serves);
wit-typed descriptor/member records (every advisory field a
structural break); adopting wasip3 async now (determinism hazard,
unstable encoding); a single `datboi-guest` crate (two independently
versioned lanes give one crate no honest version number);
suffix-`-guest` crate names (inverts the house family-prefix grammar
and scatters crates.io prefix search).

*Amendment (2026-07-14, the break landed):* shipped whole the day
after the ruling — wit tree, vending crates, hosts, exec/ingest,
fixtures, goldens, dev-store wipe, publish tooling; flake gate green.
Three refinements recorded in worlds.md §landed notes: (1) MEASURED —
a wit doc-comment edit churns every component's bytes (wit-bindgen
embeds the doc-bearing encoded wit), so wit text freezes with its
version and a typo fix is a format event caught by the golden pins;
(2) the buffered sugar is a trait + export macro (statics, not
closures); (3) extractor recipe params stay HOST-interpreted member
selection and the world call passes an empty bstr — world-level
params (passwords) will be a recipe-schema forwarded subset, not a
re-reading of existing bytes. Component stamps now carry both source
trees (`tree:<crate>;guest:<guest-crate>`).

## D90 — At-rest compression delegates to the filesystem (2026-07-15)

Object identity is the *uncompressed* bytes' blake3 (D2/D18), so
compression at rest can only ever be an encoding below or beside the
store — and the ruling is: below. The store writes plain bytes; the
filesystem compresses (ZFS/btrfs zstd on the NAS — the target
deployment already does this transparently). Store-level encoding
(seekable-zstd frames, a per-blob encoding flag) is REJECTED until a
backend without a filesystem underneath actually needs it — S3/HTTP
are the named future exception, and cas.md's S3 sketch already
reserves the metadata flag. Why: the win over ZFS is zero on the
deployment that exists, while the cost is real — outboards verify
uncompressed bytes, so verified range reads would need a
compressed-frame offset map under every seek path, plus level policy.
The ruling forecloses nothing by construction: encodings never touch
identities, recipes, or wire hashes, so this retrofits exactly like
D19 packing if the S3 day comes. Operational guidance for local
stores on ext4/xfs (documented posture, not a gap): a loopback file
carrying btrfs or ZFS with zstd on and discard/hole-punching enabled,
so the backing file shrinks with the store.
*Rejected:* uniform store-level seekable zstd now (obao frame-map
complexity + compression-level knobs, for bytes the target filesystem
already saves); leaving the question open (the ext4 story reads as
oversight instead of posture, and the question keeps costing
attention).

## D91 — Affine piece-swap: pieces over container, sealed packs per decomposition (2026-07-15)

First exercise of the third residency knob (WHICH literal holds the
bytes — the open-questions dat-aware-residency thread): when a
resident literal's rebuild route is AFFINE (pure-builtin assemble,
the D63 class), the planner may materialize the route's pieces and
evict the container — pieces over container. Affine-gated because
both costs that could bite can't: serving the evicted ROM stays
range-arithmetic (no recompute, D63), and the spill rule is
unreachable (no opaque op can sit below a random-access demand, by
construction). NEVER eager: a lone ROM's swap buys pad savings and
pays a piece bill, so the swap is gated on a plan-time sharing
predicate — piece bytes claimed by ≥2 distinct rebuild recipes (or
already resident from elsewhere) above a molten threshold, a policy
KV like the watermarks. Evidence: MKDS USA↔EUR share 556 of 564
NitroFS pieces — a variant pair converges to ~1.02× instead of 2×.
This is a MAINTENANCE PHASE (plan-time SQL alongside
license/mark/evict), never an analyzer: D47 stays intact, sweeps and
claims untouched; variant B finishing nds-split trips variant A's
predicate one ambient wake later, no event plumbing. Materialization
writes ONE SEALED PACK per decomposition — pieces in coverage order,
magic'd self-describing index footer — D19's packing clause
exercised for the first time: inode growth O(swapped ROMs), not
O(pieces); rebuild IO ~sequential (coverage order = read order);
cross-variant serving touches ~2 packs; recovery scans still sniff
contents; packs immutable, rsyncable. Pieces are grounding leaves
(their only route derives from the container they ground), so packs
are stable; tombstone-and-repack under the gc guard is the escape
hatch, not the plan. Prerequisite: the D56 disk-headroom guard — the
swap is transiently double-resident by design. Interactions recorded
now: (1) this ruling CREATES resident grounding-leaf pieces, the
exact population D59's has-any-route gate mispredicts (routed on
paper, route-less to the D21 fixpoint) — the rank-7 CDC amendment in
open-questions is the queued fix, trigger unchanged; (2) chunk sets
are the same small-blob-flood shape — pack-per-chunking is the named
follow-on, not built here.
*Rejected:* eager swap-on-decomposition (inode + IO cost for
pad-only savings on lone ROMs); keep-dat-named-blobs-resident as the
general rule (dats name the ROM, which would block the swap
everywhere it pays — the instinct is right only where eviction
degrades serving to recompute, i.e. opaque routes, which this ruling
simply never touches); loose piece files (O(pieces) inodes cranks
the D19 accepted cost toward millions at scale); a background
repacker (the swap job knows membership and read order at write
time — grouping needs no guessing, packing rides the swap).

## D92 — Analyzers consume the logical CAS (2026-07-15)

The refinement fixpoint's promise is "analysis advances over the
corpus" — and the corpus is every grounded identity, not every
resident literal. Ruled: sweep candidacy is GROUNDED, not resident;
analyzers read blob bytes through the executor (verified streams,
spill for seek-demanding formats) instead of `store.get`. The
resident-only gate was an implementation leak, not doctrine: the
analyzer contract (D45, "pure function of bytes × identity") never
mentioned residency — bytes-by-hash are identical through any route
— and the happy path only worked by side effect (preflate and the
7z/rar extractors happened to materialize what the next analyzer
needed). The stalls it caused were real: a dat-matched .nds STORED
in a zip was claimed at ingest and never analyzed (no NitroFS
claims, no trim alias); members of preflate-refused containers, same;
every nested interior gated on unrelated materialization events.
Three arguments carried it. (1) Trust: executor materializations are
tee-verified (D4), so a logical read is exactly as trustworthy as a
physical one, and a wrong claim could only ever waste replay CPU —
D4's stated worst case. (2) Purity: preserved perfectly; provenance
rows stay identity-keyed and route-blind. (3) Consistency — the
clincher: the D39 audit already grades grounded-but-absent as
have-verified. The library says "you have this ROM" about identities
analysis refused to look at; every other subsystem (audit, serving,
GC grounding) defines existence as groundedness. This ruling brings
the last holdout into the system's own philosophy. What stays MOLTEN
(policy KV, the D60/D72 pattern — mechanism ruled, thresholds
molten): eagerness — which absent blobs enqueue (dat-named first is
the obvious start, the D71 dat-aware scheduling lane), replay budget
per sweep, and head-sniff admission (sniffing through an opaque
route costs a partial replay). Owed design work, named not blocking:
`enqueue_unanalyzed` becomes grounded-set-aware — a fixpoint
question at enqueue time, at corpus scale (the audit rollup already
computes this set; sharing or caching it is the likely shape).
Recursion is bounded by content depth plus the existing per-format
policy gates (the NARC recipe-volume clause), not by residency
accident.
*Rejected:* resident-only sweeps as permanent doctrine (the gap this
entry exists to close); materialize-for-analysis as the primary
mechanism (entangles residency policy with analysis progress —
residency is the planner's knob, D91's territory; the executor's
bounded spill inside one analysis is fine, a residency flip is not);
analyzer-side special-casing per container format (the executor
already generalizes exactly this).

*Amendment (same day, D91 landed):* pack resolution went
STORE-INTERNAL, not index tables — `Store::open` scans pack footers
(one tail read per pack, O(decompositions)) into an in-memory map,
and `get`/`has`/`len` fall through to bounded windows, so every
consumer present and future inherits pack support by construction
and recovery needs no database (footers are the truth, D15).
`Store::get` returns the windowed `Blob` handle; a packed blob
refuses eviction explicitly (`Blocked::Packed`). Rejected in the
landing: cache-db pack tables (a resolution cache nothing needed —
the map is derivable state the store already owns); obao sidecars at
pack time (packed pieces serve through the D4 plain-read literal
default; `ensure_obao` over the window upgrades later). Landed
defaults: `swap:share-min-pct` 50, `swap:enabled` on, swap phase on
ambient ticks under the D72 guard. Owed, recorded in open-questions:
pack scrub coverage (LANDED 2026-07-16 — `scrub_pack` re-hashes each
whole pack against its identity, one read, certifying every member and
back-filling aliases), tombstone-and-repack, packs for chunk sets.

## D93 — Fearless concurrency: parallel by default, serialization must name its argument (2026-07-16)

The posture inverts: concurrency is the DEFAULT everywhere, and any
serialization point must carry a named correctness argument — "one at
a time" is no longer a design to inherit, only a conclusion to prove.
The named survivors: the D72 gc guard (two grounding counterfactuals
can jointly strand an inverse pair — the one correctness lease), and
the request path's single WRITE connection (auth flows are
check-then-act — invite redemption, session mint — whose atomicity
today comes from doing both halves under one lock hold). Everything
else parallelizes:

**Refinement drains multi-threaded by default.** Worker count =
`max(ceil(n/2), n−2)` of available parallelism (n≤2 ⇒ 1, 4 ⇒ 2, 8 ⇒
6, 16 ⇒ 14), molten as `refine:workers` ("auto" | a number; 1
restores the old shape; read at daemon start). One PRIME worker keeps
everything coordination-shaped: wake handling, queue refresh (the
per-wake grounding fixpoint), the tray job per family drain, lease
amnesty, and ALL maintenance phases. The remaining workers are
DRONES: own `Db` connection each, a shared `Executor` (it is `Sync` —
pinned by test — so compiled components are cached once), nice(19),
and nothing but claim-analyze-complete loops. This supersedes D71's
"one dedicated worker thread" SHAPE while cashing the design D71
already built: the lease column is claim-granular work distribution
(daemon + CLI sweeps already exercised it), at-least-once absorbs
every race, and a lease is dedup — so adding workers is scheduling,
not correctness. Tray progress switches to queue-depth deltas so
drone work shows in the prime's job.

**Request-path reads leave the mutex.** WAL has always allowed N
readers + 1 writer; the `Mutex<Db>` serialized reads for no named
reason. The server gains a pool of READ-ONLY connections
(`Db::open_read_only` — flags-level read-only, so a misclassified
handler ERRORS loudly instead of corrupting quietly: the fearless
posture is safe exactly because the fence is mechanical). The
per-request auth middleware (`resolve` — pure SELECT) and the
read-only serving/browsing surfaces move to the pool; every write
stays on the single write connection behind the mutex, preserving
D71-era reasoning wholesale for the surfaces that mutate.

*Rejected:* a rayon/work-stealing pool inside the drain (the lease
column already IS the dispatcher; a pool inside a queue is two
schedulers fighting); n workers (leave headroom for the request path
and a running emulator — the formula's floor and ceiling both exist
on purpose); connection-per-request writes (check-then-act atomicity
would silently become a race — each write surface must be audited to
row-level guards before it leaves the mutex, a per-surface follow-up,
not a default); per-drone Executors (recompiles every component per
thread for no isolation win).

*Amendment (same day, D93 landed):* the build surfaced two latent
race classes and settled the shape that prevents them. (1) The claim
transaction was DEFERRED — a read-then-write whose upgrade returns
SQLITE_BUSY without consulting the busy handler, latent since D71 for
daemon+CLI, constant under drones. Ruling refinement, made MECHANICAL
rather than audited-for: every read-write connection sets its
DEFAULT transaction behavior to IMMEDIATE at open
(`set_transaction_behavior`), so `transaction()` and
`unchecked_transaction()` cannot mint the deferred-upgrade class at
all; `Db::cache_write_tx`/`state_write_tx` remain the self-
documenting spelling. Safe to flip wholesale because pure-read
TRANSACTIONS on rw connections don't exist here — reads ride the
read-only pool or bare statements (verified: the grounding fixpoint
uses temp-table batches, no transaction). The audit that preceded
the flip also converted the read-then-write sites it found (invite
acceptance, dat unification) and hardened the migration ladder
against concurrent first-opens (re-check the stamp inside each
IMMEDIATE step).
(2) Cross-thread signaling follows condvar discipline: everything a
sleeper can be woken FOR lives under the condvar's own mutex (the
prime's inbox) — a signal flag outside it reproduced the classic
lost-wake, caught as a once-per-many-runs e2e flake. Also landed:
tray notes report fleet-wide provenance deltas (a drone's positives
must not vanish from the prime's job); job completion gates on
fleet-idle-or-queue-empty; seek quarantine writes are best-effort
(cache-grade by their own schema comment) so read-only serving
connections stay serving. The worker formula
was also revised on challenge: `max(⌈n/2⌉, n−2)` is core-
proportional, but cores are not the binding constraint — nice(19)
already protects the request path, the claim lock serializes small
items (throughput plateaus at a handful of workers), preflate's
split state is ~70 MiB worst-case per ACTIVE worker (a core-count
fleet on 32 cores ≈ 2 GiB of ceiling), and interleaving many
sequential NFS readers can sink aggregate throughput below one on
spinning arrays. Landed default: `⌈n/2⌉.clamp(1, 6)` — memory- and
IO-shaped, not CPU-shaped; big iron overrides the molten knob. What
remains vigilance, named: write handlers still choose the write
mutex by hand — the per-surface audit that would let writes pool
(row-guarded check-then-act) is future work, and until then the
mutex is the named argument.
