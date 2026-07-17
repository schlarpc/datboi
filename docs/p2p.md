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

*Open (see open-questions):* which reconciliation algorithm; whether to
reconcile against a peer's advertised piece inventory (privacy: reveals
holdings) or against a specific want-target's piece manifest (leaks less);
how piece manifests ride the D34 channel.

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
**Designed, not built:** streaming instead of whole-blob buffering (the
fsm/async encoder over `open_stream` + spill, for 4 GB ROMs), hash-seq
requests, piece-set reconciliation, the opt-in swarm tiers.
**Deferred to integration:** folding `datboi-p2p` into the host workspace
+ nix vendoring (it is an excluded leaf today so the heavy iroh tree never
churns the host lockfile or `nix build .#datboi`); wiring the iroh
`SecretKey` to the on-disk identity; a `datboi share` / `datboi fetch`
operator surface (the serve+web home per D96).
