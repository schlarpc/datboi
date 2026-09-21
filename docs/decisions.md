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

*Amendment (2026-07-07, ruled; recorded here 2026-07-17 during the
open-questions condense):* the M1 NFS benchmark is **indefinitely
deferred** — a local-SSD run cannot answer what the bench gates (NFS
metadata round-trips are the whole case for aggregation and the fanout
freeze), so no bench until the NFS machine exists. Accepted
consequences: the 2-level×256 fanout is frozen-by-default at first
real corpus; aggregation stays available later as an additive layer;
the recovery walk stays at 8 workers. A local scale-smoke (50k blobs,
MAME-ish size histogram) DID run to catch algorithmic pathologies in
our own code: ingest is linear (~890 files/s, fsync-per-blob
dominated, as designed); recovery was SQLite-autocommit-bound, fixed
by batching the rebuild passes in transactions — fast recover 13.7 s →
2.5 s per 50k (~20k blobs/s ⇒ the DB side of a 10M-blob recovery
≈ 8 min; the NFS walk then dominates, which is the part the deferred
bench would tune).

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

*Amendment (2026-09-21, the deferred verify landed):* the decompressing
verify this entry deferred now exists as the `chd-verify` analyzer, and
the grading it gates changes accordingly. **A CHD whose every hunk this
build decompressed, and whose logical data hashes to what the header
declares, links at `BASIS_SHA1` — have-verified.** That is not a
softening of the ruling, it is the ruling's own exit condition: the
grade rises because the evidence changed from an attestation we read to
a digest we computed over bytes we produced ourselves. Everything else
about D44 stands. The declared alias namespace is untouched, still
answering nothing but disk claims at `BASIS_DECLARED`; a CHD nobody has
verified yet is still `probable`; and a truncated CHD with an intact
header still fails, now for the reason the entry named — the hunks are
not there.

The mechanism is a second alias namespace, `AliasAlgo::ChdSha1Verified`,
written only by the analyzer and only after a full decompression. Two
namespaces rather than one column: the *claim* a header makes and the
*conclusion* a verify reached are different facts about different
evidence, and a shared row would make "who said this" a matter of
reading a flag correctly. Neither namespace ever answers a real sha1
lookup (the digest describes decompressed content, not the blob), which
is the invariant this entry set and the new one inherits verbatim.
`link_identities_to_blobs` now upserts the basis to the MAXIMUM of what
it has and what it found, so a re-link cannot silently demote a
verified CHD back to probable — the old `INSERT OR IGNORE` would have
frozen whichever grade got there first.

Three consequences, ruled explicitly.

**v1–v4 parse.** The legacy headers and hunk maps are read now, so those
files stop being opaque bytes with a note. What they get is honesty
about what each version declares, not a promotion: v4 carries the
combined raw+metadata sha1 dats reference and behaves exactly like v5;
v3's `sha1` field covers the RAW data only, and is recorded as the
declared disk digest because that is what a v3-era dat listed; **v1 and
v2 declare an md5 and no sha1 at all**, so they are parsed, verified,
reported — and then held as ordinary literals with no disk-claim alias,
because there is no sha1 they could answer with and offering their md5
in a sha1's place is precisely the lie this entry forbids.

**Partial verification is not verification.** The analyzer refuses a
file *before* decompressing anything if any hunk in its map needs a
codec this build lacks (`avhu`, today), or if it is a delta against a
parent. `zlib`, `lzma`, `huff`, `flac`, `cdzl`, `cdlz`, `cdfl` and
uncompressed hunks are all decoded. A refusal is a `Negative` naming
the codec, not an `Err` (D81): the file will not become decodable by
retrying, only by a new analyzer version — which is exactly the event
the fixpoint is built on (D45).

**Already-ingested CHDs are swept, not re-ingested.** A new analyzer
identity means every blob is unanalyzed for it (D45), so the existing
corpus is re-covered by the ordinary ambient sweep with no migration,
no re-scan and no operator step. The alternative — verifying at ingest
— was rejected on D45's own terms: a full decompression of 522 GB is
the most expensive thing in the pipeline and belongs in the background
by construction.

*Rejected:* a `verified` boolean on the existing `identity_blob` row
(the same shared-row confusion, one indirection later, and it would
make the grade depend on reading a flag rather than on which namespace
the evidence lives in); trusting the map's per-hunk CRCs as a cheap
"verify" (they are written by the same tool as the sha1 — the identical
self-attestation this entry refused, at a weaker checksum; they are
checked, but as corruption detection *inside* a real verify, never as a
substitute for one); promoting v1/v2 by hashing their decompressed data
and offering the result as a sha1 (nothing in the file attests to it,
so it is our number, not the dumper's, and it answers no claim anyone
makes); linking the COMPUTED digest when it disagrees with the header
(tempting — the content genuinely is whatever it hashes to — but a
file that contradicts itself is damage, the disagreeing content
matches no dat in practice, and laundering it into a have on our own
authority is the inverse of what this entry ruled; the mismatch is
reported and the link stays `probable`).

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
zlib and fully covered.

*Amendment (2026-09-21, D126 — the gap is wider than "a clean error"):*
the sentence above is accurate for the SPLIT half and was wrong about
the whole. preflate-rs errors cleanly at split time, `SplitReader::fail`
catches it, the negative is recorded — all as written. But a different
input class reaches `recreate` and PANICS inside the guest
(`preflate-rs-0.7.6/src/tree_predictor.rs:169`, observed on the live
corpus), which wasmtime turns into a TRAP, not an error. Two things
follow. The coverage gap is not only "some containers stay literal": it
also produces recipes that were minted (the split's own
`verify_compression` pass having recompressed and compared every window)
and that nonetheless cannot be rebuilt — a split-verify / rebuild
divergence inside preflate-rs 0.7.6, which is the real defect and is
upstream's. And the SECOND failure had no record at all until D126:
until then the trap was treated as environmental, so the route was
retried forever rather than poisoned. D126 rules what the trap means and
where it is written down; this entry's "optimization issue" framing
still holds for the split-time gap only. *Rejected:* zlib-exact compressor components
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

*Amendment (2026-09-20): a floor only where a floor exists — non-affine
derives bless.* D63's cost objection to mandatory blessing was aimed at
affine routes over verified inputs, where the carve-out already
guarantees every served byte and an output bao buys nothing but a full
pass over a TB-scale image. It does not reach `deflate-decompress@1`
zip members, and reading it that broadly left most ROMs inside zips
unreadable through a view. Measured on the live deployment: 152,014
deflate members, 107,090 of them over 16 KiB and therefore with neither
a sidecar nor a carve-out — `serve_range` returned `MissingOutboard`,
the daemon 500, the NFS client `NFS3ERR_IO`, and the client saw an
empty read. `mame -verifyroms` over the mount passed 5,699 machines
where our own audit says 27,975 are complete. Members at or under one
chunk group have an empty outboard by construction and worked, which is
what hid it. Two things are different about these routes. (1) **There
is no floor to be a ceiling over**: the route is not affine, nothing
qualifies for the carve-out, and the output bao is not a promotion —
it is the only way to serve the bytes at all. (2) **The pass D63
refused to pay is already being paid**: `hash_member` inflates every
DEFLATE member in full to compute its alias tuple (D2), so the tree is
one more hasher on bytes already streaming past, not a second pass.
Ingest therefore computes the output outboard in the same inflate that
computes the tuple and stores the sidecar beside the absent member
(~len/256, ~0.4% of content — 143.0 GB of members costs ~560 MB of
trees), checking the obao root against the tuple's blake3 on the way:
the same value computed two ways, so the check is free. STORED members
are untouched — the carve-out covers them and D63's cost objection
still stands there. For members already ingested, which no ingest
change reaches retroactively, `serve_range` blesses ON DEMAND where it
used to return `MissingOutboard`: the same "cheap and one-time" shape
as the lazy `ensure_obao` on literals, under a per-hash single-flight
so a client's parallel readahead cannot turn one cold read into sixteen
materializations of the same blob. No size cap — a cap turns "the first
read is slow" back into "the bytes are unreadable", which is the bug.
A blessing failure stays an error: D49's never-bad-bytes rule forbids
the soft fallback of slicing an unverified spill. *Rejected:* blessing
STORED members too (the carve-out already guarantees those bytes;
D63's objection is exactly on point there); deferring the on-demand
bless to a background pass and failing the read meanwhile (leaves the
corpus unreadable until a sweep that does not exist); a size cap on
on-demand blessing (permanent unreadability for the biggest members —
the same failure in a nicer wrapper).

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

*Amendment (2026-09-06, D111 — generated inputs):* a rebuild input
with a non-failed ZERO-INPUT route (the XGD1 filler stream) is
`generated`: it costs nothing to hold, ever, so it is NEVER packed —
its route regenerates it — and it sits outside the sharing fraction,
which now measures only the bytes that would need packing. It carries
a second trigger instead: when the generated bytes are at least
`swap:generated-min-pct` of the CONTAINER (molten, default 25), the
swap fires regardless of sharing.
The original predicate protected against paying an inode-and-IO bill
for pad savings on a lone ROM; a lone seed-era Xbox disc is the
opposite economy — evidence in hand from the first real walk (Halo:
Combat Evolved v1.09, the rc4-era twin of the seed-era fixture): the
game partition is 3,622,736 sectors of which 1,595,642 (44%) are
filler and 256,992 (7%) are zero; on a seed-era master those 44% are
regenerated from four bytes, so the swap writes the 56% of data
pieces once and reclaims ~3.3 GB per disc. 25% clears that with
margin and never touches an NDS ROM (generated = 0). The synthetic
gate regenerates 97% of its bytes and swaps on this trigger alone
(a lone disc shares nothing), packing the pieces and not the filler.

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

## D103 — recon/2: postcard envelopes, raw frames; the scope registry is an enum (2026-07-17)

DESIGNED, NOT BUILT — the ruling lands before the code (house rule);
`datboi/recon/1` stays the live wire until recon/2 is implemented.

The scope surface is growing on three axes — more blake3 sets, sets
parameterized by an argument ("blobs in dat X", "inputs of recipe Y"),
and sets in other hash algebras (sha1-shaped, sha1‖blake3 alias pairs
for dat gap-fill) — and recon/1's one-byte request can't carry
arguments, while its bare-stream response can't distinguish "I don't
speak that scope" or "I don't have that dat" from a dead wire. Rulings:

**Each layer sits in its encoding register.** The codebase has three
earned registers, now stated as a rule: identity bytes (CAS object
encodings) are hand-controlled canonical forms a macro must never own
(D18/D69); operator/control surfaces are typed-and-negotiable (D69's
REST); homogeneous machine-to-machine record streams are hand-rolled
fixed binary (D100). A wire protocol's *envelope* — request, response
header — is control-plane data: heterogeneous, evolvable, tiny. It gets
a serialized struct. The *payload* — the coded-symbol stream — is a
record stream and stays raw. This is not REST-here-binary-there
inconsistency; it is the same envelope/stream split iroh-blobs itself
makes (postcard requests, raw verified byte streams).

**The wire.** Request (initiator→responder): a length-prefixed postcard
message — an enum with one variant per scope, payloads where scopes
take arguments. The length prefix is required because the initiator's
send half stays open for the stop signal, so FIN cannot frame the
request. Response (responder→initiator): a length-prefixed postcard
header — `Accepted { set_size, frame_len }` or `Refused { code }` —
then the raw coded-symbol stream exactly as today (fixed-width records,
every byte-string parses, stop byte between batches, drain cap;
goldens untouched). `frame_len` is redundant with the scope's protocol
constant but costs ~2 bytes and lets a dumb tool skip a stream it
doesn't understand. Errors are HEADER-TIME ONLY: mid-stream failure
stays a QUIC stream reset — in-band trailers would need escape
sequences inside the frame stream, destroying the every-record-parses
property. The initiator's convergence budget already bounds a lying
stream. The stop signal stays a raw byte.

**Postcard, not CBOR.** This is a Rust↔Rust friends-plane protocol;
CBOR's win is cross-language self-description nothing consumes, and
postcard matches the protocol family we embed in (iroh-blobs' own
envelopes) at the smallest dependency cost.

**D69's derive scoping, refined.** The serde-derive ban exists because
identity bytes must never be macro-owned. A VERSIONED WIRE ENVELOPE is
the same category as the REST API — negotiable surface, not identity —
so serde+postcard derives are allowed in `datboi-p2p`, scoped to the
envelope module; coded-symbol records and CAS encodings stay
hand-rolled. (`datboi-api` remains the only crate with API-shape
derives; this admits wire envelopes, not a second API surface.)

**The scope registry stays a closed enum in one file.** The request
enum IS the registry: variants are append-only, never renumbered, each
declaring its argument shape and its symbol width as protocol
constants (one width per stream — the D100 amendment; a non-32 width
owes its own goldens before becoming surface). The symmetric-prior
convention is now stated: a scope is ONE set definition evaluated on
both databases — the responder advertises it, the initiator runs the
same query locally as its decoder prior. That symmetry is what makes
the diff meaningful; a scope wanting an asymmetric prior is a design
smell to stop at (D102's roots scope bends it knowingly — the walk
mops up the asymmetry).

**One rev, everything batched.** Request envelope, response header, and
the D102 scopes ship together as `datboi/recon/2`; recon/1 is DELETED,
not maintained (the peer population is one operator). ALPN remains the
only version negotiation.

*Rejected:* CBOR envelopes (self-description without a consumer);
an RPC framework (the exchange is "stream until told to stop" — QUIC
bi-streams natively are that call model; a framework would re-multiplex
QUIC inside QUIC and fight the cancel semantics); serializing the
symbol stream (re-rejected from D100 — per-record overhead, and a
parse-failure path per record where today none can exist); in-band
error trailers (escape sequences kill every-record-parses); a bare
status byte instead of the postcard header (requests were growing
arguments anyway; one envelope register, not two); serde-free manual
envelope impls (the D69 refinement is the honest ruling, not a
workaround); keeping recon/1 alive beside recon/2 (nothing speaks it
but us).

*Amendment (same day):* the new wire ships as `datboi/recon/1`, not
`/2`. ALPN version numbers exist to disambiguate across a deployed
peer population, and the original recon/1 wire never had one — the
peer population is one operator's machines in lockstep behind an
unreleased `--p2p` opt-in, and the ruling above already deletes the
old wire in the same rev, so there is no window where two formats
coexist. Burning `/1` on a format nothing external ever spoke would
misname the first real protocol rev forever. Everything else stands
unchanged; the NEXT incompatible change is `/2`.

## D104 — Web ingest surface: one content-classified drop, REST + polling, HTTP custody is copy (2026-07-11; recorded 2026-07-17)

Retroactive recording — these were ruled and shipped 2026-07-11 during
the M5 web sessions and lived only in open-questions.md until the
condense. The CLI-only-mutations posture they first punched holes in
was later inverted wholesale by D96; what remains load-bearing here is
the ingest-surface design itself.

**One drop surface, content-classified.** Users don't route bytes to
the right upload box; the ingest job classifies every staged file by
content (the house philosophy: magic bytes and `datboi_formats::detect`
— names never decide). A file whose head detects as a dat imports via
`import_dat`; a zip whose central directory names EXACTLY one member
whose head detects as a dat imports that member (extraction bounded by
the declared size, riding the D35 walker — a multi-member zip is a ROM
container by construction and is never sniffed further); everything
else runs the pipeline unchanged. Pipeline counters stay pure: a dat
import is not a `files_scanned`; the report carries a separate
`dats_imported` lane. Both the Ingest screen and the Library screen's
dat card ride the same staged flow.

**Transport is REST + polling.** Staged uploads
(`POST /v1/ingest/uploads` streaming raw bytes, no multipart, headroom
guard instead of a body cap) return in-memory tokens;
`POST /v1/ingest` spends them all-or-nothing into a background job.
Upload progress is the browser's own XHR meter and server-side events
are file-granular, so SSE/WebSockets buy nothing today — the tray
polls 2 s while running, the screen 1 s on its job. The upgrade path
(SSE over the bounded-mpsc pattern, per-byte progress via the D71
Pulse trait) is named, not built.

**Custody over HTTP is always copy.** The browser cannot move
originals; NAS-local ingest (move/reflink custody) stays CLI. Report
paths wear the client's original names; staging paths never leak.

*Rejected:* multipart uploads (one file IS the request); separate
dat-vs-rom upload endpoints/boxes (users shouldn't need to know, and
zipped dats — how No-Intro/Redump actually ship — fit neither box);
name-based classification (names never decide); SSE/WS for
file-granular progress (complexity with nothing to carry).

## D105 — Pack format v2: the outboard rides the pack, derived not described (2026-07-17)

Rules both pack-format reconsiderations the M6-spike review flagged
under D91 (outboard-in-pack, footer integrity) as one format revision:

```text
[member bytes…]        back-to-back from 0, coverage order
[obao section]         member-rooted obao4 trees, member order
[footer: b"datboi/pack/1\n", u32 count, rows of (32B blake3, u64 offset, u64 len)]
[trailer: 32B blake3(footer), u64 footer_len, b"DBOIPACK"]
```

**The outboard is a byproduct of verification the write already
does.** `put_pack` streams every member byte through a hasher to prove
its identity; the bao root IS the blake3 hash (the D52 golden pins
this), so that hasher becomes `obao::compute` and each member's tree
falls out of the write for free. The D91 amendment's post-pack bless
loop is deleted (it re-read everything just written), the lazy
`ensure_obao` backstop for recovery-restored packs dies (`scan_packs`
derives the tree locations from the footer), and a footer that
references a tree disagreeing with the member bytes is structurally
impossible — one pass produces bytes, proof, and tree together.
Outboard inodes for packed members go to zero: the chunk-pack phase
now drops BOTH redundant loose files (`.data` and `.obao4`), and the
swap phase writes no loose sidecars at all. The write spools the
section to a staging file rather than RAM (packs may reach disc-image
scale; ~0.4% of 100 GiB is 400 MB better not held).

**Derived, not described.** The footer rows are byte-identical to
v1 — the obao section adds ZERO fields. Everything about it is
computed: trees sit in member order starting at the last member's
end; each tree's length is `outboard_size(len)`; small members
(≤ 16 KiB, empty outboard) contribute nothing — absence derived,
exactly the loose-sidecar rule. The parser ENFORCES the derivation
(member offsets are the prefix sum of lengths from zero; data +
section + footer + trailer tile the file exactly), so redundancy that
could disagree with reality doesn't exist to disagree. This couples
the pack format to obao4 permanently — cheap: D52 froze obao4 first,
and any future change is a new magic anyway.

**Footer integrity: blake3(footer) in the trailer.** The pack's
filename already commits to every byte (whole-file scrub proves it);
the gap was OPEN-time — `parse_footer` accepted any plausible table,
and a parseable-but-wrong offset mis-slices members through the D4
plain-read path until a scrub or verified read notices. Open now
checks the footer's own hash (still one small tail read), using the
house primitive. Deliberately NOT covered at open: the obao section —
checking it would read the whole section and defeat the O(1) open,
and trees are self-authenticating on use (rot fails validation, never
verifies wrong bytes — the D49 loose-sidecar trust model), with
whole-file scrub as the localizer (members verify clean + whole-file
mismatch ⇒ the rot is in the section or footer).

**Ships as `datboi/pack/1` — v2 replaces v1 outright.** No deployed
store holds a v1 pack (D91 landed 07-15 and the swap is policy-gated),
so per the D103 amendment doctrine — version numbers are for deployed
populations — the layout keeps the /1 name and the v1 parser is
deleted. A stray old-format file fails the trailer shape / footer-hash
check and is refused whole (reported bad, never mis-sliced).

*Rejected:* one whole-pack obao with member subtrees (blake3 is
position-dependent — chunk counters and root finalization mean
`obao(pack)` shares no subtree with `obao(member)`; N member-rooted
trees is the only shape); explicit per-member `(obao_offset, obao_len)`
rows and a CBOR footer body (both add describe-vs-derive disagreement
surfaces to buy evolution that magic-versioning already provides; the
store layout stays hand-rolled fixed binary, D19); CRC32 for the
footer check (a new dependency, weaker, saves nanoseconds on a
kilobytes-scale footer); hashing the obao section at open (defeats the
tail-read open for rot the self-auth path already fails safe on);
16 KiB member alignment (nothing verifies across the pack boundary, so
there is nothing to align FOR). Deferred with an open-questions flag:
scrub-REPAIR of a rotted obao section — the trees are recomputable
byte-identically from member bytes in the same file, so an in-place
rewrite is restoration-to-name, but it's still the write-once
posture's first carve-out and wants its own ruling.

## D106 — p2p tests exercise iroh against an in-process relay, never public n0 (2026-07-18)

The recon/sync/blob tests bound their endpoints with `presets::N0` —
Number 0's public relay servers plus DNS/pkarr discovery — and then
awaited `endpoint.online()`, which blocks until a home-relay
connection to that public infrastructure is established, with no
timeout. On 2026-07-18 a CI run hung every in-flight test for the full
6-hour job cap: the GitHub runner couldn't promptly reach n0's relays,
so `online()` never resolved and nextest (no `terminate-after`
configured) never reaped the parked tasks. The mountain of
magic-nix-cache noise in that log — FlakeHub 401s, cache rate-limit
418s, disabled-substituter warnings — was a red herring; those are
harmless build-from-source fallbacks. The tests passed locally because
a dev box reaches n0 fine, which is exactly what makes an
internet-dependent unit test a flake generator.

**Test-reachable code binds no public infrastructure.** A `#[cfg(test)]`
harness stands up iroh's own `test-utils` relay coordinator —
`run_relay_server()`, a relay on `127.0.0.1:0` — and dials through it.
Every test endpoint binds `presets::Minimal` +
`RelayMode::Custom(<local map>)` + `ca_tls_config(insecure_skip_verify)`
(the test relay's cert is self-signed) and connects over the
relay-bearing `endpoint.addr()`. This exercises the real iroh
connection path — relay home connection, `online()`, QUIC connect
through the coordinator — but wholly on loopback, so `online()`
resolves in milliseconds and the run is net-less. The point was never
to bypass iroh (a direct-addr shortcut would have; rejected below) but
to run the coordinator ourselves. This mirrors iroh's own endpoint
test suite exactly. Discovery-by-id (pkarr/DNS resolve) is not the unit
under test — these tests carry explicit addresses — and stays
production's concern; the daemon keeps real n0 discovery. The spike-era
`Provider::serve`/`fetch` helpers, which have no production caller and
are reached only by tests, lose their hardcoded N0 bind and take an
endpoint from the harness — a test-only function that dials production
infrastructure is precisely the footgun this removes. Production
`serve_holdings`/`sync_blocking` keep `presets::N0`: the daemon SHOULD
speak to the real n0 network.

**A hang can never cost six hours again.** `.config/nextest.toml` gains
`slow-timeout = { period, terminate-after }`, so any future
deadlock — from any cause, not just this one — dies in minutes as a red
X naming the test, not a wall-clock job timeout with nothing to read.

*Rejected:* binding test endpoints with relays disabled and connecting
over direct loopback addresses (net-less, but routes around the relay
coordination code the p2p transport rests on — the opposite of what a
p2p test should cover); keeping `presets::N0` and
wrapping `online()` in a bounded timeout (a test that depends on
reaching the public internet and merely fails faster is still a flake,
just a quicker one); leaving `Provider::serve`/`fetch` on N0 as thin
convenience shims (keeps the loaded footgun pointed at the next test
author, for helpers with no production consumer — D-north-star:
correct-by-construction over compat).

## D107 — D-numbers are internal vocabulary; they never reach a user-facing surface (2026-07-18)

The decision log is a record for the people building datboi, and code
comments and commit messages cite it liberally (house rule) — that
stays. But a `D<n>` is meaningless jargon to someone using the
software: it points at a document they can't see and names a debate
they never had. So the rule: **no `D<n>` reference on any user-facing
surface** — not the README, not CLI output (error and status text a
user reads), not web UI copy, not daemon logs (the operator reads
them). The rationale a citation carried moves into a code comment next
to the string, where its audience actually is; the string itself
states the behavior in the user's terms. Audit at ruling time swept
every surface a user or operator can see — CLI output, HTTP error
bodies and the OpenAPI description, analyzer verdicts, library error
types (`#[error]` Display), web copy, and daemon `tracing` logs — and
cleaned roughly a dozen-and-a-half strings across nine crates; the
README was already clean. Web copy is wuchale-extracted (D67), so
`en.po` is the authoritative sweep for translated surfaces. The only
`D<n>` left in a string literal is a `#[cfg(test)]` assertion message,
which no user runs.

*Amendment (same day):* daemon logs count as user-facing. The initial
audit scoped README/CLI/web and treated `tracing` output as an
operator diagnostic outside the ruling; corrected — an operator
reading logs is a user, and a `D<n>` there is the same dead-end
jargon. The log statements (and the operator-visible startup and error
strings that flow to them) were folded into the sweep.

*Rejected:* keeping the citations as "harmless" (they're log noise in
the user's face — the same instinct that let a D-number ride into
shipped UI is the one worth ruling out); a lint/CI grep forbidding
`D\d+` in string literals (worth considering later, but heavy, and the
extract step already surfaces web copy; the test-assert exemption would
need modeling too — filed as a watch item, not built now).

## D108 — Analyzer sweep classes: ordering is claim-gated, not list-ordered (2026-07-18)

The D59 rank-7 sequencing ("structural decomposition eats the blob
before CDC takes the remainder") lived only in the element order of
the server's `families()` vec — and the D93 drone fleet defeats it:
drones drain family queues concurrently with the prime, so on the
first real ingest chunk claimed a PS1 bin while ecm was still
concluding, minting 1,518 pieces (~520 MB resident) that the D59
coverage gate would have declined seconds later. Ordering is now a
property of the analyzer itself: the `Analyzer` trait requires
`class()` — `Structural` (mints format-aware routes: preflate, ecm,
nds, narc) before `Fallback` (format-blind: chunk, noop) — and the
gate is enforced where it cannot be forgotten or raced: (1) the one
canonical roster (`analyzers::sweep_roster`) sorts by class, so a
driver walking it enqueues and drains blocker families first —
registration order can no longer lie; (2) `claim_sweep_items` takes a
mandatory blocked-by list and refuses to hand out an item while any
blocking family still holds a `sweep_queue` row for the same blob,
and the list is computed inside `process_round` (the D60 single entry
point every sweep caller uses) as the roster's ENABLED lower-class
families — prime, drones, CLI sweeps, and `/v1/sweep` all pass
through it. Corollaries: a disabled family never blocks (its stale
rows have no drain to clear them); a structural family's
environmental error keeps its queue row and therefore keeps the
blob's fallback gated — conservative, and consistent with "the
environment failed, not the analysis". *Rejected:* prime-only
ordering (observed defeated by drones); a per-wake stage barrier
across the fleet (worker coordination, and one big structural item
stalls fallback work on unrelated blobs); re-check-and-requeue inside
ChunkAnalyzer (repairs one family instead of the class; the D59 guard
stays as defense-in-depth).

## D109 — cache.db drops the never-wired blob.obao column (2026-07-18)

`blob.obao` shipped in the v1 DDL anticipating index-tracked outboard
presence and was never read or written anywhere — within one ingest
the flags (all 0) already disagreed with disk (license replays had
written two sidecars via `ensure_obao`). Outboard presence is a STORE
fact, not an index fact: every consumer asks the filesystem
(`ensure_obao` computes on miss), and D105 pack members carry their
trees in the pack itself. Dropped via cache migration v7. A future
index-side outboard ledger re-earns a column with an actual reader.
*Rejected:* wiring the flag up (a cache of a cheap stat that must be
kept true across eviction, repack, and recovery for no query that
wants it).

## D110 — ex-7z: 7z extraction moves into a sandboxed component (2026-07-18)

Measured on the first real ingest (Redump PS1, solid LZMA1 96 MiB
dict, 227 MB): sevenz-rust2 0.21.3 decodes at 9.2 MB/s where
single-threaded p7zip does 49 MB/s on the same bytes — the upload
spent ~2 minutes in extraction, ~all of it LZMA decode (solid blocks
are inherently serial; the ceiling is native decode speed, not
parallelism). Direction: an `ex-7z` wasm component vendoring 7-Zip's
own ANSI-C 7z decoder (the LZMA SDK), same shape as ex-unrar (D58)
behind the same extractor world and D89 batch ABI, replacing the
sevenz-rust2 path in `process_7z`. Speed is half the point; the shape
is the other half: members stream through the batch pipe/consumer
path (decode overlaps hashing + storing for free), and each member
gains a container→member derive recipe pinning the component — 7z
members become evictable and rebuildable exactly like rar members,
closing archive.rs's "no rebuild transform yet, so members are
extract-only residents" clause. sevenz-rust2 leaves the tree when
ex-7z lands. *Rejected:* host-native liblzma bindings (fastest raw
decode, but an unsandboxed C dependency and no recipe/replay story);
keeping sevenz-rust2 and only pipelining the hashing (saves the small
share; the decoder is the bottleneck).

*Amendment (same day):* landed at FULL 7zDec folder parity in one
motion. The interim shape — LZMA/LZMA2/Delta streaming first, with
sevenz-rust2 kept as a refusal fallback — was challenged and rejected:
two decoders behind one format is exactly the split-brain this ruling
exists to end, and "temporary" fallbacks calcify. So the streaming
pipeline covers PPMd7, the branch-filter family (x86 through the
converter's resumable state, the fixed-width ISAs through
instruction-boundary carry — the chunked-resume contract Bra.h itself
documents), and the BCJ2 four-coder tree (main stream
dictionary-streamed, call/jump/rc buffered whole — bomb-sized side
streams refuse at the allocator under the memory cap). Every shape is
gate-tested byte-exact against 7-Zip-written fixtures; shapes upstream
7zDec itself refuses (arbitrary chains, raw-stream BCJ2-only) refuse
here too and the container stays literal. sevenz-rust2's reader is
gone; its writer half survives as a dev-dependency that forges test
fixtures.

## D111 — XDVDFS decomposition + XGD1 filler regeneration: a disc is an assemble over its files and a seed (2026-09-05)

Xbox game discs — XGD1 through XGD3, as redump images or bare XISOs —
are XDVDFS: a volume descriptor at sector 32, a binary-tree directory
at absolute sectors, files at absolute sector-aligned extents, nothing
compressed or encrypted at the container level. So the container lane
is the D83 shape verbatim: a native analyzer `xdvdfs-split/1` (family
`xdvdfs`, class Structural) parses the tree into an exact coverage map
and mints per-piece derive slices plus a coverage-walk rebuild, every
recipe a builtin assemble. What is new is the filler. Every non-data
sector of the game partition is written from a mastering PRNG stream
that advances ONLY over filler sectors, in disc order. Two generators
exist: early XGD1 masters (layout-tool build ≤ 4830 — Blade II at 4808
is the last known seed-era disc, NFL Fever 2003 beta at 4830) use a
32-bit-seeded generator over GF(2^32 − 5), and the seed falls to a
meet-in-the-middle solver in ~4 ms (vendored from the xiso-trim
prototype: sound by construction — a seed is returned only after
regenerating and comparing all 2048 bytes); later discs use
rc4-drop-2048 under a 128-bit key and stay literal. Ruled: (1) **the
filler stream is its own zero-input recipe** — `xf-xgd1-prng fill
{seed, sectors}` → F, grounded vacuously by the D21 fixpoint (no
inputs to be absent) — and the disc rebuild is a builtin assemble over
the pieces, ranges of F, and zero fills. The component knows only the
generator and its affine stream jump (`serve-range` is arithmetic);
assemble composes. (2) **Redump facts are discovered, never
hardcoded.** The references (xbox_shrinker, XboxKit, xbox-dvd-compress)
and the reasoning agree: stream position 0 is game-partition sector
0; the 16 security-sector ranges (4096 sectors each, unreadable by any
drive, zero in every dump) CONSUME the stream — the physical disc
carries PRNG bytes there; the trailing zero pad and the layer-1 video
sectors do not. The analyzer's rule is prediction + equality: a
non-data sector is a stream sector iff it equals the predicted one; a
zero run of exactly 4096 sectors under a known seed is a security
range (consumed, Fill 0); every other non-data run is Fill (uniform)
or residue (a gap piece). A wrong guess costs residue, never a wrong
claim, and D4 replay is the proof. The layout-tool version is
advisory (recorded in the verdict detail): recovery is cheap, discs
between 4808 and 4830 are unknown, the sector decides. (3) **Bare
XISOs ride the same walk at base 0** — full, trimmed, or extract-xiso
rebuilt; redump images are detected by the volume-descriptor magic at
the known partition bases (XGD1, XGD2, XGD2-hybrid, XGD3), and
everything outside the game partition (video partition, layer gaps)
is run-classified into pieces and fills, so the video partition dedupes
across a mastering wave by identity. (4) **Seekable wasm children
serve ranges in place**: the executor's `open_random` serves a
declared-seekable, unquarantined wasm node through `serve-range` per
window instead of spilling it — F under the assemble is the first
consumer, and the parent's outboard verification still covers every
served byte. (5) The seed solver runs natively in the analyzer, from
the rlib the component is built from (xf-ecm's verify-at-discovery
twin); the analyzer regenerates and compares every filler sector before
claiming, and hashes the generated stream as F's identity in the same
pass. rc4-era and Xbox 360 discs still decompose — files dedupe across
variants; the filler becomes gap pieces, exactly XboxKit's `.filler`
sidecar in recipe form. Piece volume is bounded by `xdvdfs:max-pieces`
(molten, default 4096): past it, pieces are contiguous data runs
(`extent@…`) instead of files — recipe count bounded, filler
regeneration unchanged. Naming: family `xdvdfs`, because the
filesystem spans Xbox and 360 (`xiso` would lie by the first XGD2
disc); component `xf-xgd1-prng` names the GENERATOR, leaving
`xf-xgd1-rc4` for the day a key surfaces. Fixtures: two real 2 KiB
game-partition sector-0s ride the component crate (Halo v1.02 for the
seed era, Halo v1.09 for rc4) — filler bytes, no game content.
*Rejected:* one `recreate` op over a trimmed blob + layout (the xf-ecm
shape — it reimplements assemble inside the component and forfeits
per-file dedupe); an ex-xdvdfs extractor component (nothing to
sandbox — D83's argument verbatim); hardcoding redump geometry
(partition lengths vary by wave, security-sector positions vary by
disc and live in the SS.bin, not the image); treating the last partial
sector of a file as data (the references do; we slice the exact file
length and the zero tail is a fill — the piece IS the file); refusing
rc4-era discs (the decomposition still pays; only the filler is
literal); a version gate deciding the generator (advisory only).
*Swap (same day):* the D91 amendment above makes generated inputs
free and adds the regeneration trigger, so a lone seed-era disc swaps
on day one — see the D91 amendment for the evidence.
*Amendment (2026-09-06, the real seed-era disc):* Halo: Combat
Evolved v1.02 (build 3926) through the whole pipeline on a fresh
store — 1,230 pieces (1,166 files, 58 tables), seed 0x4E998EB0,
1,545,266 stream sectors matched + 65,536 consumed across the 16
security ranges: 3.30 GB of filler, 42.2% of the image, zero residue
inside the game partition (the 12.4 MB of residue is the video
partition and the layer-1 tail). Walk 10 s, sweep 17 s, swap 35 s
(3.74 GB packed, 7.83 GB reclaimed), full rebuild streamed and
verified 32 s, verified ranges in milliseconds. One correction it
forced: a directory table's declared size is its BYTE length —
sector-rounded on the v1.09 master, exact (88 bytes, 0xFF slack) on
v1.02 — so a table piece is its declared bytes and the slack
classifies like a file tail; the multiple-of-2048 refusal was wrong
and is gone. The wiki's "always a multiple of 0x800" is a lie the
first seed-era disc disproved.
*Amendment (same day, verified ranges on the composite):* the
in-place child path is the RANGE path only. `open_random` takes a
mode: range serving (`produce_range`) serves a seekable wasm child
through `serve-range`, and a window that fails the composite's
outboard check indicts that child exactly as a top-level seek path
would (quarantine, then the next read spills it through `run`);
materialization — replay, sequential streams, spills — always spills
the child through `run`, so a seek-path lie can never reach a claim
check and poison a recipe that is not lying (D49's two verifiers stay
on their own paths). Gate: a lying reference-stream child under an
assemble is caught through the composite, quarantined, and served
correctly on the next read. On the real disc the spill-based replay
is faster than per-window instantiation (full rebuild 15 s, swap
25 s).
*Amendment (2026-09-06, later — XGD2, XGD3, and seven bare XISOs):*
the shapes the position note named, all from archive.org. **XGD2**
(Dante's Inferno, redump, 7.84 GB): partition base 0xFD90000 by
magic, 7 files + 2 tables (EA packs everything into a few files),
rc4-era, 442 MB of fill, 1.40 GB of literal filler residue (17.9%),
video partition walked (13 files: the VIDEO_TS set plus a
`_SYSTEMU` directory — the system update rides the video partition
on XGD2 too, not only XGD3), both views claimed; swap packed 7.40 GB
and reclaimed 442 MB; full rebuild 5.8 s, verified ranges 30 ms.
**XGD3** (Devil May Cry HD Collection, redump, 8.74 GB): base
0x2080000, 142 files + 8 tables, rc4-era, 26 MB fill, 1.52 GB
literal filler (17.4%), video partition walked (1 file, 1 dir: the
system update alone), XISO view claimed; swap packed 8.71 GB to
reclaim 26 MB (the D112 floor fires on 26 MB — the ruling's
"one write, reclaim forever", measured at its least flattering);
full rebuild 9.9 s. **Seven extract-xiso-rebuilt XISOs** (base 0,
19 to 3,220 files): no filler of any era (the tool writes none), a
6,144-byte residue at sector 15 on every one (a fake ISO9660 PVD
extract-xiso stamps, dedupes by identity), walks in 1–260 ms; Jet
Set Radio Future (3,220 files, 2.5 GB) through the whole pipeline
packs 3,257 members into one pack in 11 s and reclaims 163 MB of
duplicate files and fill, rebuild 5.6 s — the piece cap (4,096) is
still unexercised by a real disc. Two
corrections the discs forced, both in the shared ISO9660 walker:
an XGD2 video partition declares its root directory as 194 bytes,
so a directory extent is its declared BYTES (the D111 table lesson
again — the multiple-of-2048 refusal was wrong here too), and the
descriptor set tolerates ECMA-167 recognition descriptors in
sequence. Watch: XGD3's declared video volume (25,062 sectors,
51 MB) is LARGER than the layer-0 chunk before the game partition
(34 MB) — the volume spans both layers, so no video view is claimed
on XGD3 (a two-range view over the layer-0 head and the layer-1
tail would need the SS to place the tail; deferred until a dat
names it).

## D112 — The swap fires on reclaimed bytes, not a ratio (2026-09-06)

D91's predicate ("≥ 50% of the rebuild's input bytes shared or
resident") and D111's second trigger ("≥ 25% of the container
generated") both expressed the swap's economics as a ratio, and the
two-disc measurement showed the ratio was the wrong shape twice over.
Halo v1.09 beside its seed-era twin has 36.6% of its packable bytes
already resident — under 50%, so it stayed a 7.8 GB literal — while
swapping it would write 4.38 GB once and reclaim 3.45 GB forever
(the pair sits at 73.9% of raw; it would sit at 51.9%). And a lone
rc4-era disc reclaims 0.92 GB of zero pads at "12%", which a ratio
refuses while accepting an NDS pair whose whole saving is a few
megabytes. The cost and the benefit are absolute byte counts with
different lifetimes — one write of the packed bytes plus one inode,
against reclaimed bytes forever — so dividing the benefit by the
container size compares it to neither, and throws away scale
besides. Ruled: ONE predicate, `swap:reclaim-min-bytes` (molten,
default 4 MiB — the D59 unit the system already treats as worth a
recipe): the swap fires when the container's size minus the bytes it
would have to pack (absent, single-claimed, non-generated inputs,
deduped) clears the floor. Resident pieces, pieces claimed by ≥ 2
decompositions (D91's pair-breaking heuristic, kept: the first
variant's pack IS the second's sharing), generated streams (D111) and
fill bytes are all reclaim. `swap:share-min-pct` and
`swap:generated-min-pct` are gone. Against the corpus: lone Halo
v1.02 reclaims 4.09 GB, v1.09 beside it 3.45 GB, lone v1.09 0.92 GB,
an NDS variant pair nearly its whole size — all swap; a lone padded
NDS ROM's pad is the one case under the floor, which is the outcome
D91 wanted for it. *Rejected:* a reclaim-fraction-of-container ratio
(same shape, same two failures); a cost/benefit ratio (reclaim ÷
packed — penalizes exactly the big partially-shared discs where the
absolute saving is largest); keeping the sharing ratio alongside a
floor (two knobs saying one thing).

## D113 — The video partition is a filesystem, and the disc's two halves are views (2026-09-06)

D111 classified everything outside the game partition by zero runs:
bit-exact under replay, but the video partition's pieces were named
`gap@…` and split wherever a zero run happened to fall — dedupe by
accident of layout, not by what the bytes are. The video partition of
a redump image is a plain ISO9660 DVD-Video volume (Halo: 6,992
sectors declared in the PVD, seven files under VIDEO_TS, byte-identical
across both discs). Ruled: (1) the analyzer walks it as ISO9660 —
PVD at sector 16, directory records in tree order, files as pieces
named `video:/VIDEO_TS/…`, directory extents as pieces, the
descriptors and UDF structures as the residue they are — so a video
file dedupes by identity wherever it recurs: across an XGD1/XGD2
mastering wave, and on XGD3 where every disc's video partition is
unique EXCEPT the system update file inside it, which this split
frees for the whole 360 corpus. (2) Two identities are claimed as
affine views over the image, alias tuples and all, so files shaped by
the community's tools dat-match without ever being stored: the
**video volume** — `[0, PVD volume space size)`, the ISO9660 volume
as it declares itself — and the **XISO**, the game partition as the
security-sector geometry defines it. The XISO's length is not
derivable from the image (it lives in the SS, which no drive reads),
so the XGD table that already carried the partition bases now carries
the partition lengths too (XGD1 through XGD3, XboxKit's values) —
advisory data for a view claim, never a decoding rule; an image whose
partition does not fit the table's length simply claims no XISO view.
XboxKit's video ISO pads the volume out to the physical layer lengths;
ours is the declared volume — the principled identity, and the one
that survives a re-mastering with the same content. (3) No layering:
the alternative — the disc splits into an XISO blob and a video blob,
and `xdvdfs-split` runs one level down on the XISO (the NDS→NARC
shape) — was rejected because the D91 swap materializes every absent
piece of a candidate, and an XISO piece is 3.4 GB with the regenerable
filler INSIDE it: the layer would put the filler back into a pack and
undo D111. One flat coverage map keeps the filler a zero-input stream
and the XISO a slice. A bare XISO ingested on its own is walked by the
same analyzer at base 0 and claims the same file pieces, so a redump
image and its XISO dedupe by identity whichever arrives first.
*Rejected:* a UDF walk (the UDF descriptors reference the same
extents as the ISO9660 tree; walking both would double-claim);
deriving the XISO length from content (the trailing pad is part of
the identity and unmarked in the image); layering (above).
*Amendment (2026-09-06, later — measured across a wave):* the
96 MB heads of three XGD2 images walked by the same tree. Dante's
Inferno (2010) and Tomb Raider (2013) carry byte-identical VIDEO_TS
files (the five-file "this disc is for Xbox 360" video, 2.26 MB)
under different system updates; WALL-E (2008, another master)
shares nothing with either. So the video files dedupe across the
wave by identity as ruled, and the system update dedupes only
among discs pressed with the same update — the per-file split is
what makes either sharing reachable at all (a whole-partition
piece would share nothing across those three).

*Amendment (2026-09-06, D116):* the "no layering" rejection above
rested on the swap materializing a candidate's DIRECT inputs. D116
changed the swap to pack the route graph's grounding leaves, so a
layered decomposition no longer packs its intermediate; the flat
XDVDFS map stands because it is also the simpler map (the filler is a
stream over the disc's own address space), not because layering is
unsafe.

## D114 — ISO9660 volumes decompose: `iso9660-split/1` walks cooked images by their primary tree (2026-09-06)

D113 wrote an ISO9660 walker to name the files inside a redump Xbox
image's video partition; the same walker, one level up, is the
container lane for every cooked 2048-byte-sector disc image — PS2 and
PSP redump `.iso`, PC discs, DVD-Video — and the trigger both deferred
media lanes (Lepton, balrogg) named. Ruled: a Structural analyzer
`iso9660-split/1` (family `iso9660`) parses the primary volume
descriptor at sector 16 and its directory tree into an exact coverage
map and mints through the D83 path verbatim: files and directory
tables as pieces named by path, a builtin `assemble@1` slice per piece
and one coverage-walk rebuild, every gap classified (fill / inline
literal / residue piece). Three shapes are decided here, not left to
the walker's whim: (1) **primary tree only.** Joliet (the
supplementary descriptor), Rock Ridge and UDF all reference the same
extents the primary tree does; walking two trees double-claims, so
one tree names the bytes and the primary is the one every disc has.
Names are the primary tree's (`;version` stripped); path tables and
the UDF metadata are residue — content-addressed, small, and never
the dedupe that matters. (2) **A uniform file is a fill, not a
piece.** PS2 masters pad with `DUMMY.DAT`-shaped files of zeros, tens
to hundreds of megabytes, whose only identity is their length; a
piece claim would pack them. Files of at least 1 MiB are read once
at layout time and a single-valued one becomes a fill region of its
length — zero storage, no claim (the file's hash is never minted:
nothing dedupes against a run of zeros by identity). (3) **The
residue gate decides ownership between structural families.** A
redump Xbox image carries a DVD-Video volume at sector 16, so this
walker sees it too; its game partition is bytes the primary tree
does not name, and more than a quarter of an image outside the tree
(above a 4 MiB floor, so descriptor/path-table/UDF overhead never
trips it) is a settled Negative: "not iso9660-shaped". `xdvdfs-split/1`
owns the disc, deterministically, in either sweep order — no
family-to-family coupling. Corollaries: multi-extent files (the
ISO9660 shape of a > 4 GiB file) are one piece per extent (`path`,
`path#1`, …); a directory's declared length is its BYTE length,
sector-rounded or not — an XGD2 video partition declares its root as
194 bytes, the D111 lesson recurring; the descriptor set (sector 16
through the terminator, ECMA-167 recognition descriptors tolerated
in sequence) is a declared range so a zero system area classifies as
fill on its own; piece volume rides `iso9660:max-pieces` (molten,
4096) with the D111 coalescing rule. No views, no generated streams:
a lone image reclaims only its fills and duplicate members (the
D112 predicate decides, and a plain data disc mostly stays literal),
and the win is cross-image sharing — regional and revision variants
of a disc hold most of their files in common, and every shared piece
is reclaim. Raw 2352-byte images (bin/cue) are out of scope: the ECM
lane's stream keeps mode-2 subheaders, so it is not a cooked volume;
layering ISO9660 under ECM is a later ruling. Proven on real discs
on the day: a PSP image (15 files, 20 MB of fill, swap + bit-exact
rebuild + verified ranges) and a 1.8 GB Xbox 360 press-kit DVD-ROM
(278 files, 245 MB reclaimed from duplicate members alone), while
three redump Xbox images (XGD1, XGD2, XGD3) each concluded Negative
at the residue gate. *Rejected:* a UDF walk (double-claim, and UDF
is the tree a disc may lack); Joliet names (same extents, and
absent on console discs); claiming uniform files as pieces (the
D111 "piece IS the file" principle is about partial sectors, not
about storing zeros); an explicit "declines XDVDFS images" guard
(coupling where the residue gate already concludes); a raw-sector
mode (a different sector pitch is a different lane).

## D115 — GameCube decomposition + junk regeneration: `gcm-split/1` is D111 on a positional generator (2026-09-06)

A GameCube disc is pure concatenation — boot.bin and bi2.bin at 0,
the apploader at 0x2440, the DOL and the FST where the boot block
points, files at absolute FST offsets — and every unused byte is
written by Nintendo's mastering tool from a lagged Fibonacci
generator (`k = 521, j = 32`, Dolphin's reconstruction, `nod`'s port)
that is RESEEDED at every 32 KiB sector from the first four bytes of
the game ID, the disc number and the sector index. So junk is a pure
function of position: byte `x` of the disc is `LFG(seed(id, disc,
x / 32 KiB))[x % 32 KiB]`, no stream state crosses a data extent,
and nothing is consumed by the data in between — simpler than XGD1,
where the stream advances only over filler sectors and security
ranges eat it. Ruled: (1) **the junk stream is a zero-input recipe
over the disc's whole address space** — `xf-gc-junk fill {id, disc,
len}` → J, `len` the image length — and the disc's rebuild is a
builtin assemble whose junk segments are ranges of J at the SAME
offsets the junk occupies on the disc; the component knows only the
generator (`serve-range` reseeds the sector and skips), assemble
composes. J's identity is shared by every disc pressed under one
game ID and disc number (revisions: Doubutsu no Mori + and its Rev 1
claim the same J), and never stored. (2) **Prediction + equality,
per byte.** The analyzer generates the expected junk for each gap
once and walks the gap as runs: junk where the disc equals the
generator for at least 16 bytes (2^-128 by chance) or to the run's
end, fill where one byte value holds for 512 bytes or to the end,
residue up to wherever the next junk or fill run begins — linear,
via run lengths precomputed backwards. A file's slack within its last
sector is junk from the byte after the file (the mastering tool
writes junk into every unused byte), and the walk confirms it: four
real discs leave 30 to 339 BYTES of residue each. A wrong guess
costs residue, never a wrong claim; D4 replay is the proof. A
zero-padded or NKit-scrubbed master matches nothing, claims no J,
publishes no component — its pad is fills. (3) **System pieces are
named ndstool-style** (`sys/bi2.bin`, `sys/apploader.img`,
`sys/main.dol`, `sys/fst.bin`; boot.bin is 0x440 bytes and rides as
a literal), files by FST path; two FST entries over one extent are
one piece. The DOL's length is the end of its furthest section, the
apploader's its header's code + trailer sizes. Piece volume rides
`gcm:max-pieces` (molten, 4096) with the D111 coalescing rule. (4)
**The swap's view filter lets generated inputs through** (D112
tail, amended here): J is exactly as large as the disc, and a route
with an input at least as large as its output is a view — unless
that input is generated, which is never packed and so never the
"whole" a slice comes from. Nothing about the disc's size is
assumed: a trimmed image is walked as far as it goes and an FST
extent past the end is a refusal. A Wii disc (magic at 0x18) is a
settled Negative naming the other family. Measured on the day (all
from archive.org, redump-verified): Doubutsu no Mori + (Japan) —
12 files, 97.4% of the 1.46 GB disc is junk, 32 bytes of residue;
Rally Championship (USA) — 228 files, 61.9% junk, 339 bytes of
residue; walks in 3.6–5.3 s of which most is generating and hashing
J once. Through the whole pipeline on a fresh store: Doubutsu no
Mori + (Rev 1) packs 37.9 MB and evicts the 1.46 GB disc (swap 9 s,
full rebuild through the component 8.3 s, verified ranges 10 ms);
Rally Championship (USA) packs 556 MB. Pairs: the USA/Europe Rally
discs (different game IDs, so different junk streams) share 99.8% of
their file bytes and the pair sits at 19.1% of raw after one swap
pass; the two Doubutsu revisions (one game ID, one J) sit at 2.5% of
raw. RVZ and NKit forms are NOT ingest forms yet: decoding is
deterministic (nod's decode of an RVZ reproduces the redump hash),
but a byte-exact RVZ rebuild pins a compressor, and that is its own
ruling; the corpus's RVZ discs were decoded offline for these
gates. Wii is the next family (partitions, the D12 key as an input,
the same generator inside a partition) and needs its rulings first.
*Rejected:* one stream that advances only over junk (the XGD1 shape
— wrong for a positional generator, and it would make J unique per
disc where the disc address space makes it shared per title); a
per-gap `fill {id, disc, offset, len}` recipe (thousands of
zero-input recipes where one range of one stream does); trusting the
1.46 GB size (an image is what it is); treating the junk after a
file's end as slack to classify by shape (it is junk, and it
verifies as junk); depending on the `nod` crate at runtime (the
generator is 80 lines and must be component-frozen; nod stays the
reference and the decoder for RVZ/NKit offline).

## D116 — Wii decomposition: the swap packs grounding leaves, keys are found by hash, and the re-encrypt serves in place (2026-09-06)

A Wii disc is partitions — each an AES-128-CBC body under a title key
that the ticket carries wrapped under a console COMMON key, with a
SHA-1 hash tree (H0 per KiB, H1 per sector, H2 per 8-sector subgroup,
one block per 32 KiB sector, recomputable from the data) — and the
same mastering junk as GameCube around and inside them. So the
container lane is D115 one level down: `wii-split/1` (family `wii`,
Structural) claims the disc's structures and the partition BODIES as
pieces of the disc, and a verified body decomposes further — its
plaintext walks as a GameCube volume (`gcm::parse_volume`, shared,
with the `>> 2` offsets) into system + FST-file pieces with the junk
regenerated; the body's rebuild is one `xf-wii-crypt encrypt
{wrapped title key, title id, sectors}` over the plaintext and the
key, its derive the `decrypt` inverse. The update partition dedupes
across every disc pressed with the same system menu; the channel
partitions too. Three rulings make it fit: (1) **key discovery by
known hash (D12 made concrete).** The build knows the six console
common keys by the blake3 of their 16 bytes — retail, Korean, vWii
and their debug twins — never by value; a ticket's issuer string and
key index name which one, and the analyzer reads it from the store
like any blob. A disc whose key is not held is neither an
environmental error (a queue row would gate the disc's fallback
families forever, D108) nor a settled Negative (it would never
re-run): it is DEFERRED — the item leaves the queue with no analysis
row and waits in `sweep_deferred` (cache v8) on the key's hash, and
the next queue refresh re-enqueues it once that hash is a resident
data blob. Cache-grade: a rebuilt cache re-derives the wait in three
small reads. (2) **The swap packs grounding leaves, not direct inputs
(D91/D112 extended).** An absent, non-generated input with a DOWNWARD
route — a non-failed route of its own that is not a view by D112's
test (no non-generated input at least as large as its output) — is an
intermediate: never packed, walked instead, and its route licensed
bottom-up WITHOUT materializing (`Executor::license`, the D25 proof
over a hashing sink) before the container evicts; an input with no
downward route is a leaf and packs. The body (encrypt < slice of the
disc) and the plaintext (assemble < decrypt) are intermediates; the
files are leaves. One-level decompositions are unchanged (every input
is a leaf), and the existing gates prove it. (3) **The re-encrypt's
seek class is Affine at a 2 MiB quantum.** A sector's ciphertext
needs its group's H2s, so `serve-range` hashes one 2 MiB group and
encrypts the sectors a window touches; `decrypt` is sector-granular.
The executor already serves a declared-seekable wasm child in place
(D111), so an evicted disc's range read costs one group of AES + SHA-1
in wasm, never a spill of the partition. Verification precedes every
claim (D111's rule): the analyzer decrypts every sector natively,
recomputes the tree, and compares each hash block BYTE FOR BYTE,
padding included; a partition that disagrees stays an opaque piece of
the disc — a converted WBFS rip (whose scrubbed sectors carry zero
hash blocks) does exactly that, and its disc still decomposes around
it. One correction the real disc forced: a partition's junk is seeded
from the PARTITION's own boot-block ID and disc number, not the disc
header's — under the disc's ID the update and channel partitions of
Super Smash Bros. Brawl matched nothing and left 6–40 KB of residue
each; under their own IDs they match and leave 56–84 BYTES. A
partition seeded like the disc (the data partition) shares the disc's
one junk stream (the same generator over a prefix of the same address
space); any other claims its own `fill {id, disc, len}`. `nod` seeds
every partition from the disc header and is wrong for those; ours is
verified by equality either way. Measured on the day: Super Smash
Bros. Brawl (Europe, Australia), redump, 8.51 GB dual-layer — 15
partitions (update, data, 13 channels), every body verified; 769 MB
of junk regenerated (9.0% of the image: 323 MB between partitions,
447 MB inside the 7.46 GB data partition, ~0.3 MB inside the rest);
679 bytes of residue on the disc, 870 inside partitions; the data
partition's 5,880 files coalesced to 3 extents at `wii:max-pieces`
(4096) — the first real disc to exercise a piece cap; walk 18 s.
*Amendment (same day, the pipeline on the real disc):* Brawl through
ingest → sweep → swap → evict → rebuild on a fresh store: ingest 25 s,
walk + mint 27 s; the swap packed 7.29 GB of leaves (never the bodies,
never the plaintexts) and evicted the 8.51 GB disc — 85.7% of raw
resident for a lone disc (the 9% junk, the 3% of hash blocks and the
fills are the reclaim; the sharing case needs a second disc); full
rebuild through the component streamed and verified in 364 s; four
verified ranges in 149 ms. The swap took 1,703 s: packing reads the
disc once, and the licensing then runs the wasm AES + SHA-1 pass over
the 7.46 GB data partition twice (the encrypt route's verify-only
license, then the top replay's stream through it) — the cost watch
item in open-questions names the native fast path as the lever.
*Amendment (same day, one level down on DS):* the leaf walk changes
D94's swap behaviour, as it should. A NARC piece of a ROM has its own
assemble over its members — a downward route — so the swap now packs
the MEMBERS and licenses the NARC's rebuild verify-only instead of
packing the NARC whole. Gate: two regional variants differing in one
member inside a NARC swap to one pack of every member once plus the
ROM's other pieces, neither NARC packed, both ROMs rebuilt and range-
served through the two-level assemble; before D116 the pair packed
both NARCs, the shared members twice. This is the cross-variant
sharing D94 was built for, reached at the swap for the first time.
*Rejected:* shipping key bytes (D12/D26 stand; hashes are names, not
keys); an environmental error for a missing key (gates fallback
forever); a Negative for a missing key (never re-runs); a dedicated
key table in the index (a key is a blob like any other, and the
sweep already knows how to wait); packing the body or the plaintext
(disc-sized, regenerable — the D113 layering objection, now removed
at the swap); replay-licensing the intermediates (writes both,
disc scale each); seeding partition junk from the disc header
(nod's rule, measured wrong); a UDF-style second tree for the
partition (the FST is the one tree); Opaque for the re-encrypt (every
range read would spill 4+ GB); keeping D113's "no layering"
rationale (superseded, amended there).

## D117 — A dat revision names the bytes, not the sighting (2026-09-20)

Importing a dat whose bytes the current revision of that source already
carries is a **no-op**: `import_dat` returns the existing revision and
writes nothing. It used to mint a fresh revision every time — new
`dat_revision` row, a full re-insert of entries and claims, re-unify,
re-rollup, and `set_current_revision` onto the new one, with D38 then
demoting the old. Measured on a real MAME 0.287 listxml: two imports of
one file gave revisions 1 and 2, both blob `31021e1d…`, 49,860 entries
and 400,003 claims each, ~22 s of pure churn for the second. This is
D15 read consistently — rows are a deterministic function of the blob,
so identical bytes cannot mean a different revision — and it is what
makes `dat import` safe to put in a boot-time unit or a timer, which is
how the arcade cabinet's rom server (hosts/datboi in schlarpc-flake)
consumes it. The check is per `(source, blob)` and only against the
source's **current** revision: re-importing an older blob still moves
the source back to it as a new revision, because that is a real change
of what the source currently says. A demoted current revision (D38
header-only, rows deleted) re-materializes rather than short-circuits —
the no-op promises the rows are already there. The blob still lands in
CAS on every import, unchanged; `put_new` was always idempotent. The
report gains an `unchanged` flag so the CLI can say "already current"
instead of lying about work it did not do.

*Rejected:* keeping the churn and telling consumers to guard with their
own stamps (they did, and it is a worse version of this check placed
further from the facts); updating `imported_at` on a no-op re-import (a
revision would then carry a time that no longer identifies when its
content was first seen — "when did we last look" belongs to a sighting
log, which nothing has asked for); comparing header fields rather than
the blob (a dat whose only change is its date IS a new revision by this
same rule, and byte equality is the only comparison that cannot drift);
short-circuiting on the blob alone regardless of which source it was
imported under (provider/system are half the durable source identity,
dats.md — the same bytes filed under two names are two sources).

## D118 — A view snapshot hash is a function of its content (2026-09-20)

`created_at` leaves the manifest. `ViewSnapshot` carried the evaluation
time inside the CBOR that `evaluate_view` hashes, so every `view eval`
minted a new snapshot even when the ViewDef, the source revision and
the held set were all identical — measured: four consecutive evals over
one source and one store, four hashes. That made the snapshot hash
useless for the question every consumer actually has ("did this view
change?"), accumulated one meta blob per eval, and — because fileids
beneath a view are keyed `(snapshot, path)` (D33) — meant a no-op
re-eval stale every handle a mounted client held, re-walking a console's
tree for nothing. A snapshot is the content-addressed *result* of an
evaluation (D23); the time an evaluation happened is an event, and the
`tag` row the flip writes already records it with the exact same value.
So the time was never lost, only misplaced. The object goes to
`datboi/viewsnap/2`, which is what the version in the header is for:
v2 has no key 1 and rejects one, v1 still decodes (its `created_at` is
read and kept in the struct, so pinned snapshots and GC roots from
before this ruling stay readable), and nothing writes v1 again. Re-eval
of an unchanged view now re-mints the identical hash, `set_tag` writes
the same value it already held, and the flip is genuinely a no-op.

*Rejected:* keeping the field and hashing a subset of the manifest (two
notions of "the bytes" for one object, and the stored blob would no
longer be what its hash attests); zeroing `created_at` in the encoding
(the field would be a lie rather than absent, and a decoder could not
tell a zeroed one from an epoch one); a v1-tolerant decode that ignores
key 1 without a version bump (decode-then-encode would change the hash
of an existing object, which is the one thing a content-addressed
object may never do); leaving it and having consumers diff the row set
themselves (that is the snapshot's whole job).

## D119 — An entry name is a label, not a key (2026-09-20)

`entry` drops `UNIQUE (revision_id, name)`. Real dats carry several
entries with one name: No-Intro's Game Boy set lists "Lion King, The
(USA, Europe) (Beta)" four times over four genuinely different dumps
(131,072 / 262,144 / 524,288 / 524,288 bytes, four distinct CRCs), and
"Small Town Emo (World)" twice over two, distinguished only by release
date. The constraint turned that into `UNIQUE constraint failed:
entry.revision_id, entry.name` and rejected the whole file: six of nine
No-Intro dats fetched for one adoption — NES, SNES, N64, Game Boy, Game
Boy Color and Game Boy Advance — were entirely unimportable, which is
most of a rom manager's reason to exist. A dat's `name` is the
publisher's display label; identity is `entry_id`, and the thing that
was meant to carry cross-revision identity is `stable_key` (the
No-Intro id where present, rom-content overlap otherwise, dats.md §61).
Conflating the two made the label load-bearing and the loudest dats in
the world unreadable.

What replaces it: nothing, by design — duplicate names are simply
allowed, and `entry_by_name (revision_id, name)` (non-unique) keeps the
parent-resolution lookup and the autoindex's former users fast. The one
place uniqueness was implicitly relied on is `cloneof`/`romof`
resolution, a correlated subquery that would silently take whichever
row the planner reached first; it now takes `MIN(entry_id)` explicitly,
so an ambiguous parent reference resolves the same way on every build
and every re-import rather than by query-plan luck. The ambiguity is
the dat's, not ours, and a deterministic arbitrary choice is the honest
reading of it. cache.db goes to v9 with a ladder step that rebuilds the
table, since SQLite cannot drop a table constraint in place.

*Rejected:* disambiguating duplicates on import by suffixing the second
(inventing a name no dat contains, which then leaks into every view
path and every frontend list); skipping duplicate entries with a count
in the report (silently losing real dumps — three of the four Lion King
betas would vanish); keeping the constraint and declaring these dats
malformed (they are what the publishers ship, and a manager that reads
only hypothetical dats is not a manager); making `stable_key` unique
instead (it is nullable and derived, and a dat with no ids would
collapse to one entry).

## D120 — Ingest fans out on files; one writer owns the database (2026-09-20)

`Ingester::ingest` walks serially and the wall clock is one core's hash
chain. Measured adopting a 549 GB MAME set (35,494 zips, ~767 CHDs) over
NFS on an 8-core EPYC 9124: **142 MB/s, 8 files/s, 71% of ONE core** —
seven cores idle while the same mount reads 1,519 MB/s cold, 10x what
ingest consumes. The constraint is `AliasHasher`: every byte goes through
crc32 + md5 + sha1 + sha256 + blake3 serially (D2's full tuple — dats
identify by the legacy digests, the store addresses by blake3), and every
zip member is inflated and run through the same five again. sha1, sha256
and blake3 each have a hardware path on this host; md5 does not, and is
roughly half the chain's cost on its own. The chain is not going to get
faster, so the files have to overlap.

The pipeline splits in two. A bounded pool of workers does everything
that is pure CPU or source I/O — open the file, stream it through
`put_new`, sniff the head, hash every zip member out of the stored blob,
evaluate detectors, build the recipes — and hands back a fully-formed
`FileWork` saying what to record. One writer thread applies it: every
`Db` mutation in the process happens there, in walk order. SQLite has a
single writer under WAL regardless, so only the hashing is worth fanning
out; the store is a different matter and workers write it directly, since
every store write is content-addressed, idempotent and already safe from
several threads (`Store` is `Sync` — the D89 extract path has published
from consumer threads since rar landed).

**Determinism comes from ordering the commit queue, not the work.** The
walk stamps a sequence number on every item it produces — files,
symlink notes, `read_dir` failures alike — and the writer retires them
strictly in that order out of a small reorder buffer. Every
`IngestReport` field keeps its meaning and its order: counters, `notes`,
`errors`, `member_skips` and `fresh_blobs` all land exactly as the serial
walk produced them, which is what makes "same corpus, same report" a
testable claim rather than a hope.

**Crash discipline is unchanged.** All of a file's rows are written by
one thread in the old order, and its `source_file` row is still written
last — a crash re-processes that file, every write is a content-addressed
upsert, at-least-once holds. Ordering the commits also means a crash
truncates the run at a walk-order prefix instead of leaving a lattice of
whichever files happened to finish.

Three placements fall out of the split. The rescan-cache lookup is a `Db`
read, so it stays on the writer and happens *before* dispatch — a cache
hit must never reach a worker, since not reading the file is the entire
point of the cache. 7z/rar extraction also stays on the writer:
`ensure_extractor` lazily builds and publishes wasm state and each member
mints a recipe, and that path already fans out internally (D89 batch
pipes, a consumer thread per member), so a container is parallel where it
counts and containers are the rare case in the corpora that hurt.
Everything else — the zip lane, detectors, the CHD header — is worker
work.

**In-flight work is bounded by bytes, not by file count.** The thing being
bounded is not the serial pipeline's footprint: measured mid-run its
working set is ~106 MB of anon (the 12–14 GB systemd reports as
`MemoryPeak` is the cgroup's reclaimable page cache from streaming
hundreds of GB over NFS, not heap — measure RSS or cgroup `anon`, never
`MemoryPeak`, on an I/O-heavy unit). It is that skipper evaluation
buffers a whole file under `skipper_cap` (256 MB default), so N workers
multiply one such buffer by N. The writer therefore dispatches only while
the summed size of dispatched-but-unretired files fits a cap derived from
the worker count, and a file bigger than the whole cap is admitted alone.
Dispatch is in walk order and the queue is FIFO, so the lowest unretired
sequence number always holds its slot and always runs: the budget cannot
deadlock against the reorder buffer that is waiting on it.

Parallelism defaults to `available_parallelism` and is configurable —
`IngestConfig::parallelism` (0 = derive it) and `datboi ingest --jobs`,
wired like `--rescan`. One worker is not a special case: it is the same
pipeline with a pool of one, so there is no second code path to keep
honest.

*Rejected:* parallelising the five hashes *within* a file (they are
independent over the same byte stream, but a tee with per-chunk
synchronization is paid by every `AliasHasher` caller — `put_new`, member
hashing, detector variants — to win a case we do not have: 35,494 files
saturate 8 cores without it. The residual, one huge CHD alone on an idle
box, is a watch item and not a reason to complicate the hasher); several
DB writers or a connection per worker (SQLite takes one writer under WAL;
it converts a hash bottleneck into lock contention and throws away the
commit order that makes reports deterministic); letting workers write the
DB behind a mutex (same lost ordering, and the mutex would serialize them
anyway); `rayon` over the walk (collects the path list whole — the D36
"10M small files" case is exactly where that bites — with no byte bound
and no ordered commit); bounding in-flight work by file count (a count
tuned for 35k small zips is N × 256 MB the moment the files are the ones
that matter); sorting the report at the end instead of ordering the queue
(`notes`, `errors` and `fresh_blobs` have no sort key that reproduces
walk order, and `fresh_blobs` is deliberately id order); a separate serial
implementation kept beside the parallel one for the `-j1` case (two
pipelines, one of them untested by the deployment that matters).

*Amendment (same day):* the in-flight budget charges what a file may
BUFFER, not what a file is. Weighing every file by its size throttles
the lane the pool exists for — eight 1 GB CHDs each stream through a
64 KiB buffer, and a size-weighted budget admits them one at a time —
while still not naming the thing it bounds. Skipper evaluation is the
only lane that holds a whole file, and it holds it twice (`apply`
copies the variant out beside the buffer), so `stage` charges a file
its size when a detector set is loaded and the file is under
`skipper_cap`, and charges zero otherwise; over-charging a container
that merely looks eligible is the safe direction. A file heavier than
the whole cap runs alone rather than never, and the lanes charged
nothing are bounded by the worker count and a 256-entry reorder buffer
instead. Measured, 10 × 200 MiB detector-lane files at 8 workers:
512 MiB of budget, two files in flight, 812 MiB peak RSS against
411 MiB for the same corpus serial — the doubling is the variant copy,
so the practical heap ceiling is ~2× the cap and the parallelism a
big-file detector corpus gets is cap/size, by design. The lanes that
matter are untouched: 1.9 GiB of loose files and zips with no detector
set ingests in 6.46 s at `--jobs 1` and 0.55 s at `--jobs 16` (11.7x
on 16 cores, 14.7 cores busy, 304 MB/s → 3.5 GB/s) with peak RSS
moving 12 MiB → 14 MiB.

## D121 — The blessing pass, built: bulk, parallel, and aimed at the routes with no floor (2026-09-21)

D63 named an "optional background **blessing pass** (materialize-to-null,
tee, cache the obao4)" on 2026-07-10 and nothing ever built it. Its
2026-09-20 amendment then made the pass load-bearing for a corpus it was
never aimed at: 152,014 `deflate-decompress@1` members on the live
deployment, 107,090 of them over one bao group, each needing a full
inflate before its first byte can be served. The amendment's on-demand
blessing makes those bytes readable, but it charges the whole bill to
the first reader, and the first reader is `mame -verifyroms` — ONE
serial process. Measured over the NFS mount: 6,090 sets/hr, ~8 hours
projected, the daemon at ~257% of 800% CPU. The cost is not the reads;
it is 107,090 materializations serialised behind a client that issues
them one at a time. `datboi bless` unserialises them: the same work, off
the read path, fanned out across the cores the daemon already has, so
the wall clock is bounded by reading the zips instead of by the client.

**Default selection inverts D63's aim, which is why this is a ruling and
not just an implementation.** D63's sentence promotes *carved-out*
routes — affine ones that already serve — and its own `*Rejected:*` list
refuses mandatory blessing for exactly those. The routes that actually
need a pass today are the ones the amendment found: non-affine, no
carve-out, no floor to be a ceiling over, where the tree is the only way
to serve the bytes at all. So the default blesses what the carve-out
CANNOT serve, and the promotion D63 literally described — affine routes,
a floor traded up to a ceiling — is opt-in behind `--include-affine`.
Both halves of D63 survive: the cost objection still governs the affine
case (nobody pays a full pass over a TB-scale image by default), and the
optional promotion is still available to anyone who wants it.

**The predicate is not re-implemented.** Candidate selection is a cheap
SQL filter (Data namespace, non-resident, size over one chunk group, at
least one non-Failed producing recipe); the actual skip/bless verdict is
`Executor::affine_carveout` on the planned route — the same predicate
`serve_range` consults, called from the same place. A second definition
of "is this affine" in SQL would drift from the one that decides what
gets served, and drift here means blessing routes that need nothing or
skipping routes that are unreadable.

**The shape is D120's, with one lane it does not need.** A bounded pool
of workers does the materialize-and-hash (`open_sequential` →
`obao::compute` → `put_obao`) and touches no `Db`; the coordinator owns
the only `Db` handle and does every read — the candidate walk, the
planning, the carve-out check — before dispatch, exactly where D120 puts
its rescan-cache lookup and for the same reason. D120's writer half is
vacuous here: blessing mutates no `Db` row at all, so there is nothing to
retire in order. Determinism therefore comes from sorting the one ordered
output (the failure list) by its candidate sequence number rather than
from a reorder buffer — D120 rejected sort-at-the-end because `notes`,
`errors` and `fresh_blobs` have no sort key that reproduces walk order;
this report has exactly one such key and one such list, so the rejection
does not reach it. In-flight work is bounded by what a job BUFFERS, per
D120's amendment: the buffer here is the outboard `obao::compute` builds
in memory (~len/256), so a job is charged `outboard_size(len)` against a
per-worker tree budget, and a job heavier than the whole budget runs
alone. Parallelism defaults to `available_parallelism`, `--jobs N`
overrides, `-j1` is the same pipeline with a pool of one.

**Resumability is the store's, not a checkpoint's.** The pass holds no
state: a sidecar on disk IS the record that a blob is blessed, `put_obao`
is temp → fsync → rename and idempotent, and nothing else is written. So
an interrupted run loses at most the in-flight materializations, a re-run
skips everything already blessed, and running it beside a live daemon
that is blessing on demand is safe by the same argument the D63 amendment
already made for its own herd.

**`READ_POOL_SIZE` stays at 4.** The comment invited raising it once "a
surface measures a need", and this surface measured one — the NFS read
handler holds a read connection across the whole of `serve_range`, so a
cold member pins one of four connections for a multi-second
materialization. But the number was never the defect: the defect was a
multi-second read. With blessing moved ahead of the reader, reads are
short again and the original premise ("four absorbs one slow read
without serializing the rest") holds on its own terms. Raising it would
be buying concurrency for a path that no longer blocks, and it is not
free — each connection is a `Db` handle, and a bigger pool means more
requests simultaneously holding one across a materialization in whatever
case we have not found yet. Re-open it on a measurement of concurrent
readers, not on this one.

**Measured.** A synthetic corpus of 150 zips × 8 DEFLATE members ×
4 MiB (1,200 members, 4.69 GiB of content, 2.3 GB of zips — a ~2:1
ratio, the shape the MAME set shows), ingested, then stripped of the
trees ingest built so every member is in the pre-amendment state. On an
AMD Ryzen 7 5800X (8 cores / 16 threads), store on ZFS:

| jobs | best wall | throughput | members/s |
|------|-----------|------------|-----------|
| 1    | 16.35 s   | 294 MiB/s  | 73        |
| 16   | 1.83 s    | 2,620 MiB/s| 655       |

**8.9x**, and the ratio is the stable part: across seven trials each the
medians are 21.6 s and 2.46 s — 8.8x — while the absolutes move ±40%
because the machine was shared with other work throughout (load average
9–12). Per-thread throughput falls from 294 to 164 MiB/s at 16 threads,
which is what SMT on a hash-and-inflate chain should look like. The
intermediate points (2, 4, 8) were too contended on this box to report
honestly. Scaled at the corpus this exists for, one core's 73
members/s is ~24 minutes for 107,090 members and sixteen threads' 655
is ~2.7 — against the ~8 hours the same blessings cost when a serial
client pays for them one at a time inside its own reads.

*Rejected:* blessing everything with a route by default (that is D63's
rejected mandatory blessing, re-proposed with a CLI in front of it — a
full pass over every TB-scale affine image for no served-byte
guarantee); a SQL-side affine predicate (a second definition of the
carve-out that drifts from the one `serve_range` uses); advancing recipe
verify state to `ReplayedLocal` on a successful blessing (it is a full
materialization with a claim check, so it LOOKS like D25's licensing
event — but licensing permits literal drops, and a pass whose stated job
is "add a sidecar" must not acquire the authority to delete bytes as a
side effect; a recipe with several outputs is not proven by hashing one
of them either); a `--size-cap` on what gets blessed (the same
permanent-unreadability failure the amendment already rejected for
on-demand blessing); recording progress in the `Db` for resume (the
sidecar already is the record, and a checkpoint table would be a second
truth to keep honest); raising `READ_POOL_SIZE` (above); a daemon job +
web surface for the pass (the right eventual home under D96 — bless is
byte-level work that belongs in the D74 ledger with its own `JobKind`
and an activity row — but a new `JobKind` is a wire-enum and web change,
and the CLI is what the corpus needs today; ledger-stamped like
`Recover`/`Snapshot`/view eval, i.e. not yet, and for the same recorded
reason).

*Amendment (same day): blessing removes an error; keeping the bytes
removes the cost. `--materialize`, opt-in, size-gated.* Measured after
the pass was written, and it changes what the pass is worth.
`deflate-decompress@1` carries `seek_class = Opaque`, and `produce_range`
excludes Opaque from the random-access lane — so an opaque route serves
EVERY range read by spilling a fresh full materialization of the output
to a temp file, windowing it, and throwing it away. Reading a 264 MB
member in 128 KB NFS chunks is ~2,100 windows × 264 MB of inflate:
quadratic in member size. On the live host one such member (`acheart`)
held a client in uninterruptible sleep for 30+ minutes. A blessed opaque
member is readable and still quadratic: the sidecar buys the D49 check
and the absence of `MissingOutboard`, nothing about the per-read cost.

The pass already inflates every one of those members in full. The bytes
it discards are exactly the bytes that, kept, make the member a resident
literal — and a resident literal takes `serve_range`'s first branch:
a plain verified file read, no route, no spill, O(range). So
`--materialize` swaps `obao::compute`-and-drop for
`Store::put_with_obao`, which tees the same single stream to the store
while building the same tree. One inflate per member, forever, instead
of one per window.

**It is opt-in, and it is not free.** Materializing the 141,986 absent
members costs ~133 GB, and — this is the part that must not be
assumed — **none of it is reclaimed by evicting the containers**. A zip
is evictable only through a rebuild route, and the only thing that mints
one is `preflate-split` (its `#recreate` + affine-assemble pair). The
absent members are by construction the ones in containers preflate has
NOT split; the 10,028 already-resident ones are the ones it has. So the
trade is a straight +133 GB high-water, not a swap, until the preflate
backlog catches up — and at the measured ~2 members per 3 minutes
against 141,986 remaining, that is months, which is why "wait for
refine" is not an answer either. `--min-size` is the dial that makes
this proportionate: the 724 absent members over 16 MB are where the
quadratic is catastrophic, and `--materialize --min-size 16M` buys the
cure for a few tens of GB instead of 133.

**It stays reversible, and that is a requirement, not a nicety.** A
materialized member records `Residency::Resident`, `verified_at`, and —
when the route claims exactly one output, which `deflate-decompress@1`
always does — advances that recipe to `ReplayedLocal`. That last one is
D25's licensing event, and it is what makes `Db::is_evictable` true for
the member afterwards: the route grounds in the container, which is
still resident, so `datboi evict` and the D72 watermarks can take the
bytes back. Without the licensing step the pass would be a one-way
spend, and a residency decision nobody can undo is not one a CLI flag
should be able to make. Several-output routes stay unlicensed —
producing one output does not prove the others — and `Executor::replay`
remains the way to license those.

**Both halves of that trade were checked on the bench corpus, not just
argued.** After `bless --materialize --limit 20` over the 1,200-member
fixture: `datboi evict --dry-run` reports "20 blob(s) evictable,
83886080 byte(s) reclaimable, 0 candidate(s) blocked" — the licensing
advance does make the spend refundable — while `datboi status` still
reports the 150 containers as "literal-only … (no rebuild route yet)",
which is the other half: the containers stay put, so the bytes are a
high-water and not a swap.

**This is a ruling on half of an open question, and only half.**
`docs/open-questions.md` leaves open "materialize view-pinned absent
members whose containers refused a preflate split (the serving case)".
What is ruled here is the OPERATOR-INVOKED case: an explicit flag, an
explicit size floor, reversible, with the bill stated. What stays open
is the automatic half — whether the daemon should do this on its own,
driven by view pinning or dat-awareness, and under what watermark. That
needs the residency policy this repo has not written yet, not a flag.

**The `Db` lane comes back.** The blessing-only pass mutates no index
row, which is why the entry above calls D120's writer half vacuous. With
`--materialize` it is not: residency, `verified_at` and the licensing
advance are index writes, and they happen on the coordinator, in D120's
exact shape — workers publish content-addressed bytes (idempotent, safe
from N threads), the coordinator alone says what they mean. A D56
headroom check runs per job rather than once, because N workers spend
the same free space concurrently; the first `InsufficientHeadroom` stops
staging and drains, so a full disk is one reported failure instead of
141,985 identical ones, and everything already published stands.

*Rejected (amendment):* materializing by default (a 133 GB residency
decision is the operator's, and the flag is where they make it);
assuming the containers become evictable in exchange (they do not —
checked: no rebuild route exists without preflate, and the members that
have one are already resident); skipping the licensing advance (it would
make the spend one-way, which is the difference between a cache and a
commitment); reclassifying `deflate-decompress@1` as
manifest-seekable so `produce_range` could window it (DEFLATE has no
manifest — a window into a deflate stream needs the decoder state at
that point, which is what preflate corrections ARE; that is a real
design, not a seek-class edit); caching the spill per blob instead
(cheaper in storage and genuinely attractive — it is `docs/views.md`
seekability rule 3's "cache tier", which does not exist in the code —
but a cache tier is its own ruling with its own eviction policy, and
inventing one inside a CLI flag is exactly the quiet settling this
repo's rules forbid).

*Amendment (2026-09-21, second): two defects found by the first live
run, and the reporting change that would have made them obvious.* A
`bless --materialize --min-size 16M --jobs 8` on the deployment reported
"examined 637, already blessed 261, blessed 376, nothing outstanding"
against a corpus that a straight SQL count put at 763 absent members of
16 MiB or more. Neither the number nor the word was right.

**Defect 1 — the goal state was read off the wrong artifact.** Triage
skipped any candidate with a sidecar, in BOTH modes. Blessing is indeed
finished when a tree exists; materializing is finished when the BYTES
exist, and a sidecar over absent bytes is not partial progress toward
that — it is the signature of a member some reader already paid a full
materialization for inside its own read, under the first amendment's
on-demand blessing. Those are by construction the members a client has
already proved are painful: 251 of them, including one of two distinct
276,826,264-byte blobs whose twin had been materialized and whose own
reads still hung for ten minutes. The one flag that would have fixed
them was the one that skipped them. The predicate is now per-mode, and
`materialize_does_not_skip_an_already_blessed_but_absent_member` pins it.

**Defect 2 — the size floor was exclusive.** The candidate query asked
`size > :min_size`, so `--min-size 16M` meant "strictly more than
16 MiB". On a rom corpus that is not an edge case: rom sizes are powers
of two, so the floor deleted a whole class. The floor is now inclusive,
and the chunk-group rule — blobs at or under one group have an empty
outboard by construction — is expressed by the caller passing
`GROUP_BYTES + 1`, not by an off-by-one in SQL. While there:
`blob.size` records STORE knowledge and a candidate's bytes are by
definition not local, so the size now COALESCEs over the recipe's
claimed output size, which is the only size an `ensure_blob` row ever
has.

**The paging was sound, and is now pinned rather than trusted.** The
keyset cursor is `blob_id`, which is unique, so a page boundary has no
ties to straddle; `keyset_paging_loses_nothing_across_boundaries_or_ties`
walks 300 identically-sized candidates at five page sizes and
`the_walk_covers_a_population_many_pages_deep` runs the pass itself over
9,001 candidates — three pages — and asserts every one was examined. The
remaining gap between two live runs (706 vs 637 candidates) is drift:
that host has a refine worker and a MAME verify running, and the
population moves.

**So the report stops inferring completion.** `Db::bless_candidate_count`
is a count twin of the paging query — literally the same SQL predicate,
in one string — read before the run and again after. The report carries
`population`, `population_after` and `walked_it_all`, "nothing
outstanding" is printed only when the walk covered the population it was
handed, and a run that did not says `INCOMPLETE WALK: examined N of M`
and exits 1. A non-zero `population_after` is NOT a shortfall and says
so: SQL cannot see a sidecar, so a blessing run leaves the count exactly
where it found it, and a materializing run leaves whatever the carve-out
declined.

*Ruled out, having looked:* `--min-size 16M` parsing (K/M/G are powers
of two, so 16M is 16,777,216 — and either reading would have WIDENED the
set, not narrowed it); `affine_carveout`/`plan()` silently declining
members whose container the swap evicted (a member whose container has
no rebuild route lands in `no_route`, which the run reported as zero,
and preflate-split members are resident anyway); and inconsistent
counting of a blob claimed by several recipes (the predicate is an
`EXISTS` subquery, so a blob appears once however many parent and clone
sets name it).

## D122 — The store is the authority on bytes; `blob.residency` is a repairable cache of it (2026-09-21)

Two subsystems answer "are these bytes here?": `Store::has`, which
stats a content-addressed path, and `blob.residency`, a column. They
are allowed to disagree, nothing said so out loud, and D121's
`--materialize` turned that silence into 250 wrong rows on the live
corpus — bytes durable on disk, the index calling them `Absent`.

**The invariant, stated.** The store is the authority. `blob.residency`
exists because the planner asks about millions of blobs at once and
cannot stat them all; it is a CACHE of the store's answer, and a cache
is allowed to be stale. Drift is permitted in exactly one direction and
repaired wherever it is noticed:

- **Index says absent, bytes are present** — safe, and repairable for
  free by anything already holding the hash. Serving is unaffected
  (`serve_range` keys off `store.has`), so the damage is confined to
  planning: GC and `evict` read a blob that costs nothing to keep and
  has nothing to reclaim, and D27's accounting is short by that many
  blobs. Any pass that notices MUST repair it rather than skip it.
- **Index says resident, bytes are gone** — unsafe, and not repairable
  by inference: it is either an eviction whose bookkeeping did not land
  or real loss. `scrub` already reports these as `missing` (D81: a
  deterministic conclusion about bytes is a verdict, not an `Err`), and
  that stays the only correct response.

**Why the two can drift at all, permanently and by construction.** A
blob becomes resident through two writes on two different media: a
filesystem `rename` into the store, and a SQLite row. There is no
transaction spanning those, and there cannot be. Every writer in this
codebase therefore has a window where the bytes are durable and the row
is not — D121's pass has a deep one (workers publish bytes, the
coordinator alone may write the `Db`, and up to a queue's worth of
finished jobs sit between them), but `ingest`, `replay` and the D89
extract path all have the same shape at smaller scale. The answer is
not to chase atomicity across media. It is to make the row DERIVED:
`datboi recover` already rebuilds the whole index from the store (D15),
and `scrub` already upserts `Residency::Resident` for every blob it
reads and verifies. This entry only names the rule those two were
already following and requires it of everything else.

**What repair may and may not claim.** Finding bytes at their address
proves residency and nothing else. A repair therefore writes residency
alone: NOT `verified_at` (the bytes were not re-hashed — `scrub` sets
that, and it pays a full read for the right), and NOT `ReplayedLocal`
on any producing recipe (D25's licensing is a claim about a ROUTE
replaying, which finding a file proves nothing about). The consequence
is deliberate and safe: a repaired-but-unlicensed blob is kept rather
than dropped, because `Db::is_evictable` still refuses it. Getting the
licence back means replaying the route — `datboi materialize`, or the
pass itself on a corpus where the bytes are genuinely absent.

**A failed index write must not discard durable work.** D121's pass
propagated an index error straight out of the run, which threw away
every finished-but-unretired job in the queue — on a host where the
refine worker holds the write lock long enough to exhaust SQLite's 5 s
`busy_timeout`, that is the normal case and not an exotic one. An index
write that fails now stops STAGING (the ENOSPC discipline) and keeps
draining, so the damage is bounded to what was in flight, the report
counts it as `unrecorded`, and the run does not call itself complete.

*Rejected:* a transaction spanning the store write and the index write
(there is no such thing across a filesystem and SQLite; pretending
otherwise would mean a journal, which is a second index to keep honest);
treating "bytes present, row says absent" as an error (it is the
expected residue of any interrupted writer, and erroring would make a
crash-recovery path noisy without repairing anything); repairing it
only in `scrub` (scrub pays a full re-hash of every blob it touches and
samples by hash prefix, so a corpus-sized drift costs a corpus-sized
read to fix something a `stat` already proved); setting `verified_at`
or licensing the route on repair (above — claiming evidence nobody
gathered); making `blob.residency` authoritative and the store derived
(inverts D15: the store is the durable artifact, the databases are
rebuildable from it, and that is the whole shape of recovery).

## D123 — A transport container is a choice, not a format property; drop is operator-invoked and dat-gated (2026-09-21)

Amends D35. D35 chose "containers-stay-literal with members-as-claims
(≈1.0× storage)" on 2026-07-03 and priced it in storage alone, which
was all it could price: serving did not exist yet (its own milestone
order is M2 shrink → M3 views/serving). Three costs fall outside that
measurement, and adopting a MAME set paid all three.

**The read tax.** A zip's DEFLATE member is `deflate-decompress@1`,
seek class **Opaque**, and `produce_range` excludes Opaque from the
random-access lane — every range read re-materializes the member from
byte 0. ~278 GB of work to read a 264 MB rom; one member held a serial
reader in uninterruptible sleep for over an hour. D121's
`--materialize` is the cure and it is a +133 GB one, because the
container stays.

**Unreclaimable containers, and a reclaim pointing the wrong way.** For
rar/7z the only recipe minted is `container→member` — the code says so:
*"makes the MEMBER evictable"*. The container has no reverse route (it
is not reconstructible, which is precisely why its members were
extracted), so the D21 fixpoint can never ground it. That is 2× storage
forever, and the only eviction on offer is *drop the roms, keep the
archive* — regenerating roms on demand through a wasm extractor at
`SeekClass::Opaque`, i.e. re-creating cost one.

**CDC is structurally blinded.** `ChunkAnalyzer` refuses non-resident
blobs ("chunking would materialize it"), and a zip's members are absent
by construction, so the dedup primitive has never seen a rom's
plaintext at all.

**What is actually wrong is the asymmetry, and that is what this fixes
unconditionally.** No dat names an archive. rar, 7z and zip are
*equally* absent from every dat, yet the manager treats them as three
different kinds of thing: zip members are claims over a retained
container, 7z/rar members are resident blobs beside a retained
container, and whether a container can ever be reclaimed depends only on
whether preflate (D53) happens to work on it. From a dat's point of view
all three are the same object: packaging someone shipped the roms in.
So retention stops being a property of the FORMAT and becomes one
choice, spelled the same way for all three — `datboi ingest --unpack`
at the door, `datboi unpack` for a corpus already inside.

**The default does NOT flip, and that is a ruling against the proposal
in open-questions.md.** Three arguments, in the order they weigh:

1. **Asymmetric regret.** Waiting costs nothing: a retained container
   can be unpacked at any later moment, by a command that exists now.
   Dropping is final — the archive is not reconstructible, that is the
   defining property of the thing being dropped. A default whose wrong
   answer is unrecoverable and whose right answer is merely deferred is
   not a close call.
2. **Dropping forecloses the strictly better outcome.** preflate gives
   members-resident AND a container reconstructible at ~0.002%
   corrections — both wins, no storage trade. You cannot preflate a zip
   you deleted. The backlog is slow (~2 members per 3 minutes against
   141,986), but that is a throughput problem with a throughput fix,
   whereas deletion makes the good outcome permanently unreachable for
   those zips. Extract-and-drop is the right answer for what preflate
   *refuses* (D53's coverage gap) and for rar/7z, which have no rebuild
   route at all and never will. That is a targeting question, not a
   default.
3. **D121, one day old, ruled the same shape.** "Materializing by
   default (a 133 GB residency decision is the operator's, and the flag
   is where they make it)." Unpacking the measured corpus is a **+66 GB
   logical / +44 GB on-disk** decision in the same direction — 35,494
   zips are 69.1 GB, their 134,307 distinct members are 135.2 GB,
   cross-zip sharing is only 1.13× because a split set gives clone zips
   their own roms, and the dataset compresses 1.28× while the zips do
   not. A default that makes the canonical corpus 95% larger has to be
   asked for out loud.

What is NOT claimed for retention: that it is cheap. The current middle
state — containers *and* D121-materialized members — is the worst of the
three, and an operator who has decided their corpus is a serving corpus
should run `unpack` and stop paying for both. The wins are real and they
are not storage: O(1) reads instead of quadratic, `deflate-decompress@1`
ceasing to be a *route* anyone serves through, and plaintext dedup
becoming measurable for the first time.

**Already-stored containers are converted by an explicit run, never
retroactively.** `datboi unpack` is D120/D121's shape — bounded worker
pool, one `Db` owner, keyset-paged, resumable, `--dry-run`, `--jobs`,
`--limit`, progress, and D121's honest-completion reporting (a count
twin of the paging predicate, `walked_it_all`, and no "nothing
outstanding" over a population it did not finish). Nothing in the daemon
does this on its own. The automatic half — unpack what refine has
*proven* unpreflatable, under a residency policy — is deliberately left
open, exactly as D121 left its own automatic half open.

**Dropping an uncoverable container is acceptable, and here is the
consent it needs.** This is the second byte-destroying code path in the
store (eviction, D25/D27, was the first) and it is a different act:
eviction drops a **covered** blob, where a replayed-local route grounded
in retained literals can bring the bytes back, and the D49 sidecar is
kept precisely so it still serves. Unpack drops an **uncoverable** one —
nothing brings it back, and its bit-exact reproduction is gone forever.
So it is fenced five ways, all enforced at the last moment before the
unlink:

- **Operator-invoked only.** A flag or a command. Never a watermark,
  never the daemon, never a side effect of anything else.
- **Never a blob a dat names.** A container any `rom_claim` reaches
  through `identity_blob` is content, not transport, and is refused
  outright. The measured claim is that no dat names an archive; this is
  the gate that makes the ruling safe if that is ever false for one dat.
  Pinned blobs (`pinned_reason`) are refused on the same footing.
- **Never before every member is durably resident.** The gate is
  index-driven, not extraction-driven: *every* blob claimed as an output
  of a non-Failed recipe whose sole input is this container must satisfy
  `Store::has` before the unlink. A member the re-parse failed to
  produce therefore blocks the drop rather than being silently lost.
- **Never something the sniff does not call transport.** Containerhood
  is decided by re-reading the head and asking `looks_like_zip` /
  `looks_like_7z` / `looks_like_rar` — the *same* predicate ingest used
  to decide it was a container. A second definition would drift, and the
  drift would be catastrophic: single-input recipes also describe D9
  detector variants and every D111/D114/D115/D116 disc decomposition,
  and those inputs are dat-named discs.
- **`--dry-run` states the bill first**, in both directions: container
  bytes reclaimed and member bytes added.

**Provenance survives, and the dangling recipes are how.** `source_file`
records "these bytes arrived as roms/pac.zip" — byte-provenance, not a
cache — so the container keeps its blob row, its alias tuple and its
`source_file` link. Residency goes to **`Absent`**, not
`EvictedCovered`: there is no covering route and the enum must not say
there is. The container is then out of the GC orphan predicate (which
requires `residency = 0`), so the record of where the roms came from
cannot be swept.

**The `container→member` recipes are KEPT, unchanged.** This is the
ruling the question asked for, and the reasons are three:

1. **They are the provenance edge.** After the drop, the recipe row is
   the only thing tying rom `pac.6e` to `roms/pac.zip`. Delete it and
   "where did these roms come from" becomes unanswerable for every
   unpacked member — which contradicts the requirement that dropping the
   container must not erase where the roms came from. The typed
   acquisition-event table that would carry this instead is an owed
   piece of work (open-questions.md), not something to invent inside
   this command.
2. **They already cannot be mistaken for a live route, mechanically.**
   `Executor::plan` returns `Plan::Literal` for a resident member before
   it ever reads `recipes_for_output`, and a plan that *does* reach the
   recipe recurses into the container, finds no bytes and no producing
   recipe, and fails `NoRoute`. Every grounding mode seeds
   `temp.grounded` from `residency = 0`, so an `Absent` container is
   never grounded, the recipe never fires, and `is_evictable` is false
   for the member — which is correct: an unpacked member is the only
   copy of those bytes and must not be evictable. The blessing predicate
   never sees either end (members are resident; containers have no
   producing recipe). Tests pin all of this rather than leaving it to be
   re-derived.
3. **Deleting them would not even be durable.** `datboi recover`
   rebuilds the recipe index by walking `meta/` and re-indexing every
   recipe object it finds (D15). Deleting the index rows means deleting
   the recipe *objects*, which is a second byte-destroying act, on the
   one namespace the whole index is derived from, to remove rows that
   are both true and inert.

**What is destroyed is bytes, not the record.** After an unpack the
index still says: this blob existed, here is its alias tuple, it arrived
at this path, and these members came out of it. Only the archive's bytes
are gone.

*Rejected:* flipping the default to extract-and-drop (argued above —
asymmetric regret, forecloses preflate, and a +66 GB decision on the
canonical corpus belongs at a flag; the proposal's own constituency for
retention, TorrentZip-verified distribution, is exactly the population
preflate covers at 0.002%, which is a reason to keep the container long
enough for refine to reach it, not a reason to delete it); retroactively
unpacking existing containers on upgrade (a byte-destroying migration
nobody typed); marking the dangling recipes `Failed` (`Failed` is
terminal poison meaning *these bytes came out wrong* — D48/D81
vocabulary — and `fail_error` would have to hold a sentence that is not
true; it would also make `rehabilitate` a permanent re-failure loop);
deleting them (above); `Residency::EvictedCovered` for the dropped
container (it asserts a covering route that by construction does not
exist, and it would make `datboi status` report reclaimable-and-
rebuildable bytes that are neither); letting the daemon or a watermark
unpack (D27's planner destroys covered bytes; this destroys uncoverable
ones, and no automatic policy has been written that could license that);
skipping the store write for the container at ingest under `--unpack`
(the container is written and then dropped, which costs one transient
copy per file — bounded per file, never per corpus — and buys ONE code
path shared by both doors, with identical ordering and identical crash
behaviour; two pipelines, one of them untested by the deployment that
matters, is D120's rejection and it applies here); minting no
`container→member` recipe under `--unpack` at ingest (it would make the
two doors converge on *different* graphs, and it would throw away the
provenance edge at exactly the moment it is created); a `--min-size`
floor on containers (the read tax is per-member and the storage trade is
per-container; there is no one number, and a zip's members are not
independently droppable anyway — the gate is all-or-nothing by
construction).

## D124 — CHD hunk decomposition is not built; FastCDC already generalises it (2026-09-21)

With the `chd-verify` sweep landed (D44 amendment), the remaining CHD
question was storage: decompose each file into hunk blobs plus a
reassembly recipe, so hunks shared between CHDs are stored once. Ruled:
**not built**, and the reason is not "unmeasured" — it is that the
design is strictly dominated by something already running.

**The win it could deliver is a subset of what `chunk` already finds.**
A repeated compressed hunk is a repeated run of bytes. FastCDC over the
CHD as opaque bytes finds repeated runs of bytes *at content-defined
boundaries*, so it catches every hunk-aligned repeat plus every repeat a
fixed 19584-byte grid would miss. CHDs are over the 4 MiB chunk
threshold by orders of magnitude, so the fallback family already covers
this corpus. Hunk decomposition would add a second, weaker mechanism for
the same saving.

**And the alignment it depends on does not survive the cases that
matter.** CHD hunks sit on a fixed grid at fixed offsets. Two CHDs share
a hunk blob only if the same bytes met the same codec at the same
settings on the same boundary — which holds exactly when the two files
are byte-identical, and whole-file dedup already handles that for free.
Between a disc and its re-dump by a later chdman, the per-hunk codec
choice and the codec's own settings both move; between regional
variants, one inserted byte shifts every subsequent hunk off the grid.
Repetition *inside* one file is already free too: CHD's own
`COMPRESSION_SELF` map entries point a repeated hunk at an earlier one,
so a disc's padding costs one hunk, not N.

**The index cost is not small.** 522.5 GB of CHD across 767 files is
~26M hunks at CD hunk size before counting the hard-disk sets — a blob
row and a recipe input per hunk, against an index that holds hundreds of
thousands of blobs today. A hundredfold index for a saving predicted
near zero, and a non-seekable rebuild path for files that are currently
plain literals.

**Serving is not a motivation here, and that is the load-bearing
difference from D35.** CHDs were ingested as literals, so a range read
of one is a file read. The zip-member problem (a range read that has to
inflate a member) does not arise, so none of D121/D122's urgency
transfers.

**The measurement, and what would reopen this.** The harness is in the
tree — `cargo test -p datboi-formats --test chd_dedup -- --ignored
--nocapture` with `DATBOI_CHD_DIR` — and it reports the number that
matters: bytes saved ACROSS files, separated from the within-file
repeats CHD already handles and with byte-identical files excluded so
blob-level dedup cannot flatter the result. It could not be run here:
this work had no access to the live corpus, by instruction. Reopen if
cross-file saving exceeds a few percent of hunk bytes AND exceeds what
the `chunk` family is already claiming on the same blobs — the second
half is the real test, because the first alone would be re-finding
savings we already have.

**The decompressed variant is a different and much larger project.**
Storing inflated hunks is where real dedup lives (transforms.md ranks it
third), but serving a `.chd` again needs byte-exact recompression, which
that same table records as NOT reproducible across chdman versions. It
would want a preflate-shaped corrections lane per codec — for zlib, for
LZMA, and for FLAC, which has no preflate analogue — on top of a corpus
that inflates by the compression ratio. The harness prices both halves
(`DATBOI_CHD_DECOMPRESS=1` reports the inflated resident size against
today's); nothing should be built until it has.

*Rejected:* building it and measuring after (the measurement is cheap
and the build is not); hunk-as-blob "because it is the obvious
decomposition" (obvious for a format with content-defined or
uncompressed pieces — CHD is neither); treating per-file hunk repetition
as the win (the format already collapses it).


## D125 — Analysis candidacy: only a named blob is a document; extents are nobody's candidate (2026-09-21)

`Db::enqueue_unanalyzed` selected EVERY data blob for EVERY analyzer,
and `ChunkAnalyzer` *mints* data blobs. Measured on the live host after
one chunking pass over 767 CHDs: 1,055,274 chunks minted, data blobs
1,171,000 → 1,871,511, `sweep_queue` 1,700,000 → 12,227,830 — of which
11,360,339 rows (93%) ask a question about a chunker output. The
refiner's overwhelming majority occupation became asking whether a
256 KiB rolling-hash cut is a Wii disc, a GameCube disc, a NARC, an
ISO9660 volume — a million times each, to record foregone conclusions
— and every future chunking pass multiplies it again.

**D47 does not require the cross product.** Its hard rule is that
*catalog contents* never influence what gets claimed, so that instances
holding the same bytes converge on the same claim set. A predicate over
a blob's OWN index facts — its size, its residency, the recipe edges
around it — is identical on every instance holding the same bytes and
the same graph. "Analysis must not depend on which dats are loaded" is
a much narrower claim than "analysis must consider every blob for every
analyzer", and only the first is ruled.

**Level 1, structural and global: documents versus extents.**
A **document** is a blob some producer NAMED — a thing in the source's
own vocabulary. An **extent** is a blob that something CUT OUT and
nobody named: boundaries from a mechanism indifferent to the content's
structure, the FastCDC chunk being the pure case — a rolling-hash cut
point that exists only to make another blob cheaper to store. Extents
are nobody's candidate: nothing true of an arbitrary byte range is
better said of it than of its parent, and D108 already guarantees every
structural family concluded on the parent WHOLE before the fallback
chunker cut it.

The discriminator is already in the data and needs no new vocabulary:
`extent := generated OR (is-a-part AND unnamed)`.

1. **is-a-part** — some recipe consumes this blob to build an output
   STRICTLY LARGER than it. That is D112's view/decomposition
   comparison read the other way round: an input at least as large as
   the output is the WHOLE (a container, whose members are slices of
   it), and only a smaller input is a piece being assembled into a
   whole. Being cut out needs positive evidence, so a blob with no
   edges at all — a peer-fetched rom, a bare claim — is NOT an extent,
   and neither is a container that arrived without a `source_file` row.
   Unknown has to fail toward doing the work, and this term is what
   makes that true.
2. **unnamed** — no `source_file` row, and no NAMED `recipe_output`.
   Every structural splitter in the tree names its outputs
   (`Some(piece.name)`, `Some(view.name)`,
   `Some("{prefix}/body.bin")`), and ingest's own `zip_member_recipe`
   names every zip member — which is why a `preflate-split` member
   plaintext, a rom someone shipped and possibly a container itself,
   stays a candidate while the raw deflate stream beside it does not.
   (The plaintext is usually LARGER than the stream it feeds, so term 1
   already spares it; the name is the second, independent guard for the
   incompressible member where it is not.) `ChunkAnalyzer` alone names
   nothing: its chunks appear only as `InputRef { role: None }` and its
   one `OutputRef` is the reassembled original, unnamed.
   One edge worth stating rather than rediscovering: term 1 inverts for
   COMPRESSION. A zip is smaller than the members it holds, so its
   member recipes have outputs larger than the container and term 1
   calls it a part. Term 2 is what keeps it a document, and it holds
   for every custody door there is — each one runs through `Ingester`,
   which writes the `source_file` row — and for a nested container,
   which is its parent's named member. A future door that minted
   `container→member` recipes without either would need one.

3. **generated** — a zero-input recipe's output. D111/D112's junk and
   filler streams ARE named (`Some("gc-junk")`) AND they span their
   disc's whole address space, so neither other term catches them;
   analysing one is as pointless as analysing a chunk and would
   materialise a disc-sized PRNG expansion through the executor to do
   it. This term reuses D112's own zero-input-route predicate, now
   factored out so the swap and the queue cannot drift apart on what
   `generated` means.

**Level 2, semantic and per-analyzer:** `Analyzer::candidacy()` returns
the necessary conditions this analyzer's candidates must meet, ANDed
into the enqueue. Shipped here are the index-only ones — `min_size`
(the smallest blob this format can be) and `resident_only` (`chunk`
mints resident chunks, so chunking an absent blob would materialise it,
the opposite of the dedup goal). A condition must be NECESSARY, never
merely likely: a blob it excludes is one the analyzer would have
concluded Negative about without reading a byte, so the D24/D48 record
loses nothing — a negative that was never in doubt was never worth a
row. Unlike a recorded negative, a predicate is re-evaluated free on
every wake, which FIXES a real bug: `chunk`'s "not a resident literal"
negative was permanent, so a blob that became resident later was never
chunked.

The cheap-sniff half of Level 2 — where a lepton-on-JPEGs analyzer
declares "JPEG-shaped" and the Wii splitter "magic at 0x18", so that
format-specialised analyzers scale as they arrive — is ruled as the
layer and NOT built here: a sniff needs bytes, and enqueue is one SQL
pass over ~1.9M rows, so it belongs at CLAIM time over the head the
executor can already produce. Recorded in open-questions.

**Measured on a synthetic corpus** (two 6 MiB near-twins, chunked, then
the whole roster asked to enqueue both ways): 24 chunks minted, roster
queue **258 rows under the cross product, 18 under candidacy — a 93.0%
reduction**, the same fraction the live host's 11,360,339-of-12,227,830
is. Chunking now SHRINKS the queue (the two originals settle for
`chunk` and nothing takes their place) where before it multiplied it.

**Enqueue also prunes.** `enqueue_unanalyzed` deletes unleased queue
rows whose blob no longer satisfies candidacy, in the same call that
inserts — and it is the only writer the queue has, beside `enqueue_fresh`
under the same predicate. A live database therefore converges on its
next ambient refine wake with no operator action; nothing about the
12.2M rows needs a migration, a flag, or a hand-written DELETE. The
one-time bill is one predicate evaluation per queued row per family,
inside the refine worker's wake — tens of seconds at 12.2M rows, on the
niced thread, holding the cache.db write lock in bursts. `datboi sweep
<family>` reports it as `N pruned` for an operator who wants to watch
it happen family by family instead.

*Rejected:* "don't analyze analyzer-produced blobs" (too broad, and
wrong in the one direction that matters — a `preflate-split` member is
analyzer-produced and is exactly a rom that may match a dat and may
itself be a container); a `blob.piece` column or a cache table
recording the fact at mint (D18/D79 rule blob meaning out of the row
and into the edges, and a cache table that cannot be rebuilt from CAS
bytes breaks D15 — the queue would re-explode after every `recover`);
marking the chunker's inputs with a new `recipe_input.role` (right in
principle, `"skeleton"` is the precedent, but the mark changes the
recipe's bytes, so it costs a `fastcdc/2` identity and a full re-read
of every chunked container to converge — and the named-output test
reads the same fact off the recipes that already exist); a
recipe-SHAPE test for the chunker's assemble (an affine `assemble@1`
over role-less inputs with an unnamed output — it works, but it admits
the preflate skeleton, the corrections blob and the raw member streams
that the named-output test correctly excludes, and it reads the
chunker's implementation where the name test reads the producer's
intent); the name test ALONE, without term 1 (simpler, and
wrong at the edges that matter — a peer-fetched rom and a container
that never had a `source_file` row would both be called extents on the
strength of a name nobody had any reason to give them); a cheap sniff gate before enqueue (a read per
blob per wake, ~1.9M reads, to answer what the index answers for free);
leaving it to scheduling (D47 permits dat-aware ORDER and
`bump_dat_matched_priorities` already runs — ordering cannot fix a
queue that grows tenfold with every chunking pass).

## D126 — A trapping route disproves the route, not the bytes; the read path records it (2026-09-21)

Observed on the live host, repeatedly: `preflate-rs-0.7.6/src/
tree_predictor.rs:169` PANICS inside `xf-preflate`'s `recreate`,
wasmtime traps it, and the executor's streaming path flattens it into a
bare `io::Error` carrying a string. Every consumer then loses the one
fact that matters — that the same bytes, through the same pinned
component, at the same fuel budget, will fail identically forever. A
sweep item whose bytes exist only behind that route therefore errors
*environmentally* (D81) and is retried on every ambient wake; the
`analysis` table holds no trap row at all, only successes. The same
failures stopped 77 routes in a `bless --materialize --min-size 1M`
run, and would stop the same 77 on the next run, and the next.

**D53's "the failure is a clean error" is half right, and the half it
gets wrong is the half that is failing.** `preflate-rs` DOES cleanly
error at SPLIT time when its complevel estimator finds no candidates —
`SplitReader::fail` catches it, the analyzer records a D48 negative, the
container stays literal, and that sentence stands. The panic is a
different input class reached at REBUILD time, inside the guest. A trap
is not an error, and D53 is amended to say so.

**Ruled, in three parts.**

1. **A claim-level failure keeps its identity across the pipe.**
   `PipeHandle::fail_deterministic` marks the producer's verdict and
   `pipe::deterministic_cause` reads it back out of the `io::Error`
   chain, so the distinction survives the thread boundary that D51's
   composition puts between a guest and its consumer.
   `ExecError::is_claim_failure` — D25's existing predicate, which
   already calls a non-fuel trap a disproof — gains the
   wrapped-in-I/O case, because a spill turns a nested node's trap
   into `ExecError::Io`. Fuel exhaustion stays retryable, exactly as
   D25 already rules.

2. **The record is a poisoned recipe, not an analysis row.** The route
   claims it produces those bytes; it does not. That is D25's `Failed`,
   the same verdict `replay` and `license` already write for the same
   trap — now written on the streaming READ path too (the sweep's
   logical open, and the bless coordinator) instead of being discarded
   because the reader happened to be a `Read` rather than a replay. One
   poison settles the blob for ALL TEN families at once: a poisoned
   route stops grounding it, so `refresh_absent_eligibility` stops
   admitting it and `bless_candidates` stops selecting it.
   `datboi scrub --rehabilitate` stays the escape hatch for a wrong
   poisoning, unchanged.

   Two consequences worth naming rather than discovering. A spilled
   route that RUNS to completion and produces bytes that are not the
   item's is the same verdict — it is `StoreError::HashMismatch`'s
   shape, which `is_claim_failure` has always called a disproof — so it
   poisons too. That was once dangerous: a guest trap could race the
   pipe into a clean-looking short stream and poison a good recipe
   ("spill produced 0 bytes"). The pipe's finished-verdict wait fixed
   that race, and this ruling leans on the fix. And `open_stream` is
   `open_stream_route` now, so the p2p serve path and the swap's packer
   record a disproof too — right in both places, for the same reason:
   a route that traps on open is not a network hiccup.

3. **The item waits; it does not conclude.** The analyzer never read
   the bytes, so `Negative` would be a false statement in a signed,
   restorable artifact — `AnalysisRow` is defined as "what `analyzer`
   concluded about `blob`'s bytes" (D48), and the row is snapshotted,
   so an instance that later holds the literal would import our row and
   permanently decline to look. The right vocabulary already exists:
   D116's deferral. The item leaves the queue with NO analysis row,
   waiting on its own hash, and `enqueue_unanalyzed` re-admits it the
   moment those bytes are resident. Settled for scheduling, open for
   truth.

**The line stays where D81 drew it**: would the same input produce the
same verdict. Out of disk, a missing component, an I/O error on the
byte source, instantiation and world-wiring failures, and fuel
exhaustion are all still `Err`, still queued, still retried.

**On "more fuel might succeed".** The obvious objection is answered,
but not by analyzer versioning — `analyzer_tag(VERSIONED_NAME)` governs
the ANALYZER's identity, and the thing that traps here is a RECIPE's
pinned component, which analyzer versioning does not reach. It is
answered by `is_fuel_exhaustion`, which D25 already wrote for exactly
this reason: a budget outcome is a policy outcome and never poisons, so
a retune can still rescue the recipe, while a panic — which no budget
changes — does.

*Amendment (2026-09-21, the wrapper sweep — this ruling did not fire):*
the first cut unwrapped the verdict out of `ExecError::Io`, which is the
shape `spill` produces, and the pass that motivated the whole ruling
does not spill. A live `bless --materialize --min-size 1M` reported
`"poisoned":0` beside the same 77 failures. `obao::compute` reads a
whole route, so a trap arrives as `Store(Obao(Io(Deterministic)))`
materializing and `Obao(Io(Deterministic))` blessing — neither matched,
and `is_claim_failure` fell through to `false`. Naming wrappers one at a
time was the wrong repair: the predicate now WALKS the source chain
(`deterministic_in_chain`, with the `io::Error` special case, since
`io::Error::source()` returns its payload's source rather than the
payload), so a consumer that boxes a route failure a fourth way is
covered without a fourth arm.

The sweep that found it found a second gap and one lossy site worth
naming. (1) `bless_plan` STRINGIFIED its `ObaoError` into
`Malformed(format!(..))`, destroying the chain and poisoning
unconditionally — wrong in both directions at once, since a bad disk
would poison a good recipe. It now carries the error as a source
(`ExecError::Obao`), so the walk decides. (2) A blessing pass whose
whole-route re-hash disagrees with the claim IS a disproof, and it
reported `RangeVerifyFailed`, which this predicate refuses ON PURPOSE
because `serve_range` uses that same variant for a seekable
component's lying window — where the doctrine is to quarantine the
seek claim, not poison the recipe (D49 rule 3). Two different failures
sharing one variant, and the blessing one was getting the serving
one's answer: reported, never recorded, repeated every run. It is now
`ClaimMismatch`, matching what `put_with_obao` already reaches on the
materializing twin. Left alone, and noted: `TransformRandom::read_at`
flattens a `RuntimeError` into a string on the `serve_range` path —
lossy, but that path's answer is the seek quarantine, not a poisoning,
so nothing there wants the verdict.

*Rejected:* recording the trap as a D48 `Negative` on the swept blob
(it asserts a conclusion about bytes nothing read; it rides the
snapshots; and it has to be paid once per family — ten trap executions
per blob — where one poison settles all ten); treating a guest panic as
fuel-retryable (`is_fuel_exhaustion` already separates the two, and
conflating them would make every real disproof retry forever, which is
the bug); a retry counter or backoff on the queue row (it converts a
permanent, knowable verdict into a slower permanent verdict, and adds
authoritative state to a queue the schema calls derivable); poisoning
the exact node that trapped rather than the top route (the streaming
composition does not know which thread failed by the time the reader
sees it, and `replay` already poisons the top route for a claim failure
anywhere in its tree — matching it is consistency, not a compromise).

## D127 — A READDIR cookie names the snapshot its walk is reading (2026-09-21)

`ls` on a view root does not terminate. Measured on bagel against
`/mnt/arcade-view`, ~36,833 entries: 2,067 READDIR calls and 204 MB
received for a directory whose honest enumeration is 288 calls and
4.0 MB, zero timeouts, ~62 ms per call — the server answering promptly
and wrongly, roughly fifty times over, while the client sat in `D` at
`rpc_wait_bit_killable` ignoring everything but SIGKILL. Two defects,
each survivable alone, jointly non-terminating.

The load-bearing one is an identity mismatch this log already contains
half of. D33 keys everything beneath a view `(snapshot, path)` and
names the view directory itself by view — correct, and the reason an
`eval` mid-read never changes bytes under a held id. READDIR is where
the two classes meet: the directory is named by view, the cookies it
hands out are snapshot-keyed. It re-resolved the view by NAME on every
call, so a flip between call K and K+1 moved the enumeration to a tree
in which no outstanding cookie existed, and the only answer left was
`NFS3ERR_BAD_COOKIE` — whose only legal client response is to restart
the walk from zero. Every restart was itself long enough to catch
another flip. The fix needs no state, because the cookie IS a fileid
and a fileid already resolves to a node: a view root's children are
`Node::Path(snapshot, name)`, so the snapshot a walk is reading is
recoverable from the walk's own cookie. `start_after` names the tree;
only a first page (cookie 0) or a cookie not shaped like a view root's
child resolves the view by name. That pins an enumeration to the
snapshot it started on for free, and it does not weaken D33 — it is
D33 applied one level up: an in-progress READDIR is a held id like any
other, and a fresh walk still sees the new tree on its first page.

The second defect made the window wide enough to hit. Every call
rebuilt the entire child list from the index — walking the manifest,
allocating an owned `String` per entry, sorting, then LINEAR-SCANNING
the result for the resume cookie — to return one page and discard the
rest, so listing N entries in pages of P cost `N/P` × `O(N log N)`.
Listings are now built once per `(snapshot, path)` and kept, with a
`fileid -> position` index so resume is a hash lookup. A snapshot is
immutable, so such a listing can never go stale and needs no
invalidation; the export root, whose children are the mutable `view/`
tags, is the one directory still built per call (it is O(#views)). The
cache is a performance device only and must stay one: `IdTable` mints
ids deterministically, so a rebuild after a wholesale drop yields the
identical entries under the identical cookies. Measured over a
synthetic 36,833-entry view root, pages of 128: 288 calls and 4.0 MB
before and after, 288 child-list builds and 3.12 s of server CPU
before, 1 build and 64 ms after (48×). Pages after the first now touch
neither the tag row nor the store.

*Rejected:* caching the listing WITHOUT pinning the snapshot (the
cheap half of the work and none of the termination — a flip still
invalidates every cookie, and a faster walk that restarts forever
still never finishes); a server-side enumeration handle keyed by
client (state in a protocol that has none, and it would have to expire
on a timer that is another way to strand a walk); resolving the view
once and freezing it for some interval (picks an arbitrary staleness
window and still strands whoever crosses it); gating the pin on the
pinned snapshot's `view_name` matching the view (it fails a renamed
view for no gain — a cookie from an unrelated tree can already be
handed to READDIR on that tree's own directory id, so the pin grants
nothing the id table did not already); streaming pages straight off
the manifest's `BTreeMap` range instead of materializing a listing (a
directory's name order and its rows' path order genuinely differ —
`Alpha.txt` sorts before `Alpha/` — so the range walk cannot emit the
name-sorted order pagination is specified against); an LRU over the
listing cache (a wholesale drop at a byte ceiling is what `manifests`
already does, and correctness does not depend on the cache surviving).
