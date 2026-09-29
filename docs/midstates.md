# datboi — fetching by dat hash: midstate chains

*Status: DESIGN PASS OPEN, no D-entry yet (2026-09-28). Nothing here is
ratified until it lands as a D-entry. The rulings owed are listed at the
end. Source: waddup's `DESIGN-MIDSTATE.md` (sp1-benchmark branch,
f3a2f72), which drops waddup's ZK binding proof in favor of verifying
Merkle–Damgård chaining values in reverse. This doc adapts that
mechanism to datboi. It replaces the "waddup ZKP slot" that D8 tier 2
and D22 reserved, which means the D-entry has to amend both.
[p2p.md](p2p.md) owns transport and trust tiers, [cas.md](cas.md) owns
aliases and outboards, and [dats.md](dats.md) owns what a dat entry
claims.*

## The gap this fills

Today every p2p fetch is by **blake3**. When a leecher knows only a dat
entry (a sha1/md5/crc tuple plus a size), it needs someone to translate
entry→blake3 first. That is journey 3 in the p2p use-case audit ("dat
gap-fill"), and nothing serves it yet. D8's answer was to trust a
friend's signed `(dat_hash → blake3)` table. For strangers, D8's answer
was a ZK proof binding sha256(F) to blake3(F). Waddup measured that
proof: the zkVMs run at ~1 KiB/s, and purpose-built provers at ~2 MiB/s
after a lot of work. Waddup's own conclusion is that its real goal is
"don't waste time on wrong bytes", not "a transferable certificate", and
for that goal no prover is needed.

**Midstate chains** give that guarantee for any Merkle–Damgård hash
(md5, sha1, sha256, sha512) at native hashing speed and ~0.01% metadata.
They do not work for SHA-3 or crc32, and no dat we ingest keys on SHA-3.

## Terminology (and whether any of it is established)

- **Midstate** is a real term of art. It is Bitcoin-mining vocabulary
  for the SHA-256 state after the first 64-byte block of a block header.
  It also names the precomputed inner/outer state in HMAC
  implementations. The academic term is **chaining value**, and FIPS
  180-4 calls it the **intermediate hash value** H⁽ⁱ⁾. We use
  "midstate" in code because it is short and unambiguous. Docs should
  say "chaining value" at least once.
- **"Midstate chaining"** is **not** an established name for the
  protocol. The technique has been independently described several
  times without a settled name:
  - Adin Schmahmann's 2022 IPFS proposal for incrementally verifying
    large SHA-2 blocks ("compute from the end", discussed under
    *Supporting Large IPLD Blocks* on discuss.ipfs.tech).
  - Chris Warburton's 2026 *SHA on the fly* post and
    `backwards-sha` demo ("downloading in reverse").
  - Waddup's doc.

  None of these is a standard. The closest academic neighbor is
  Gennaro–Rohatgi stream signing (1997), which chains hashes forward
  through the data and is a different construction.
- The security notion waddup calls "free-start preimage" is more
  usually called a **pseudo-preimage** of the compression function: the
  attacker chooses the input chaining value too. Both names appear in
  the literature. Pseudo-preimage is the one to cite.

This doc says **midstate chain** for the verified transfer and
**checkpoints** for the chaining values datboi keeps at rest. It avoids
"outboard" on purpose. In datboi, *outboard* means the obao4 file (D52),
a store-layer artifact that is byte-identical to iroh's. Checkpoints are
a different kind of thing, and they live somewhere else (below).

## The mechanism, in datboi's terms

Notation:

- `f*(m, bytes)` iterates the compression function from state `m` over
  whole blocks (64 B for md5/sha1/sha256).
- `H` is the dat digest and `L` is the dat size.
- `X` is the blake3 a peer claims for the content.

**Anchor.** `H = f*(m_b, bytes[b..L] ‖ pad(L))` for any block-aligned
boundary `b`. A peer that supplies `m_b` and the tail lets the leecher
check the tail against `H` directly.

**Backward links.** Once `m_{b₂}` is verified, a window `[b₁, b₂)` plus a
claimed `m_{b₁}` is verified when `f*(m_{b₁}, bytes[b₁..b₂]) == m_{b₂}`.
Then `m_{b₁}` is verified too. The last link runs from `b₁ = 0`, where
the chaining value is the fixed IV, so no claim is needed.

**Forward links prove nothing.** Anyone can hash garbage forward from
the IV. So the verified frontier grows from the end of the file only,
and a peer's midstates are untrusted until a link reaches them.

### Correctness does not rest on the chain

This is the point where datboi differs most from waddup. In waddup the
chain *is* the integrity argument. In datboi, integrity already comes
from three places:

1. Every byte on the wire is Bao-verified against the candidate `X`
   (stock iroh-blobs).
2. A completed blob imports through `put_with_obao` (D98).
3. The import pass recomputes the **full alias tuple**
   (`AliasHasher`, D2/D22). The dat entry's whole tuple
   (size + crc + md5 + sha1 + sha256 where present) is checked against
   those recomputed values, not against anything a peer said.

A wrong file therefore never becomes a verified alias, whatever happens
in the chain. **The chain is admission control. It bounds how many bytes
a lying peer can make us stage.** If sha1 pseudo-preimages ever became
practical, we would lose efficiency, never correctness. This is D4's
shape again: recipes cannot lie about data, only waste resources. A
midstate claim is one more thing that can only waste resources.

Pseudo-preimage resistance is what keeps the waste bounded. The best
published attacks: SHA-256 reduced-step results only, nowhere near 64
steps at practical cost; SHA-1 nothing practical; MD5 pseudo-preimage
~2^116.9 (Sasaki–Aoki 2009). Waddup's doc quotes ~2^116 as MD5's
*full-preimage* cost. That is a slip: the full preimage is ~2^123.4, and
the pseudo-preimage figure is the one that applies here. Collision
attacks (SHAttered, Shambles) are out of scope as in waddup. They need an
attacker who authored both files, and a dat names a dump the attacker
did not author. The final full-tuple check also catches a sha1-colliding
twin, because its md5 and crc differ.

### The probe is weak, and that is fine

Waddup treats "fetch the last group + `m_{N-1}`, check against `H`" as
the Sybil filter. It only filters *lazy* spam. An honest seeder serves
exactly these values to anyone who asks. So an attacker who asks one
honest peer once can replay a passing probe forever, then serve garbage
below it. The protection is the **window**: a peer can make us hold at
most its window of unverified bytes before a link fails and it is
dropped. The probe stays in the design because it is cheap and kills
the cheap attacks. The window is what bounds waste.

## Checkpoints: structure, storage, access

### What waddup stores, and why datboi doesn't

Waddup's seeder keeps a *dense* sidecar: the chaining value at every
16 KiB Bao group (20 B/group for sha1, ~0.12% of the file). Datboi stores
**sparse checkpoints** and **serves dense midstates on demand**. A
seeder answering "give me `m_b`" starts from the nearest checkpoint
`c ≤ b`, reads `[c, b)` through the verified read path, and runs
`f*`. The per-request cost is bounded by the checkpoint stride. Wire
granularity is the leecher's choice and is decoupled from storage
density.

### Stride and shape

- `stride(L) = max(1 MiB, next_pow2(ceil(L / 1024)))`. It is a pure
  function of `L`, derived and never stored (the D52/D105
  "layout is derived" habit).
- There are at most 1023 checkpoints per (blob, algo). A 4.7 GiB DVD
  gets an 8 MiB stride and 587 entries. A 50 GB BD image gets 64 MiB.
  The worst-case seeder work per request is one stride of hashing
  (~120 ms plus I/O at the top end, ~2 ms for most ROMs).
- **Blobs of 1 MiB or less have no row.** Absence *is* the empty table,
  mirroring the obao "≤ 16 KiB contributes nothing" rule. A seeder hashes
  from the IV, which costs at most 1 MiB.
- Entry `k` (for `k = 1 … ⌊(L−1)/stride⌋`) is the chaining value after
  exactly `k·stride` bytes. `m_0` is the IV, and the final state is the
  digest, so neither is stored.
- A value is serialized as the hash's own output encoding of the state
  words (big-endian for SHA, little-endian for MD5), which is the bytes
  the digest would print if finalized unpadded there. This is pinned by
  golden vectors, because the same encoding is the wire encoding.
- Algorithms: **md5, sha1, sha256**, which is exactly `AliasTuple` minus
  crc32 (linear, unusable) and blake3 (it has Bao). The cost is
  68 B per stride, ~0.0065% of the file, ~65 MB per TB of large blobs.
  All three come out of the ingest pass for free, so there is no reason
  to guess which one a future dat will key on.

### Where they live: the index, beside the aliases

```sql
CREATE TABLE midstate (
  blob_id INTEGER NOT NULL REFERENCES blob(blob_id),
  algo    INTEGER NOT NULL,          -- alias.algo codes
  states  BLOB NOT NULL,             -- headerless concat, k = 1..
  PRIMARY KEY (blob_id, algo)
) STRICT, WITHOUT ROWID;
```

Why the DB and not the store:

- **Checkpoints are alias evidence.** An alias row says
  `sha1(X) = H`. A checkpoint row says *how* the sha1 got there. Both
  are pure functions of the bytes (D22: derived cache locally), and they
  should sit in the same place.
- **Size.** obao4 lives in the store because it is 0.39% of the data,
  the byte layer needs it for D49 verified reads, and iroh must serve it
  byte-for-byte. Checkpoints are ~60× smaller, only the midstate
  protocol reads them, and they never cross the wire as a unit.
- **Inodes.** A loose sidecar per (blob, algo) repeats the per-blob
  sidecar pattern D18 rejected. A pack section (D105-style) would split
  checkpoints across two homes for no gain.
- **Not in the state snapshot.** D15 snapshots aliases because dat
  matching needs them the moment recovery finishes. Nothing needs
  checkpoints for correctness. After a bare-NAS recovery they are
  **backfilled** by an ambient maintenance sweep (D71–D74) that rides
  scrub's reads. Until a blob is backfilled, a friend's request for it
  is answered by hashing from the IV and persisting the result on the
  way (lazy backfill, concurrency-capped).

### Who computes them

- **`AliasHasher` becomes the checkpoint producer.** It already streams
  every byte through md5/sha1/sha256 at every ingest site:
  `Store::put` (store.rs), pack member verification (pack.rs), and
  scrub (store.rs `verify`). RustCrypto's streaming `Digest` hides its
  state, so the hasher drives the raw compression functions itself.
  `sha1::compress` and `sha2::compress256` are public in the 0.10 line.
  **`md-5` 0.10 keeps `compress` private**, so md5 needs a vendored
  compression function (~100 lines), differential-tested against the
  one-shot digest exactly like the riblt port (D100).
- **Recipe outputs count.** A headerless variant (D9 skippers), a
  decomposed track, or a CHD's raw data are blobs with aliases. They get
  checkpoints wherever their alias tuple is computed, which is the
  analysis/materialization pass.
- **Residency doesn't touch them.** Like the retained `.obao` under D49,
  a row outlives eviction of the literal. A virtual blob is exactly the
  one whose checkpoints are most expensive to recompute. The row dies
  only with the blob identity itself (GC forgetting the blob row).
- **Scrub checks them.** The scrub pass recomputes the tuple anyway.
  A checkpoint mismatch means DB rot. That gets a WARN and a rewrite, the
  D81 self-heal shape.

### How they are accessed: the wire

The design needs a third ALPN beside `iroh-blobs` and `datboi/recon/1`.
Call it **`datboi/midstate/1`**, using the D103 postcard envelope, with
an append-only request enum pinned by goldens:

- `Lookup { entries: [(algo, digest, size)] }` →
  per entry `[(blake3, size)]` candidates. This is multi-hit by design
  (D2: colliding files are distinct blobs both aliased to one sha1).
  It answers only for blobs the serving policy allows (below). Batched,
  because gap-fill asks about hundreds of entries at once.
- `Midstates { blob: blake3, algo, offsets: [u64] }` →
  one chaining value per offset. Offsets must be multiples of 16 KiB
  (so they line up with Bao groups and every algorithm's block size), be
  below `L`, and be capped per request. The responder sorts them and
  computes each from its nearest checkpoint. A refusal is header-time,
  as in D103.

**Bytes do not ride this ALPN.** The window's data is an ordinary
Bao-verified range get of `X` over iroh-blobs. That keeps D97/D98
unchanged: iroh stages the partial blob, and our CAS ingests the
completed one. The midstate ALPN carries only claims and evidence, the
same split recon already follows.

## The fetch, end to end

For a leecher wanting dat entry `E = (L, crc, md5, sha1, sha256?)`:

1. **Pick the chain hash.** Use the strongest Merkle–Damgård digest the
   entry carries: sha256 > sha1 > md5. An entry with crc only is not
   chain-fetchable (see *Limits*).
2. **Find candidates.** Send `Lookup` to friends, or use the bulk path:
   a parameterized recon scope over dat-attested digests (below).
   Discard any candidate whose size ≠ `L` before touching bytes. The
   padding binds `L` into `H` anyway, so a wrong size can never pass.
3. **Small files skip the chain.** If `L ≤` the peer's current window,
   fetch `X` whole and let the import check decide. Waste is bounded by
   `L` by definition. This covers the majority of ROMs by count.
4. **Probe.** Get `m_b` for `b` = the last 16 KiB boundary below `L`,
   fetch the tail range of `X`, and check it against `H`. On failure,
   drop the peer.
5. **Walk backward** in windows. Request `m_{b₁}` for the window's
   lower edge and the range `[b₁, b₂)` of `X`, then check the link.
   Several windows may be *in flight* from several peers serving the
   same `X` (iroh's multi-provider get), but the frontier only
   *commits* contiguously from the end.
6. **Complete.** The last window's link runs from the IV. `X` completes
   in iroh staging, then `put_with_obao`, then `AliasHasher` recomputes
   the tuple and writes checkpoints in the same pass, so a finished
   leecher is immediately a seeder. The whole tuple is checked against
   `E`. Aliases minted this way are ordinary locally-computed rows, and
   nothing a peer said survives the import.

Differences from waddup's flow, all deliberate:

- **No cross-candidate salvage in v1.** Waddup keeps the verified
  suffix of a fake swarm and reuses it for the next one. In datboi,
  staging is per-`X` (iroh's store), so a dropped candidate's partial is
  discarded. Salvage would mean copying verified ranges between iroh
  partials. That is deferred until someone sees it matter, because a
  fake candidate's honest suffix was real data the attacker had to hold.
- **Blame is per peer, never per candidate.** A failed link means either
  the bytes are wrong (fake `X`) or this peer's `m_{b₁}` is wrong. We
  drop the peer that supplied both. We never condemn `X` globally,
  because a Sybil swarm lying about midstates for the *genuine* `X`
  could then DoS the real file. A fake `X` dies on its own when all its
  peers are dropped.
- **The IV check is the last link, not a separate step.** Waddup's step
  4 ("check `m_0 == IV`") is vacuous as written: the leecher never
  receives `m_0`. It is the final link, with the IV as the lower state.

## Policy

Waddup leaves policy open apart from one window sketch. Datboi can't,
because both answering a lookup and asking one are disclosures.

### Posture: friends plane first, public tier needs its own ruling

- **v1 is the friends plane only.** This covers the ACL'd EndpointIds
  from D8 tier 1 and the capability-addressed `--p2p` posture of today.
  Chains give the friends plane two things it lacks:
  - gap-fill by dat entry (journey 3);
  - removing trust in a friend's mapping (a friend's alias bug becomes
    a dropped peer, not a staged wrong file).

  **The chain gate applies to friends too.** It uses one mechanism
  everywhere, and trust only sets window sizes: "reputation is an
  efficiency knob, never a safety one", which is waddup's line and D4's
  posture.
- **The public tier collides with [vision.md](vision.md)**:
  "Not a downloader/piracy tool: it manages and verifies what you have."
  Announcing `(algo, dat hash)` on a public DHT and serving strangers is
  squarely a downloader. The ZKP slot deferred that question by being
  expensive, and midstates remove that excuse. Whether datboi ever
  answers strangers is therefore a **vision ruling, not an engineering
  one**, and it gates on the recon ACL already owed (open-questions).
  This doc designs the mechanism so it *could* serve that tier. It does
  not propose turning the tier on.

### Serving (what a seeder answers)

- **Only dat-attested content.** `Lookup` answers a digest only if its
  blob is the target of a RomClaim in an imported dat. A peer's lookup
  should never work as an oracle for arbitrary private bytes. This
  excludes:
  - saves (saves.md);
  - keys and anything under the sensitive-blob policy (D12/D26);
  - the meta namespace;
  - loose files no dat names.
- **Not found and refused look the same.** An unauthorized or excluded
  lookup gets the same empty answer as a miss, so holdings don't leak
  through error codes. The D103 `Refused` code is for protocol-level
  refusals (unknown request, over caps), never per-entry.
- **Cost caps.** Each `Midstates` request costs one stride of hashing per
  offset, plus a range read. Cap offsets per request and concurrent
  requests per peer. The costly case is a **virtual blob behind a
  non-affine route** (preflate, decompressing recipes), where a range
  read may materialize from the start of a stream. Friends get it
  anyway, because it is their library too. A future stranger tier would
  serve only blobs with cheap ranged access (resident, packed, or
  affine).
- **Lazy backfill is friends-only.** A missing checkpoint row means an
  O(L) hash. That is acceptable for a friend. For a stranger it would be
  an amplification vector.

### Fetching (what a leecher tolerates)

- **Windows** (starting points, tuned by measurement):
  - unknown peer: 1 MiB, doubling per verified window, capped at 64 MiB;
  - friend: 64 MiB from the start, capped at 256 MiB.

  A failed link drops the peer for this transfer and records a strike
  against its EndpointId. **Strikes persist, but only as a
  deprioritization, never a ban**: EndpointIds are free to mint, so
  bans are theater. The persisted strike only means the peer starts at
  the smallest window next time.
- **Global budget.** Unverified staged bytes across all transfers are
  capped (default 1 GiB) so gap-fill over a big dat can't balloon iroh's
  staging. This is the waste invariant waddup proposes to prove:
  *unverified(peer) ≤ window(peer)* and *Σ ≤ budget*.
- **Candidate order.** Prefer candidates served by more friends, then by
  more peers. Peer count is Sybil-inflatable, but the window bounds what
  a wrong choice costs. Fetch one candidate at a time per entry, and
  keep the others as fallbacks.
- **Secondary-hash mismatch at import.** Sometimes the chain completes
  but the full tuple disagrees: the chain hash matches and md5/crc
  don't. That is a hash collision or a dat error, not a peer fault we
  can prove. **Discard the bytes, keep nothing, WARN loudly** with both
  tuples. Keeping them would store content no dat wants.

### Bulk discovery: a recon scope, not a lookup storm

A 10k-entry gap-fill across five friends is 50k lookups. The D103 scope
registry anticipated parameterized scopes ("blobs in dat X"). The
proposal is a scope **`DatHeld { dat_source }`**: the set of chain
digests (sha1, 20-byte symbols, per-scope width) of entries in that
dat's current revision that the party holds.

- By the symmetric-prior convention, the initiator's prior is its own
  held set for the same dat. The decoded difference is what the friend
  has and the initiator lacks, plus the reverse, which the initiator
  filters out.
- Then `Lookup` goes only to the decoded digests.
- The disclosure is the same as the existing recon scopes: the
  responder is the consenting party, and it reveals one dat's holdings.
- This is where [p2p.md](p2p.md)'s `available-from-peer(X)`
  completeness state gets its data. It does not need the D34 holdings
  channel for this journey. The channel remains the naming layer for
  curated subscription (journey 4).

### Observability (D81, D97)

- **INFO**: one line per fetch job, with entries attempted and
  completed, bytes staged, bytes discarded, peers dropped, and waste
  ratio.
- **WARN**: a dropped peer, a secondary-hash mismatch, a checkpoint
  self-heal in scrub.
- **DEBUG**: per-window verdicts.

Waste is first-class output the way D97 made savings first-class.
"Bytes discarded" is the number that tells us whether the windows are
right.

## Limits (what chains cannot reach)

- **CHD `disk` entries.** The dat sha1 is the CHD's combined digest,
  `sha1(raw_sha1 ‖ metadata hashes)` (dats.md gotcha 3). That is a hash
  of a 20-byte digest plus metadata, not of any byte stream anyone
  transfers, so no chain anchors to it. The chain could run over the
  *raw decompressed data* (anchored by `raw_sha1`, itself checked
  against the combined digest from the header's metadata), with the CHD
  rebuilt locally. Any valid CHD satisfies the claim (D44). The cost is
  shipping uncompressed data, which is often 2–3× the CHD. v1 gives
  CHDs the whole-file window, which means friends only. The raw-data
  route is a follow-up once someone wants it.
- **crc32-only entries** (older dats). crc is linear, so no chain is
  possible. Friends-plane whole-file fetch only.
- **SHA-3** has no chain, but no source dat uses it.
- **Big blobs cost full bandwidth.** A 4.7 GiB disc image chain-fetches
  as 4.7 GiB of raw bytes, even when the friend holds it as a
  decomposition and we hold its sibling variant. Recon (D100) saves
  bytes only once we *trust* `X`. See the next section.

## Later: chains over recipe materialization

Combining the two p2p planes is where datboi could beat a plain
downloader. After the probe, fetch `X`'s **affine recipe** (D100 plans
fetch like any bytes) instead of `X`'s bytes. Then materialize `X`
backward window by window, fetching only the pieces each window covers
and the ones we don't hold (the MKDS USA→EUR case: ~8 of 564 pieces).
Each window's output is chain-checked before the next window's pieces
are fetched. The waste bound still holds: at most one window's worth of
pieces. Affine routes map output ranges to input ranges locally, which
is what makes this possible. Compressed routes don't, so they fall back
to the plain byte fetch. This is not v1, but v1's shape shouldn't
preclude it: keep windows defined over output offsets of `X`, not over
transport ranges.

## Implementation sketch

- **`datboi-midstate` crate, sans-IO.** It holds:
  - the compression drivers (md5 vendored);
  - `f*`, stride, and serialization;
  - the leecher **frontier state machine**: events in (claim, window
    bytes + midstate from peer P, timeout), actions out (request,
    commit, drop peer, complete).

  The iroh shell in `datboi-p2p` only moves bytes. This is waddup's
  verification plan, scaled to what datboi already does for riblt:
  - differential tests of every `f*` against the one-shot digests;
  - golden vectors for serialization;
  - proptest plus an adversarial simulator (fake `X`, lying midstates,
    replayed probes, stalls) asserting the waste invariants.

  Kani harnesses over the frontier are cheap to add on top. Verus/Lean
  are not proposed, because correctness doesn't rest on this crate.
- **`AliasHasher` → checkpoints**, the `midstate` table, the scrub hook,
  and the backfill sweep (a D74 ledger kind).
- **`datboi/midstate/1` ALPN**, the `DatHeld` recon scope, and a
  "complete from friends" job whose web home is the dat completeness
  screen, not the Ingest card (web-ui.md: one canonical home per
  concept, and gap-fill is about a dat).

## Open rulings owed (future D-numbers)

1. **Adopt midstate chains as the entry→blake3 trust mechanism.** This
   amends D8 tier 2 and D22's last clause ("the waddup ZKP slot"), and
   the p2p.md swarm tier-2 text and roadmap M7+ line that echo them.
   The ZKP stays rejected-for-now, not dead: a proof is still the only
   route to a *transferable* certificate and to SHA-3.
2. **Checkpoints live in the index as derived cache.** Stride formula,
   three algorithms, not snapshotted, backfilled after recovery.
   *Rejected:* dense waddup sidecars, store-layer sidecars, pack
   sections.
3. **The chain is admission control, not integrity.** The import
   recompute stays the only integrity gate. This is the one ruling that
   keeps the pseudo-preimage assumption out of datboi's correctness
   story.
4. **Serving policy.** Friends only, dat-attested only, not-found and
   refused look the same, cost caps.
5. **Fetching policy.** Windows, budget, per-peer blame, strikes as
   deprioritization, discard on secondary mismatch.
6. **`DatHeld` recon scope** as the bulk discovery path (a D103 scope
   registry addition).
7. **The public tier.** This is a vision.md ruling ("not a downloader")
   before any engineering one. Recommendation: rule it explicitly out of
   scope until the recon ACL exists, rather than leave it implied as
   "M7+".
