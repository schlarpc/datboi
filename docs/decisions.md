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

*Amendment (2026-07-16): the sidecar extension is `.obao4`, not `.obao`.*
The M6 spike surfaced that bare `.obao` is iroh-blobs' convention for the
STANDARD bao format's 1 KiB (2^0) granularity, while the trailing digit
in `.obao4` names a 2^4 = 16 KiB chunk group — which is exactly the tree
this entry froze. Our file held obao4 content under an `.obao` name: a
misnomer by the established convention, so under correct-by-construction
the name changes to state what the bytes are. This is a free format event
(pre-corpus, D54 logic): no on-disk migration exists to break, and iroh
never sees our filenames (we serve via our own handler, D97, not by
pointing an iroh store at our tree — so this is naming honesty, not
interop). Changed `outboard_path` + the recovery classifier + tests; the
golden-vector (which pins the tree BYTES, not the filename) is untouched.
Applies uniformly to loose-blob and D91 packed-member sidecars — both are
the same `data/…/<hex>.obao4` files.

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
*Amendment (2026-07-16, rank-7):* the "NO existing covering route"
gate was implemented as has-any-recipe, which MISPREDICTED the
resident grounding-leaf pieces D91 creates. A decomposition piece
carries a `container→piece` recipe row, but its container grounds via
this very piece — so the piece is route-LESS to the D21 fixpoint
despite the row, and its cross-variant near-misses (MKDS USA↔EUR: 8 of
564 pieces differ, ~1.3 MiB) are exactly what CDC should dedup. The
gate is now `is_covered_by_others` — grounded WITHOUT the blob's own
literal, at the same non-failed trust level — which draws the "real
route vs recipe on paper" line precisely. Paired with an explicit
resident-only guard (chunking mints resident chunks, so an absent
grounded blob must NOT be chunked — that would materialize it, the
opposite of the dedup goal; this also replaces the old gate's
incidental reliance on absent items being "routed" to skip them
without a spill). Sequencing note preserved: NARC/SDAT interior
decomposition should eat the archive-shaped near-misses before CDC
takes the media-stream remainder.

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
*Amendment (2026-07-16):* the swap phase now BLESSES each packed
piece's obao over its window right after `put_pack`, before the
container evicts — refining the amendment below that rejected obao "at
pack time." That rejection stands for the pack FILE (sidecars never go
inside the immutable pack), but the swap evicts the container in the
same phase, so "serve through the D4 plain-read default, upgrade
later" would in practice mean the container's VERY FIRST served range
pays a lazy `ensure_obao` over every piece, on the serving thread, a
stall proportional to the whole decomposition. Blessing during the
swap costs one warm re-read of freshly written bytes and removes that
stall entirely. Sidecars live beside the member (`data/…/<hex>.obao`),
so the lazy `open_random_verified` path stays the backstop for
packs restored by bare-NAS recovery (whose member sidecars the walk
did not rebuild).
*Amendment (2026-07-16, pack-per-chunking):* the named chunk-set
follow-on landed as a maintenance phase, `pack_chunk_sets`, sibling to
the swap. Chunk pieces differ from decomposition pieces in ONE way —
the CDC analyzer writes them RESIDENT (loose) immediately, so there is
nothing to materialize. The phase iterates the same `swap_candidates`
(affine assemble routes), collects each set's LOOSE, unpacked,
grounding-leaf inputs, streams them straight out of their own loose
files into one sealed pack, blesses each obao over the window, and
drops the redundant loose `.data` (keeping the `.obao`) — trading N
inodes for one. First-packer-wins preserves cross-set dedup (a shared
chunk packs with whichever set reaches it first; the rest see it
packed and skip). Crash-safe by construction: a piece left both packed
and loose by an interrupted run is swept on the next pass.
Policy-gated `chunk:pack` (on by default, dormant until a D59 chunk
flood exists) with a `chunk:pack-min-members` floor (default 4 —
packing one piece just swaps one inode for another). The accepted cost
is write amplification (chunks written loose, then re-read into the
pack), paid on the niced maintenance thread; born-into-pack was
rejected because it needs the whole set buffered or a second CDC pass.
*Review (2026-07-16, M6 spike):* an outside-eye pass flagged two
actionable reconsiderations — outboard-in-pack (a v2 footer section so
outboard inodes are O(packs) not O(pieces), and packed-member outboards
survive bare-NAS recovery; the eager-blessing amendment above removed the
premise for keeping them loose) and a footer integrity check (the
`(offset,len)` map is trusted at open, unlike a self-verifying loose
filename) — plus three enduring watch-items (first-packer locality
coupling, repack write-amplification under sharing, content-named-but-not-
convergent packs). The unifying framing: position-independent member
identity is the feature AND a two-part metadata bill. All recorded in
open-questions.md § pack-format review.

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
back-filling aliases), tombstone-and-repack (LANDED 2026-07-16 — Store::repack rewrites a pack without its orphaned members; orphan GC routes packed pieces to it since remove_blob can't unlink pack bytes), packs for chunk sets (LANDED 2026-07-16 — pack_chunk_sets maintenance phase, D91 amendment).

*Amendment (2026-07-16, grounded-set-aware enqueue):* the owed
enqueue-side work landed as fixpoint DEDUP. `refresh_queue` was called
once per analyzer family per wake, and each call recomputed the
grounding fixpoint (`refresh_absent_eligibility`) — the corpus-scale
cost — even though that pass and the dat-priority bump are
analyzer-INDEPENDENT. Split: `enqueue_candidates` (per family) vs
`refresh_admission` (once per wake, the fixpoint + bump), so the prime
runs the fixpoint ONCE after all families enqueue, N×→1×. The
`sweep_absent_eligible` table IS the within-tick cache the owed note
called for. `refresh_queue` still bundles both for single-family
sweeps (`run_sweep`). Left deliberately: enqueue_unanalyzed does NOT
add the grounded predicate to its INSERT — the claim gate already
filters `resident OR eligible`, and an ungrounded-absent blob (a claim
with no route) is pathological, so the leaner-queue win is marginal
against the ordering coupling it would add; sharing the fixpoint with
the audit rollup (different cadence, different crate) stays a someday.

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
*Amendment (2026-07-16, the write audit):* done, and it split the
request path into THREE write lanes, each naming its argument. The
finding that made pooling safe: the daemon already runs many rw
connections (refiner prime + drones + jobs registry), coordinated by
row-level leases and the gc_guard, NOT a shared mutex — the `App.db`
mutex only ever serialized REQUEST-path writes against each other, and
D93's own IMMEDIATE-by-construction flip already made every
single-transaction surface atomic without it. So (1) the auth/admin
surfaces (each one IMMEDIATE transaction or an idempotent statement,
over users/sessions/invites/grants the pipeline never touches) moved
to a QUICK-WRITE pool — a login no longer queues behind a 512 MiB
dat-import; (2) `GET /v1/gc/orphans` was a pure read miscloseted on the
write mutex and moved to the read pool; (3) the PIPELINE writer keeps
the mutex, now with a SHARPER named argument than "check-then-act": its
survivors are MULTI-transaction sequences that must serialize in-
process while yielding the WAL lock between steps — dat import, ingest
(the lock releases between files by design), view eval, snapshot, and
gc keep/apply (apply reads the keep-set once then loops deleting, and
its delete-time re-verification checks unreferenced+aged but NOT
keep-marks, so a keep must not interleave an apply — the guard
serializes apply against other gc actors, the mutex serializes it
against keep). Wrapping a pipeline in one mega-transaction was rejected:
it would hold the WAL write lock for a whole import and starve the
refiner, the exact opposite of what the between-steps release buys.
The same day closed the two cosmetic D93 tails: (a) `refine:workers`
now LIVE-RELOADS — the prime owns the fleet's stop flags and re-reads
the knob each ambient tick, growing (spawn) or retiring (flag-and-exit)
drones without a restart, safe because drones are fungible (all drain
the one leased queue, at-least-once covers a retiree's unfinished
item). (b) The drone-holds-the-family-job-open lingering was reviewed
and KEPT: the job stays open precisely while that family still has
in-flight leased items a drone will return to — closing earlier is the
false-"done" race the completion gate exists to prevent. The "cross-
family" flavor (prime blocked on family F while a drone detours through
G) is bounded by one burst and costs only tray latency, not work; the
alternative (per-family drone tracking) buys nothing a restart-free
operator would notice.

## D94 — NARC interior decomposition: builtin-affine, one level down (2026-07-16)

The decomposition-arc step 3 lands as a native analyzer, `narc-split/1`.
A NARC (Nitro Archive — a NitroFS-file inside a .nds) is the SAME Nitro
filesystem the ROM container is: a BTAF (FAT), BTNF (FNT), GMIF (file
image), members as byte ranges of the GMIF data with alignment padding.
So its decomposition is the SAME coverage-map arithmetic one level down
— the nds `classify_gap`/`Piece`/`Region` machinery and the mint path
are shared verbatim (one tested `mint_decomposition` serves both
containers). Ruled: NARC recursion is **builtin-affine, no wasm** (the
open-questions NARC clause) — pure concatenation, every recipe an
`assemble@1`, so the D46 empty-import contract is untouched and no
component ships. Why it matters: two regional ROM variants that differ
only INSIDE a NARC (a localized text/graphics archive) share nothing at
the NitroFS-file boundary but almost everything at the NARC-MEMBER
boundary; decomposing the NARC recovers that dedup exactly, BEFORE CDC
(the D59 rank-7 lane) has to chew the media-stream remainder. Runs in
the refine family order after nds-split, before chunk. **Recipe-volume
gated** (`narc:max-members`, default 4096): a NARC can hold thousands of
tiny files, and past a point the claim + recipe volume outweighs the
dedup — those stay whole-archive literals (which still dedupe, and CDC
can still chew). Ambient reach is governed by the D92 eagerness policy
(NARC pieces are interior claims, not dat-named, so default `dat-named`
mode leaves them for an operator's `all`); the analyzer itself works on
any resident or grounded NARC and is exercised directly by `datboi
analyze narc`.
*Rejected:* a wasm NARC parser (it is pure byte arithmetic — a Rust
analyzer is the "moderately safe" bar, D58); recursing into NARC members
that are themselves compressed (SDAT audio, LZ overlays) — those need an
LZ codec + corrections blob, a separate wasm lane with its own ruling,
explicitly NOT attempted here; unconditional decomposition (the
recipe-volume flood a 60k-file NARC would mint).

## D95 — NixOS module: the DATBOI_* surface, dressed in options (2026-07-16)

The flake gains `nixosModules.default` (plus `overlays.default` for
`pkgs.datboi`) so a self-hoster adds datboi as a flake input and
`services.datboi.enable = true`s it. The option surface IS the daemon's
existing 12-factor `DATBOI_*` env surface — the same config the CLI and
the container image speak — dressed in NixOS-idiomatic camelCase: each
friendly option (`store`, `databaseDir`, `listenAddress`,
`nfsListenAddress`, `detectorsDir`, `refine`) owns exactly one
`DATBOI_*` var, and a freeform `environment` attrset is the escape hatch
for anything not yet promoted. One config vocabulary end to end; no new
config file format, no second source of truth. The unit runs as a static
`datboi` system user (NOT DynamicUser) because the store may sit on an
admin-managed/NFS path whose ownership must stay stable, and the D15
identity key under `databaseDir` is persistent per-instance state that
survives restarts and gets backed up out-of-band. Store and db dir are
created symmetrically via tmpfiles with the right ownership (StateDirectory
would pin the db dir to `/var/lib/datboi` and can't manage an arbitrary
store path), both land in `ReadWritePaths`, and `RequiresMountsFor`
orders start after a network store mount (D15: store may be NFS, db dir
never). Hardening is the standard sandbox set with ONE deliberate
omission: no `MemoryDenyWriteExecute` — datboi runs transform/extractor
components under wasmtime, whose JIT maps W+X pages, so the usual knob
would kill the runtime. The package defaults to this flake's build for
the host system (via `mkDefault`, overridable), so the module is turnkey
without the overlay. A `checks.<linux>.nixos-module` VM test boots the
service and asserts `/healthz` serves and both roots are owned by the
service user.
*Rejected:* a bespoke `settings.DATBOI_*`-keyed freeform-only surface
(honest to the daemon but clunky for operators who expect camelCase
options — the hybrid gives both); DynamicUser (fights the stable store
ownership and the persistent identity key); StateDirectory for the db
dir (can't manage a configurable/NFS store path, and we want the two
roots handled symmetrically); a config-file format (the daemon is env-only
by charter, docs/infra.md); opening the NFS port under `openFirewall`
(NFS is unauthenticated, D68 — never auto-exposed).

## D96 — the serve+web surface is the complete one; CLI is convenience (2026-07-16)

Posture inversion. The prior scope ruling (2026-07-11, encoded in the
`api.rs` header prose and the graduating-out-of-CLI-only language) treated
the CLI as the complete surface and the HTTP/web surface as a read-model
that *deep-links CLI instructions* for anything mutating or expensive —
"eviction, scrub, and view eval remain CLI-only." That is now reversed.
**Every capability MUST be reachable through the daemon's HTTP surface and
the web UI. The CLI SHOULD carry the same capabilities but is not required
to** — it is an operator convenience over the same daemon, not the system
of record. Rationale: the web UI is the product (docs/web-ui.md: "the best
rom manager ever," not a CAS admirer); a rom manager whose owner must drop
to a shell to *create a shelf* is not that. "The UI tells you to go run a
command" was a placeholder, not an architecture.

The binding half is correct-by-construction: **the two surfaces MUST share
one code path per capability.** The real work already lives in the library
crates (`datboi-ingest`, `datboi-catalog`, `datboi-exec`, `datboi-index`,
`datboi-formats`); an HTTP handler and a CLI subcommand are two thin callers
of the *same* library function, never two implementations of the same verb.
Where a verb's logic still sits inside an entrypoint crate (`dat fetch`'s
HTTP-fetch, `scrub`'s corpus walk, `recover`'s rebuild, the verified-write
primitive under `view sync`), it moves DOWN into a library crate before it
graduates to serve — that descent is the work, and it is the point:
divergence becomes unrepresentable when there is one function to call. The
same rule retires existing near-duplication: the audit/storage read models
that today run bespoke inline SQL in `api.rs` collapse onto the shared
`datboi-catalog` query the CLI's `audit`/`status` already call. New
long-running verbs (view eval, image mint, scrub, evict, snapshot) register
as jobs in the `jobs.rs` ledger the way ingest already does, so the UI gets
progress instead of a spinner. Every graduated endpoint follows the D69
contract mechanically (shapes in `datboi-api`, `paths.rs`+`http.rs` parity,
regenerated `openapi.json`/`schema.d.ts`).

**Breaking changes are welcomed where they make the design cleaner** — this
is a house daemon with no external API consumers to preserve; a better shape
beats a compatible one. Two capabilities are ruled explicit *operator
bootstrap* exceptions, permanently CLI-first and NOT required on the web
surface: `recover` (rebuilds the DB from the store — runs precisely when no
trustworthy server exists) and initial identity/`token` minting (the
chicken-and-egg before a session can exist; invite-accept remains the normal
in-band path). `view sync` writing to a *local* directory stays CLI-shaped
(it is inherently local filesystem I/O), but its verified-write primitive is
library code the daemon's own materialization shares.
*Rejected:* keeping the read-model/mutation split (the placeholder we are
replacing); a compatibility shim that lets serve reimplement a verb "for
now" (that is the divergence D96 exists to forbid — descent into a shared
crate is mandatory, not deferrable); exposing `recover`/bootstrap-token over
HTTP (they precede the trust the HTTP surface assumes); making the CLI
authoritative-but-mirrored (two systems of record is the disease).

## D97 — M6 iroh: our own handler over the logical CAS; the recipe graph makes transfer dedup-aware (2026-07-16)

Ratified after the M6 spike (`crates/datboi-p2p`) stood iroh up and moved
a verified blob between two instances. Four rulings, three of them
correcting older assumptions now that the real iroh 1.0 surface is known.

**Stack.** iroh **1.0.2** + iroh-blobs **0.103**. iroh 1.0 froze the v1
wire protocol (the decades-scale commitment R4 wanted); iroh-blobs stays
0.x but its *format* is fixed and identical to ours (blake3 + bao,
`.obao4` at 16 KiB groups), so the 0.x churn is API surface, not at-rest
bytes. The ed25519 instance key (D8/D15) IS the iroh `SecretKey` — one
secret, no second identity. Note the 1.0 rename `NodeId → EndpointId`,
`NodeAddr → EndpointAddr` (older docs say NodeId).

**obao reuse proven, not assumed (amends D52's "PENDING" hedge for the
p2p direction).** The spike checks an outboard built at iroh's block size
over the D52 golden input against the byte-for-byte golden the store
committed — they match. A resident blob's `.obao` (kept past eviction by
D49 rule 1) is already the exact tree iroh serves verified ranges from; we
publish to a peer computing nothing extra, and peer-supplied outboards
stay self-authenticating (D49). The two stores' outboards are one artifact.

**Fronting is our own `ProtocolHandler`, not a store trait (overturns
D14's "speak iroh-blobs' irpc store protocol").** iroh-blobs 0.103 exposes
no public store trait — the store is a concrete actor API — so datboi's
sharded loose-file CAS cannot be handed over by implementing an interface.
M6 serves the blobs protocol from OUR handler, reusing iroh-blobs' wire
`protocol` types + `get` downloader, storage backend ours. It serves the
**logical CAS (D92)**, not just resident literals: literal blobs stream
from `Store::get` (packed windows included) with the resident `.obao`;
**virtual** blobs — grounded-but-evicted, recipe-only — materialize
through the executor's verified stream (D25/D49) against the retained
`.obao`. A peer never learns our residency state; the wire surface is the
audit surface ("existence is groundedness"). D49's serve-side verify
carries over unchanged — a bad seek/recipe refuses the transfer, never
ships bad bytes.

**The recipe graph makes transfer dedup-aware — the reason M6 is ours.**
Stock iroh-blobs transfers one blake3 at a time (with free verified
partial/resume/multi-source *within* a blob) but is blind to cross-blob
structure. datboi factors ROMs into pieces (D91 sealed packs, D59 chunks,
D83/D94 interior members) that are shared across variants: MKDS USA↔EUR
share 556/564 pieces. Ruling: partial transfer reconciles the **piece /
grounding-leaf set** between peers (candidate algorithms: Rateless IBLT —
SIGCOMM 2024, the "set sketch" — or Willow-style range-based
reconciliation; choice deferred), fetches only the differing pieces as
ordinary bao blobs, and rebuilds the container from the affine `assemble`
recipe the receiver already holds. "Send me Mario Kart EUR" becomes "send
me these 8 pieces" (~1.3 MiB not ~64 MiB). Per-hash blob protocols can't
express this; our recipe graph makes it structural.

**Swarming is opt-in and tiered.** Friends plane first (D8/D34 holdings
channels over an EndpointId ACL, direct/n0 discovery, no public
advertisement). Public content discovery (pkarr/Mainline DHT announcing
served roots — "join the public iroh swarm") is a per-instance opt-in AND
honors the sensitive-blob advertisement policy flagged since D12/D26 (keys
never advertised). Stranger *mapping* trust (dat-hash → blake3 without a
signer) remains the waddup ZKP slot (D8 tier 2, M7+). Availability
announcement never implies willingness to serve sensitive content or
accept pushes.

*Rejected:* implementing an iroh-blobs store trait (none exists in 0.103 —
the D14 seam is gone); a separate p2p identity key (the snapshot key is
already an ed25519 keypair = a SecretKey); serving only resident literals
(would leak residency and contradict D92's logical-CAS line); whole-blob-
only transfer as the M6 ceiling (throws away the piece-level dedup that is
datboi's whole storage thesis); default/implicit public advertisement
(D12/D26 — a private collection must never leak to strangers by default).
*Deferred to build:* the handler itself, the reconciliation algorithm
pick, folding `datboi-p2p` into the host workspace + nix vendoring (it is
an excluded leaf today, like the wasm components), and the `datboi share` /
`fetch` operator surface (serve+web home per D96).

*Amendment (same day):* the LITERAL half of the handler landed in the
spike and is proven. `datboi_p2p::cas::CasProvider` implements iroh's
`ProtocolHandler`, reads a `GetRequest` off the wire (`Request::read_async`),
and answers `size(8 LE) ‖ encode_ranges_validated(data, obao, ranges)` —
bao-tree 0.16, the exact version and 16 KiB block iroh-blobs 0.103 links,
so the bytes are wire-identical and the STOCK iroh-blobs requester
(`store.remote().fetch`) fetches and blake3-verifies with no changes. It
reads from a real `datboi-store-fs::Store` — loose files and D91 packed
windows fall through `Store::get` transparently — reusing the on-disk
`.obao` as the tree; nothing is copied into an iroh store. Confirmed
along the way: iroh-blobs' provider fns bind the concrete `api::Store`
(no trait — the D14 seam really is gone), but the wire codec (`Request`,
`ChunkRangesSeq`) and get client are public, which is all the handler
needs. Still owed on the handler: the VIRTUAL half (grounded-but-evicted
blobs materialized through the executor, D92 — same encode, different byte
source), streaming instead of whole-blob buffering (the spike reads the
blob into memory; the fsm/async bao encoder + executor spill is the real
path for 4 GB ROMs), and hash-seq requests (offset > 0).

*Amendment (same day, 2): the VIRTUAL half landed too.* `CasProvider`
now holds `Arc<Store>` + `Arc<Mutex<Db>>` (the daemon's `!Sync`-DB sharing
pattern) and serves EVERY request through `Executor::serve_range` — the one
seam that already unifies both halves and is D49-verified: resident
literals read from the store; grounded-but-evicted blobs materialize on
demand through their recipe, verified against the `.obao4` that D49 rule 1
kept past eviction. Proven end-to-end: a blob whose literal was evicted
(a `deflate-decompress` recipe + retained outboard, nothing on disk) is
rebuilt on the fly and the STOCK iroh-blobs requester fetches and
blake3-verifies it — the peer cannot tell it wasn't resident, which IS the
D92/D97 "wire surface is the audit surface" claim, now demonstrated. Still
owed (unchanged): bounded-memory streaming — `serve_range(0, total)` still
buffers the whole blob, so the fsm/async encoder over `open_stream` + spill
is the 4 GB path; hash-seq requests; and per-request the executor rebuilds
its wasm hosts (per-connection today) — a shared engine is the seam if it
matters. `datboi-p2p` now path-depends on `datboi-exec` + `datboi-index`
(wasmtime + SQLite), so the excluded-leaf isolation is doing real work
keeping that weight off the host lockfile.

*Amendment (2026-07-17, 3): integrated — `datboi-p2p` is a daemon
subsystem now, no longer excluded.* Folded into the host workspace; iroh
joined the host `Cargo.lock` and the hermetic build DELIBERATELY (it is
core, not a spike). `datboi serve --p2p` (env `DATBOI_P2P`, opt-in, off by
default) spawns the `CasProvider` seedbox via `datboi_p2p::serve_holdings`,
bound to the DERIVED iroh key (D99 `identity.iroh_secret()`), over the
daemon's one leaked `&'static Store` and a dedicated read-only `Db` (so
serving reads never contend with the request path); a bind failure
(offline / no discovery) warns and the daemon serves locally, never
aborts. **Framing correction (D97's own earlier prose):** the exclusion
was SPIKE SCAFFOLDING to keep the churny iroh tree off the host lockfile
until the design settled — NOT the permanent-standalone fate of the wasm
components. Those stay excluded forever for a different reason (different
compile target + the D54 reproducibility boundary); they never link into
the daemon. `datboi-p2p` always did. Owed: the `datboi share` / `fetch`
operator surface and the web home (D96).

*Amendment (2026-07-17, 4): bounded-memory streaming landed* (the item
amendment 2 owed). The handler no longer buffers the whole blob: bytes
pull from `Executor::open_stream` (O(chunk) + spill) through a forward-only
`ReadAt` into the bao encoder, which writes to the wire over a
`spawn_blocking` + bounded-channel bridge (backpressure = the encoder
blocks). A 4 GB ROM streams to a peer without sitting in RAM; the encoder
still validates every chunk against the retained `.obao4` (D49). Remaining
minor: hash-seq requests, a shared wasm engine (per-connection today), and
partial/resumed ranges over-materialize (stream-from-0-and-discard) — a
`serve_range`-per-window fix if resumption traffic warrants it.

## D98 — The receive path stages partials in iroh's store; our CAS only ever ingests complete, verified blobs (2026-07-16)

Ruled before the M6 fetch path is built, because it is a re-litigable
posture. The **send** side (D97) serves from our CAS directly — no partial
state, bytes are already complete. The **receive** side is where partial
state is unavoidable: a multi-GB ROM arrives incrementally, resumably, from
possibly several peers, and iroh-blobs tracks that with a per-range
**bitfield** and a partial→complete lifecycle (its "blob store design
challenges": partial entries advance toward a verified size). Our CAS has
no such state and MUST NOT grow one: `Store` is complete-blobs-only by
invariant (D14 stage 1) — single-writer, tmp→fsync→atomic-rename, a file
either is the whole verified blob or does not exist. That invariant is
load-bearing for D15/D19/D49 (a present file is always whole and
hash-true).

Ruling: **iroh-blobs' own store is the receive staging area; our CAS
ingests only on completion.** An incoming transfer lands in an iroh-blobs
`FsStore` (its bitfield, its partial tracking, its multi-provider
resume — none of which we reimplement); when a blob completes and
verifies, it is imported into our CAS with the house discipline
(`put_with_obao` — one atomic publish, reusing the `.obao4` iroh already
built, since bao-tree 0.16 makes it byte-identical, D97). Clean division:
**iroh's store owns "in flight," our CAS owns "durable and grounded."**
Our complete-blobs-only invariant survives untouched, and we get resumable
multi-source fetch for free. The staging store is a disposable cache (D15
tier — nukeable, rebuildable by re-fetching), never authoritative; a crash
mid-transfer loses only progress, never a claimed blob.

Corollaries. (1) **Piece-set reconciliation composes with this** (D97): the
differing pieces are just small blobs fetched into the same staging store,
then imported and fed to the local `assemble` recipe — the receive path
doesn't special-case pieces. (2) The import step is the natural home for
the receive-side D4/D49 verification and for minting alias claims on newly
arrived bytes (D22), so a fetched blob enters the corpus exactly as an
ingested one does — one ingest seam, not two. (3) On-disk cost of
double-writing (staging store then CAS import) is accepted; reflink import
where the staging store shares a filesystem is the optimization, not the
contract.

*Rejected:* teaching our CAS partial-blob state + bitfields (reimplements
iroh's receive machinery inside the one crate that must stay simplest and
most durable, and punctures the complete-blobs-only invariant D15/D19/D49
lean on); fetching straight into `data/` with a sidecar bitfield (same
invariant breach, plus a partial file under a hash-true name is a lie the
recovery scan would have to special-case); making the staging store
authoritative (it is a cache — D15 forbids sole truth in a nukeable
store).

## D99 — Instance identity: one root secret, purpose-derived keys, never double duty (2026-07-17)

Refines D8's "the server identity keypair doubles as the iroh key." Handing
ONE ed25519 key to snapshot-signing AND iroh's handshake is exactly the
cross-protocol key reuse that makes key confusion possible (a signature
minted in one context carrying meaning in another). Ruled instead: the
on-disk `identity.key` holds a 32-byte **root secret** that **signs and
authenticates nothing directly**; every protocol key is a domain-separated
derivation via `blake3::derive_key(context, root)` → ed25519. Two today —
`snapshot-signing` (D43 recovery root) and `iroh-identity` (the iroh
`SecretKey`, whose public half is the EndpointId peers ACL, D8) — each a
distinct, unrelated key, so a signature or handshake in one plane can never
mint or verify anything in the other. Future uses get their own context
label; the root gains no new powers. blake3's KDF mode (already a
dependency, unique version-scoped context strings) is purpose-built for
this.

Storage stands where D15 already put it: a **separate `0600` file** beside
the DB, **never in `state.db`**. Recovery *nukes* `state.db` and rebuilds
it from CAS — the root must survive precisely that event, or every past
snapshot's authenticity and the whole p2p identity evaporate on a DB
rebuild. A secret also wants file perms + independent backup, and belongs
nowhere near the cache-tier DB (D37 boundary). `load_or_create_identity`
already generates + persists it 0600 on first run; **autogenerated** stands.

Consequence: the snapshot signer is now a derived key, so its public key
and the golden signature bytes moved — updated the one pinned vector; D43
already holds snapshot identity stability is not sacred. The root's raw
ed25519 public key is no longer meaningful anywhere (it signs nothing);
"the instance's identity" is now two public keys with distinct jobs
(snapshot verifying key, iroh EndpointId), both stable derivations of one
backed-up secret.

*Rejected:* one shared ed25519 key across snapshot + iroh (the D8 default —
the confusion this exists to foreclose); the root doubling as the
snapshot signer while only iroh derives (smaller change, but leaves one key
doing signer + KDF-seed duty — under a stated fear of confusion, the clean
"root signs nothing" invariant is worth the golden churn, free pre-corpus);
keeping the secret in `state.db` (lost on the recovery it must outlive).

## D100 — Reconciliation: reconcile the plans, fetch the parts; the rateless IBLT is ours (2026-07-17)

The D97 dedup-aware transfer lands as a second `ProtocolHandler` on a
datboi ALPN (`datboi/recon/1`) beside the blobs seedbox. Four rulings.

**The reconciled set is the RECIPE set, not the piece set.** The design
pass overturned the working title: v1's one scope is the meta-blob hashes
of non-Failed builtin `assemble@1` rows (the D63/D91 affine class — the
recipes decomposition mints). Reconciling pieces directly is strictly
dominated: recipes run ~1 per container vs ~10²–10³ pieces per container,
and once the initiator holds a peer's recipe, its missing pieces are a
LOCAL closure walk (inputs ∉ my grounded set, recursing through usable
local routes) — no second exchange. The fetched diff is still the D91
pieces; the reconciled set is the plans that name them. Recipes crossing
the trust boundary is not new surface — it is D8's "recipe claims from
friends" made concrete, and `index_recipe` already models it
(referenced-but-missing Absent rows, `RecipeSource::Peer`). Whole-ROM
holdings stay the D34 channel's coarser layer.

**Algorithm: rateless IBLT (SIGCOMM 2024), our own port.** Research
settled that no viable Rust implementation exists (the lone crate is an
O(d²) PoC), so `datboi_p2p::riblt` is a ~500-LOC port of the reference Go
implementation: `[u8;32]`-specialized symbols, SipHash-2-4 keyed checksum
(fixed protocol constants), the mapping-heap coding window, and the
streaming decoder with its decodable queue. The correctness proof is
DIFFERENTIAL: the Go reference (vendored, MIT) plus a generator produce
committed golden vectors — encoder output checked byte-for-byte, decoder
cases checked for exact diff recovery and symbol count — so our port is
pinned to the paper's artifact, not to our reading of it. Why not
range-based (Willow-style): multi-round refinement pays relay-path RTTs
per level and O(d·log n) comms, and needs an ordered domain; rateless
IBLT is one round, ~1.35×d symbols independent of corpus size from d=1
to millions, no tuning, adversary-robust via the keyed checksum — and the
responder is a stateless incremental stream ("send until told to stop"),
which is exactly QUIC's shape. The codec sits behind a 1-byte scope tag;
if real corpora ever embarrass the constant, swapping algorithms is a
protocol rev, not an architecture change.

**Asymmetric reveal is the privacy design.** The responder streams coded
symbols of ITS scope; the initiator decodes against a local prior that
NEVER crosses the wire and reveals only the scope request plus a stop
signal (a bound on the diff size). So reconciliation exposes the
responder's recipe inventory and nothing of the initiator's — the
open-questions manifest-privacy worry lands as: the party that answers is
the party that consents. Acceptable today because the recon ALPN sits
behind the `--p2p` opt-in and an unlisted EndpointId (knowing it is the
capability, the D8 friends plane); an explicit ACL is owed before any
discovery/advertisement tier ever turns on (open-questions, with the
swarm tiers).

**Flow, wire, and observability.** Wire: request = 1-byte scope tag;
response = `u64 LE` set size then a stream of 48-byte coded symbols
(32 XOR-sum ‖ 8 SipHash-sum LE ‖ 8 count i64 LE), batched, responder
checking for the initiator's stop byte between batches, with a
responder-side symbol cap against drain — hand-rolled fixed binary (D19
register; a fixed-width symbol stream gains nothing from CBOR). Sync
flow: reconcile recipes → fetch missing recipe blobs over the blobs ALPN
(CasProvider learns to serve Meta; bytes verify against their own hash) →
`index_recipe` as `source=Peer`, born `Pending` — the D4/D8 lazy-verify
posture: Pending grounds nothing for audit or eviction (conservative by
construction; audit visibility for never-rebuilt claims is D34
available-from-peer territory, not a grounding hack), a lying recipe
wastes replay CPU and poisons itself at rebuild, never bad bytes → local
closure walk for missing leaves → fetch them as ordinary bao blobs into
iroh staging (D98) and import (`put_with_obao`, Resident) → wants
materialize through the executor (replay verifies, Pending→ReplayedLocal).
An empty want-list is mirror mode (fetch the whole diff — the D34
full-mirror subscriber policy, explicit never default). Savings
observability ships with it, not after (the D97 requirement): named
numeric tracing fields — reconcile summary (set sizes, symbols received,
diff sizes, overhead ratio vs the ~d minimum, wire bytes) and sync
summary (pieces/bytes fetched, recipes fetched, bytes rebuilt, bytes
already held, savings pct), INFO at completion, DEBUG per piece (D81).

*Rejected:* reconciling the piece set (dominated — larger set, no plan,
same fetch); an existing Rust IBLT crate (none viable — O(d²) PoC, no
proof); range-based reconciliation as v1 (kept as the named fallback
behind the scope tag); a plain manifest-listing mode (an empty-prior
decode costs only ~1.35–2× a plain listing — one code path until
fresh-mirror traffic proves the constant matters); CBOR coded symbols
(fixed-width stream, D19 register); blocking the friends plane on ACL
machinery (the gate belongs on advertisement, not on capability-addressed
friends); trusting peer recipes as `Verified` on arrival (grounding must
not inflate on unreplayed claims — Pending is the honest state and the
audit already has a vocabulary for the rest).

*Amendment (same day): the responder encodes off a sqlite snapshot, not
a resident set.* The incremental encoder must keep every source symbol
live (coded symbol 0 sums the ENTIRE set), so the responder's original
shape — materialize the scope into a `Vec`, build the in-memory encoder
— cost ~72 B/element steady plus construction transients: ~1.4 GB peak
at a 10 M-recipe corpus, PER STREAM, on an unauthenticated-beyond-the-
EndpointId surface. The fix is the observation that the encoder never
needed the set resident — it needs a RE-ITERABLE, STABLE view: coded
symbols [m, m+k) are computable in one pass over the set by replaying
each symbol's index mapping from zero (O(k) memory, one scan per
block). So `riblt` gains a `SetSnapshot` trait (contract: every pass
yields the same distinct elements) and `encode_block`; the responder
implements the trait as a sqlite CURSOR — a dedicated read-only
connection per stream holding one read transaction across all passes,
which makes both contract halves free: WAL snapshot isolation IS the
stability (writers proceed; this stream sees one frozen set), and the
scope query is structurally DISTINCT (one recipe row per meta blob,
unique hashes). Blocks grow exponentially (1024 → ×2 → cap 131 072), so
the common small-diff case is ONE scan and ~48 KiB of coded state, and
a full-mirror drain is O(log) scans, never O(n/block). The wire stream
is byte-identical (no ALPN rev): a differential property pins
block-encode == incremental-encode on arbitrary sets and cuts, and the
goldens pin both to the Go reference. The incremental `Encoder` stays —
it feeds the decoder's windows and the differential tests. Accepted
asymmetry: the INITIATOR'S decoder prior remains O(n) resident
(~72 B/element) — that memory is spent by the party choosing to sync,
on its own box, which is the right party to pay it.
*Rejected (amendment):* a `PRAGMA data_version` change-detector instead
of a held read transaction (any unrelated write — an ingest mid-stream —
aborts every pass; the held snapshot costs only pinned WAL checkpointing
for the stream's bounded life); caching per-symbol mapping state across
passes (that cache is exactly the O(n) being removed).

*Amendment (same day): the codec is const-generic over symbol width.*
The riblt algorithm is width-agnostic — peeling is XOR algebra over a
fixed-length domain, so the one shape it resists is per-ELEMENT variable
width (padding to max plus in-band lengths buys the complexity and none
of the savings). What future scopes need is per-SCOPE width: sha1-shaped
sets, or sha1‖blake3 alias pairs (52 bytes) for dat gap-fill, where
hashing the pair down to 32 would decode to a digest of the answer
instead of the answer. So `riblt` takes `const N: usize` throughout;
`N = 32` remains the only wire width and the only reference-pinned
instantiation (the goldens prove the refactor changed zero wire bytes);
wire encode/decode went slice-shaped because stable Rust cannot spell
`[u8; N + 16]`. A width-genericity test re-proves exact diff recovery,
wire round-trip, and block==incremental at 20 and 52. Rules for a new
width becoming protocol surface: it MUST commit its own goldens from
the (generic) Go reference first, and its symbol must be a
collision-free identity for the element — reconciliation is set algebra
over symbol values, so a colliding symbol (a bare crc32 over a big
corpus) silently merges distinct elements and duplicates break peeling
outright. SipHash keys stay shared across widths (streams never mix;
per-width keys are one more thing to get wrong).
*Rejected (amendment):* per-element variable width (fights the
algebra); widening only when the first non-32 scope lands (the refactor
is mechanical now and load-bearing to the scope-API design being ruled
next); per-width SipHash keys.

## D101 — The p2p operator surface: sync is a job, the seedbox endpoint is the identity (2026-07-17)

D96 (serve+web is the complete surface) meets D100 (the sync engine).
`POST /v1/p2p/sync` `{peer, wants[]}` starts a `sync` job — a new D74
ledger kind, the additive code the ledger was designed for — that runs
`datboi_p2p::sync::sync` on the daemon's runtime over a PRIVATE write
connection (the D71/D96 posture: minutes of network never hold the
pipeline mutex), then relinks + refreshes rollups so fetched content
lights the shelf exactly like an ingest. Empty/absent `wants` is mirror
mode, as in D100. The savings summary (D97) rides `JobDetail` as
STRUCTURED wire data — a `sync` object carrying bytes fetched / rebuilt /
already-held / savings pct — numbers the web renders in the viewer's
locale, never server-composed prose. `GET /v1/p2p` answers
`{enabled, endpoint_id}` so the web can say "share this id" without the
operator grepping the daemon log.

**Outbound rides the seedbox's own endpoint.** One iroh identity per
daemon (D99): the sync initiator connects FROM the endpoint the seedbox
serves on, so the responder sees the friend key a future recon ACL will
check, and no second endpoint fights the first for the discovery record.
Accepted consequence: `POST /v1/p2p/sync` on a daemon without `--p2p` is
a clean 503 — an outbound-only lane would need its own identity story
for a case the CLI already covers.

**`datboi fetch --peer <id> [want…]`** is the D96 convenience lane:
direct library call over the local store/db under an EPHEMERAL endpoint
key — deliberately not the derived key, because a `--p2p` daemon may be
live on it and two publishers under one key corrupt the discovery
record. When recon ACLs land, the CLI defers to the daemon API (the
friend key lives there).

**The web home is the Ingest screen.** Fetching from a friend is
acquisition — bytes in — so the peer-fetch card sits beside the
drop-zone (one canonical home, web-ui.md; a "P2P" nav tab would be a
CAS-admirer surface). The job receipt is the persona moment: "1.3 MiB
fetched, 62.7 MiB rebuilt from shared pieces — 98% saved".

*Rejected:* a synchronous sync endpoint (a network-length request);
binding a fresh outbound endpoint per daemon job (works, but the friend
key IS the coming ACL story and the seedbox already holds it); savings
as a report note (prose freezes numbers away from the UI and the
translator); a dedicated nav tab; requiring explicit wants (mirror mode
is D100's subscriber shape, and the fetch card's default "everything
they have that I lack" is the honest reading of a friend link).

## D102 — Mirror completeness is a roots scope; channels stay the naming layer (2026-07-17)

Resolves the protocol-completeness gate the use-case audit raised (p2p.md
§ Use-case coverage audit): friend mirror was blind to exactly the
content nothing has decomposed — never-analyzed loose ROMs, D24
preflate-refused containers — because the one recon scope advertises
plans and plan-less blobs are invisible.

**The ruling: completeness is two planes, not one layer.** Mirror
("everything you share") is a HASH-SET question, and it stays on the
recon plane: a second scope, `RootBlobs` (wire byte 1), over the
responder's resident Data-namespace blobs with **no non-Failed producing
route** — the ur-literals. That set is the minimal cover: every held
blob is either underived (in the roots scope, fetched whole) or derived
(reachable from an advertised plan, grounded by the closure walk), so
the Ingest card's "fetches everything they share that you lack" becomes
true BY CONSTRUCTION, not by effort. The structural bonus: a young
library is nearly all roots; as analysis decomposes it, blobs migrate
out of the roots scope and under plan coverage — the audit's
invisibility class shrinks to zero by definition. The D34 holdings
channels remain owed, but as what they are: the NAMING/DISCOVERY layer
for entry-shaped journeys (dat gap-fill's entry→blake3 translation,
curated-view subscription) — journeys recon cannot serve by
construction, in either scope. They are not mirror's completeness
dependency, so mirror does not wait on the channel design or the recon
ACL it is gated behind.

**Mechanics.** Mirror mode reconciles both scopes over the same recon
connection (the responder's per-stream snapshot shape generalizes — the
scope enum picks the query); remote-only roots join the walk roots,
where the walk is already the dedup filter: a "root" the initiator
holds, or can derive via its own routes, resolves Supported and fetches
nothing — so the initiator's prior for the roots recon is simply its own
roots set, and spurious diff entries (peer roots we hold as non-roots)
cost index reads, not wire bytes. Wants mode is untouched — explicit
hashes never needed a scope. Fetched roots count as fetched leaves in
the `SyncReport`; sketch bytes/symbols sum across the two reconciles (no
API shape change).

**The soundness invariant, stated.** "No producing route" (rather than
"no groundable route") is the honest minimal cover only while every
non-Failed route's inputs are locally groundable — true in the additive
v1 world because decomposition mints plans over pieces it stores at mint
time, and ReplayedLocal is the only license to drop a literal (D25:
EvictedCovered blobs are non-resident, excluded from roots, and their
covering route replays from held inputs). Real eviction work must
revisit the roots query alongside CasProvider's serve-the-derivable
story (both halves of "advertised but unservable" — the walk's
`pieces_unavailable` deferral is the runtime backstop either way).

*Rejected:* holdings channels as the mirror completeness layer (wrong
plane — couples a structural set question to a publication/curation
surface that doesn't exist yet, is gated behind the recon ACL, and
needs recon for dedup transfer anyway: a dependency added, not saved);
advertising the full resident set (destroys the point of reconciling
plans — pieces AND containers in one scope balloons every diff);
widening the recipe scope to opaque recipes (re-rejected from the
audit: their outputs re-derive locally from covered inputs); a
groundability-checked roots query (one-level input checks false-root
chained derivations — aggregates — and full transitive grounding in SQL
re-solves the walk on the responder; the invariant above makes the
cheap query the correct one).
