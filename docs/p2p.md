# P2P & sharing

*From research pass R4. Decision D8.*

## iroh

iroh 1.0 (June 2026): wire-protocol stability across v1, QUIC + ed25519
NodeIds, NAT holepunching with relay fallback, pluggable discovery (DNS,
Pkarr/Mainline, mDNS). iroh-blobs = blake3 + bao verified streaming with
verified range requests — a ready-made CAS wire protocol aligned with our
D2 addressing. Tickets (NodeId + addrs + hash/hashseq root) are the
share-with-a-friend UX. iroh-gossip/docs are community-tier; library
metadata sync is an app-level protocol, not iroh-docs.

## Trust sequencing (D8)

1. **Friends plane (first)**: peer = NodeId (pubkey), ACL = list of
   NodeIds. The instance publishes a *signed* `(dat_hash → blake3)` mapping
   table; friends trust the signature. Recipe claims from friends follow
   the D4 lazy-verify policy.
2. **Public swarms (later)**: waddup-style ZK proofs ("I know F such that
   sha256(F)=dat_hash AND blake3(F)=swarm_hash", STARK→Groth16, ~256 B
   proof, ~1 ms verify) replace trust in anyone's mapping — strangers can
   join a swarm knowing only the dat hash. Same "expensive once, verify
   forever" slot can later cover recipe claims (H(f(x))=y) too.
   Known bottleneck from waddup zkp-benchmark: blake3-in-zkVM is ~22×
   slower per byte than the sha256 precompile — proving cost for large
   roms is the constraint to engineer around.

Human/web auth and daemon↔daemon identity are separate planes; the server
identity keypair doubles as the iroh key.

## Sharing model (D34)

Two primitives:

1. **Tickets** — immutable shares: NodeId + addrs + ViewSnapshot hash.
   A shared snapshot can never change under the recipient; no refresh
   semantics exist. Recipient pins/materializes per their own policy.
2. **Channels** — mutable named pointers:
   `(instance key, name) → snapshot hash`, signed, monotonic sequence
   number (rollback protection). Friends **subscribe** (pull-based:
   gossip/poll-on-connect); on head movement their daemon reacts per
   *subscriber-side* policy: metadata-only | on-demand | full mirror
   (prefetch + pin as GC root).

Channel promotion policy is per-channel: **holdings channels** (dir2dat
inventory, D29 — "everything I have, verified") auto-promote (inventory,
not curation; staleness is strictly worse). **Curated channels** (manual
promotion — moving the head is a push into subscribers' storage) are a
*later* feature, not v1.

Peer-availability is a first-class completeness state: subscribing to a
friend's holdings lets reports distinguish
`have(verified) / have(claimed) / available-from-peer(X) / missing`.

## M6 design — from the iroh spike (2026-07-16)

The spike (`crates/datboi-p2p`, a standalone/excluded workspace like the
wasm components) put iroh-blobs up and moved a blob between two instances,
and settled the two facts the rest of M6 rests on. Ratified as **D97**.

### Stack & versions

- **iroh 1.0.2**, **iroh-blobs 0.103**. iroh 1.0 froze the wire protocol
  across the v1 line (the decades-scale bet R4 wanted). iroh-blobs stays
  0.x but its *format* — blake3 + bao verified streaming, `.obao4` at a
  16 KiB chunk group — is exactly ours; the 0.x churn is API, not bytes.
- **Terminology drift**: iroh 1.0 renamed `NodeId → EndpointId` and
  `NodeAddr → EndpointAddr`. This doc's older R4 prose says NodeId; read
  it as EndpointId. The ed25519 instance identity (D8/D15,
  `datboi-core::identity`) is the iroh `SecretKey` directly — one keypair
  signs snapshots AND is the endpoint identity, no second secret.

### obao reuse is real, not aspirational (proven)

D52 froze `.obao` as headerless pre-order obao4, 16 KiB groups, "so the M6
p2p layer serves our sidecars unchanged." The spike *checks* this: an
outboard built at iroh's block size over the D52 golden input hashes to
the byte-for-byte golden the store test committed. So a resident blob's
existing `.obao` sidecar (D49 keeps it even after the literal evicts) is
already the tree iroh needs to serve verified ranges — we compute nothing
extra to publish a blob to a peer, and a peer-supplied outboard is
self-authenticating against the root (D49 rule 1), so it needs no trust
machinery either. The two stores' outboards are the same artifact.

### Fronting the store: our own handler over the LOGICAL CAS

D14 assumed "our store will speak iroh-blobs' irpc store protocol." That
seam is gone: **iroh-blobs 0.103 has no public store trait** — the store
is a concrete actor API (`fs`/`mem`/`readonly_mem`), so we cannot hand
iroh our sharded loose-file CAS by implementing an interface. The M6 shape
is instead **our own `ProtocolHandler`** on the blobs ALPN (or a datboi
ALPN) that answers bao get-requests by reading from datboi, reusing
iroh-blobs' `protocol` request/response types and `get` client on the
requester side. iroh-blobs remains the wire protocol and the downloader;
the provider's storage backend is ours. (If n0 later re-exposes a store
trait, we adopt it — but the handler is the load-bearing plan.)

Crucially the handler serves the **logical CAS (D92)**, not just resident
literals — the same "existence is groundedness" line audit, serving, and
analysis already draw:

- **Literal blobs** stream from `Store::get` (loose files *and* D91 packed
  windows fall through transparently) with the resident `.obao`.
- **Virtual blobs** — grounded but evicted, reconstructible only through a
  recipe — materialize through the executor's verified stream (D25/D49,
  the same path serving and analysis use), verified against the retained
  `.obao`. A peer asking for a ROM we hold only as a recipe gets the bytes;
  it never learns whether we had them resident. Residency is the planner's
  private knob (D91), invisible on the wire — the p2p surface is the audit
  surface.

D49's mandate carries over unchanged: every served range verifies against
the output outboard before bytes leave the box, so a seek-path bug or a
lying recipe surfaces as a refused transfer, never as bad bytes to a peer.

### Beyond stock blobs: dedup-aware partial transfer

Stock iroh-blobs transfers *one blake3 at a time*, and within a blob it
already does verified partial/range/resumable/multi-provider fetch for
free. What it cannot see is datboi's **cross-blob structure** — and that
is exactly where our storage wins live. D91: MKDS USA↔EUR share **556 of
564** NitroFS pieces; the variants differ by ~8 pieces (~1.3 MiB) out of a
~64 MiB ROM. A peer holding USA can hand a peer wanting EUR the whole ROM
(64 MiB) — or, if the two can agree on *which pieces differ*, just the ~8
(1.3 MiB) and let the EUR side reconstruct the container from its affine
`assemble` recipe (which it already has from the dat; D91 pieces are
grounding leaves).

Agreeing on the differing set without listing every hash is **set
reconciliation**. Two candidates, both surveyed in the spike:

- **Rateless IBLT** (SIGCOMM 2024) — sender streams coded symbols encoding
  the set difference; receiver decodes once it has enough. Near-optimal
  comms from a difference of one to millions, one round, adversary-robust.
  This is the "set sketch" instinct made precise.
- **Range-based set reconciliation** (what Willow/iroh-docs use) —
  recursive range refinement over an ordered hash domain; multi-round but
  dead simple and already in the iroh ecosystem.

The datboi-specific insight is *what set to reconcile*. Not whole-ROM
holdings (that is the D34 holdings channel, a coarser layer) but the
**piece / grounding-leaf set** — the sealed-pack pieces (D91), CDC chunks
(D59), and interior-decomposition members (D83/D94) that our recipe graph
already factors ROMs into. Reconcile pieces, fetch only the missing ones
as ordinary bao blobs (transport = stock iroh-blobs, verified), rebuild
locally through recipes we already hold. The recipe graph turns "send me
Mario Kart EUR" into "send me these 8 pieces" — content-defined,
dedup-aware transfer that a per-hash blob protocol structurally can't
express. This is the headline reason M6 is *ours* and not just a
dependency bump.

*Ruled 2026-07-17 as D100 (next section):* the algorithm is rateless
IBLT (our port); the set is the affine-RECIPE set, which dissolves the
inventory-vs-manifest privacy question into asymmetric reveal. How
manifests ride the D34 channel remains open with the channels work.

### Reconciliation: the recon ALPN (D100)

A second `ProtocolHandler` on `datboi/recon/1` beside the blobs seedbox.
The design inverted one working assumption: the reconciled set is the
**affine recipe set** (meta-blob hashes of non-Failed builtin
`assemble@1` rows), not the piece set. Recipes run ~1 per container vs
hundreds of pieces; once the initiator holds a peer's recipe, its missing
pieces are a *local* closure walk (recipe inputs ∉ my grounded set,
recursing through usable local routes). Reconcile the plans; the parts
follow by local math. The fetched diff is still the D91 pieces.

- **Codec**: `datboi_p2p::riblt` — our `[u8;32]`-specialized port of the
  SIGCOMM 2024 reference Go implementation (no viable Rust crate exists),
  SipHash-2-4 keyed checksum, streaming decoder. Differential-tested
  byte-for-byte against committed golden vectors from the vendored Go
  reference.
- **Roles**: the responder streams coded symbols of its scope (a
  stateless incremental stream); the initiator decodes against a local
  prior that never crosses the wire and sends a stop byte when decoded.
  Privacy is this asymmetry: the answering party is the consenting party;
  the initiator reveals only the scope request and a diff-size bound.
- **Wire**: request = 1-byte scope tag; response = `u64 LE` responder set
  size, then 48-byte coded symbols (32 XOR-sum ‖ 8 SipHash-sum LE ‖
  8 count i64 LE) in batches, stop-checked, capped responder-side.
- **Sync flow** (`datboi_p2p::sync`): reconcile recipes → fetch missing
  recipe blobs over the blobs ALPN (CasProvider serves Meta too; bytes
  verify against their own hash) → `index_recipe` as `source=Peer`, born
  `Pending` (D4/D8 lazy-verify: grounds nothing until replayed, poisons
  itself at rebuild if lying) → local closure walk → fetch missing leaves
  into iroh staging (D98), import `put_with_obao` → wants materialize
  through the executor (Pending→ReplayedLocal on verified replay). Empty
  want-list = mirror mode (fetch the whole diff, explicit never default).
- **Savings are first-class output** (the D97 observability requirement):
  named numeric tracing fields — set sizes, symbols received, overhead
  ratio vs the ~d minimum, pieces/bytes fetched vs bytes rebuilt,
  savings pct — INFO summaries, DEBUG per-piece verdicts (D81).

### Receiving: iroh stages partials, our CAS ingests completions (D98)

Fronting (above) is the SEND side — no partial state, bytes are already
whole. The RECEIVE side is where partial state is unavoidable (a multi-GB
ROM arrives incrementally, resumably, from several peers). Our `Store` is
complete-blobs-only by invariant (D14 stage 1: single-writer, atomic
rename, a file is the whole verified blob or absent) and must stay that
way — D15/D19/D49 all lean on "a present file is whole and hash-true." So
**iroh-blobs' own store is the staging area** (its bitfield, its
partial→complete lifecycle, its multi-provider resume), and a blob is
imported into our CAS only once it completes and verifies, via
`put_with_obao` — reusing the `.obao4` iroh already built (byte-identical,
D97). iroh owns "in flight," our CAS owns "durable and grounded"; the
staging store is a disposable cache (D15), never authoritative. Piece-set
reconciliation composes cleanly: differing pieces are just small blobs
fetched into the same staging store, imported, then fed to the local
`assemble` recipe — no special-casing.

### Swarming: opt-in, in tiers

Joining a swarm is **opt-in and layered**, never a default that leaks a
private collection to strangers:

1. **Friends plane** (D8, first): peers are EndpointIds on an ACL; a
   friend subscribes to a **holdings channel** (D34 — signed, monotonic,
   dir2dat inventory) and gains `available-from-peer(X)` completeness.
   Direct addressing / n0 discovery; no public advertisement.
2. **Public content discovery** (opt-in): iroh's pkarr/Mainline-DHT
   discovery can announce which blake3 roots we serve, so strangers
   knowing only a hash can find us — this is "join the public iroh blobs
   swarm." Gated behind (a) an explicit per-instance opt-in, and (b) the
   **sensitive-blob advertisement policy** flagged since D12/D26: keys and
   anything marked sensitive are never advertised even when the swarm is
   on. Trust for stranger transfers still rides D8's plan: bytes are
   self-verifying (blake3), but *mapping* a dat hash to a blake3 without a
   trusted signer is the **waddup ZKP** slot (D8 tier 2), still M7+.

Discovery announces *availability*; it never implies willingness to serve
sensitive content or to accept inbound pushes. Push (D34 curated channels
moving a head into a subscriber's storage) stays a later feature.

### Spike status

**Proven:** two instances exchange a verified blob (real n0 discovery +
relay path, not just loopback); our obao == iroh's obao byte-for-byte;
**the CAS-fronting handler, both halves** — `cas::CasProvider` serves
iroh's get protocol through `Executor::serve_range` (no store trait, no
byte copy): a resident literal reads from the store, and a
grounded-but-evicted blob (recipe + retained `.obao4`, nothing on disk) is
materialized on the fly and D49-verified — the stock iroh-blobs requester
fetches and blake3-verifies both, unable to tell which was resident.
**Integrated (2026-07-17, D97 amendment 3):** `datboi-p2p` is a daemon
subsystem — folded into the host workspace, iroh in the hermetic build,
and `datboi serve --p2p` spawns the seedbox under the derived iroh key
(D99). It was an *excluded spike* only so the churny iroh tree wouldn't
touch the host lockfile before the design settled — never a permanent
standalone like the wasm components (those never link into the daemon;
this always does). **Designed, not built:** hash-seq requests, the opt-in swarm tiers, and
a `datboi share` / `fetch` operator surface + web home (D96). Streaming
landed 2026-07-17 (bounded-memory, D97 amendment 4); reconciliation ruled
AND BUILT 2026-07-17 as **D100** (previous section): the riblt codec
(differential-tested against the reference), the recon ALPN beside the
seedbox, meta-namespace serving (plans fetch like any bytes, plus the
lazy-outboard backstop for recovery-restored literals), and
`datboi_p2p::sync` — reconcile → fetch-diff → rebuild, proven e2e on a
variant pair (one plan + 2 of 8 pieces cross the wire; mirror mode
grounds without materializing).
