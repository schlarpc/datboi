# Roadmap

*Ratified 2026-07-03 (D35, D36). Working milestone names.*

## M1 — MVP: "Audit truthfully, store durably"

Two load-bearing scope cuts, both ratified:

1. **Additive-only**: recipes/verification/grounding exist, but nothing is
   ever evicted — no residency planner, no code path that can destroy
   bytes. Storage wins are M3's headline (post-D50 numbering).
2. **Containers stay literal; members are claims**: ingest streams through
   zips hashing members and minting derive recipes (`slice` +
   `deflate-decompress` builtins — replay needs no zip parsing), storing
   no member bytes. ≈1.0× storage; audit works off hashes.

Consequences: zero load-bearing wasm in MVP (all recipes are builtins);
wasmtime ships as infrastructure + one reference transform + the
determinism CI gate (D5/D7 proven, nothing blocked on transform
authoring). CLI-only; daemon on localhost/unix socket; no auth until M5.

**Definition of done** (abridged; full criteria in this doc's history):
ingest a messy directory (zips, raw, headered/headerless NES, bin/cue,
CHDs) against No-Intro GBA + NES (forces skippers), Redump PS2 (forces
multi-GB streaming), MAME listxml + one softlist; streaming single-pass
full-alias-tuple hashing; skipper dual identities both directions;
CHD v5 header internal-sha1 audit (no decompression); `audit` with
have(verified)/missing/unknown honoring nodump/baddump/mia, non-merged
(D31); `dat diff` revision diffing; dir2dat with import→export→semantic-
diff-empty; Redump auto-fetch + No-Intro manual drop (D16); kill -9
anywhere → clean restart, O(changed) rescan; **bare-NAS recovery drill in
CI** (delete SQLite → recover → byte-identical audit); signed state
snapshots; determinism gate (all builtins + reference wasm, N replays × 2
architectures, byte-identical).

**Prototype-first (before any format freeze):**
1. NFS store bench: 1M synthetic blobs (real MAME size histogram), fanout
   ∈ {1,2,3}×256, ingest + cold recovery scan at parallelism {8,32,128},
   rsync/zfs-send wall-clock. Tripwire: recovery extrapolating >12 h at
   10M files forces aggregation-by-default. Freezes shard fanout.
2. Recipe codec + assemble executor + verify tee (the bytes frozen
   forever) + single-pass multi-hash throughput target ≥1 GB/s.
3. wasmtime determinism PoC (NaN canonicalization, no ambient imports,
   x86_64/aarch64 byte-identity; zstd-in-wasm overhead measurement).
4. MAME-at-scale parse (full listxml ~50k machines + softlist) through
   the schema before it's load-bearing.

**Critical path**: core object model (canonical CBOR + typed hashes) →
store-fs → executor → ingest pipeline → audit → CLI. Parallel tracks:
dat parsers + skipper interpreter; SQLite schema + recovery; wasmtime
host; daemon/CLI scaffolding; nix flake (day one).

**Testing**: proptest store invariants; kill-9 crash-injection harness
(tmpfs every commit, real NFS nightly); codec golden vectors + fuzz;
executor replay determinism as CI gate; dat parser golden corpus +
synthetic edges; "minidat" fixture universe for end-to-end;
recovery-equivalence as a property test.

## Milestones

- **M2 — "The engine streams"** — **COMPLETE 2026-07-07**: exit test
  green at full size (3.9 GiB member, bounded memory, sequential +
  seeked, verified); @2 frozen; fixpoint skeleton survives the recovery
  drill with a no-op sweep. (platform; split from the old fat M2 by
  D50): `datboi:transform@2` streaming world designed and **frozen**
  (D46: streams as resources in our own `types` interface,
  empty-linker sandbox preserved); streaming executor integration +
  spill rule; bao outboard machinery (computed on materialization,
  survives eviction — D49); mandatory output-bao verification on
  seekable recipe routes + the seek-quarantine failure class;
  determinism gate extended to @2 with seek-equivalence property tests
  (random range reads ≡ slices of full materialization, ±1 at declared
  boundaries); refinement fixpoint skeleton (D45/D47/D48): background
  sweep queue with dat-aware ordering, analyzer provenance including
  negative results, provenance snapshot batches in the recovery drill.
  **Exit test**: a ~4 GB zip member replays in bounded memory,
  verified, both sequential and seeked; a no-op analyzer sweep records
  provenance and survives bare-NAS recovery.
- **M3 — "The NAS gets smaller"** (features on that platform):
  residency planner + eviction (D21/D25/D27) — *shipped 2026-07-07*;
  FastCDC chunking — *shipped* (analyzer through the fixpoint;
  cross-image dedup + evict + verified serving proven end to end);
  wild-zip rebuild — *shipped 2026-07-07* (D53: preflate splitting,
  `xf-preflate` @2 component, per-member recreate + container assemble
  recipes, e2e split→license→evict→rebuild gate; TorrentZip is zlib
  and fully covered — the zlib-exact-compressor research question is
  DEAD, and 7z-made streams stay literal per the open issue);
  7z/rar input — *shipped 2026-07-07* (extraction-based: members
  become resident alias-indexed blobs, containers stay literal until
  an LZMA-class rebuild transform exists — prior-art research in
  flight); **remaining**: **aggregation (D36, still NFS-bench-gated —
  bench machine unavailable)**, ECM.
- **M4 — "The NAS becomes useful"**: views/snapshots/profiles (D33),
  1G1R + retool clonelists, MAME merge-mode rendering + device_ref
  closure + softlist fidelity (D31 deferred set), HTTP/WebDAV, SD sync,
  in-process NFSv3, FAT32 image synthesis.
- **M5 — "Other people can touch it"**: axum API, invites + passwords
  (D30), ACLs, Svelte web UI (D17).
- **M6 — "Friends"**: iroh, partial-blob bitfields + irpc store facade
  (D14 stage 2), holdings channels + peer-availability audit state (D34),
  tickets.
- **M7+ — frontier**: platform rebuild long tail (CHD/RVZ/NSZ, D12 key
  flows), read-only SMB1 server (D32), curated channels, waddup ZKP
  swarms, browser emulator cores.

Ordering rationale: risk-retirement × usefulness-to-the-single-NAS-user;
p2p is late because it needs store + recipes + views to be worth sharing.
