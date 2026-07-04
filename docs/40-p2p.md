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
