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
M5 (p2p) → M6+ (frontier). Full definition in 90-roadmap.md. *Rejected:*
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
from. The `revision` is the crate source's nix store hash (`src:…`),
NOT a git rev: content-scoped, so unrelated repo commits cannot churn
component bytes and any commit with the crate unchanged reproduces the
artifact byte-exactly. **Isolation**: each transform is a standalone
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
refusal gate. *Rejected:* git-rev stamping (per-commit churn breaks
reproduce-from-any-commit), warning instead of refusing (a warning is
policy nobody reads; the corpus lives forever), one shared workspace
with canonical-at-mint bytes (tolerable but makes "reproducible"
mean "from one blessed commit only").
