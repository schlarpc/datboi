# Metadata DB schema

*From design pass R8, ratified 2026-07-03 (D37–D39). Full CREATE TABLE
drafts live in the design history; this records the shape and the
load-bearing choices. Governing: D10, D15, D18–D22, D25, D27, D30, D31,
D34.*

## Two files (D37)

- **`cache.db`** — everything derivable from CAS bytes + deterministic
  re-import: blob index, aliases, recipe graph, dat entries/claims,
  identity join, audit rollups, rescan cache, peer_have. Corruption
  remedy: delete + rescan. `synchronous=NORMAL`; migrations may be
  "drop and rebuild".
- **`state.db`** — authoritative-until-snapshotted: tags/pins, users,
  invites, sessions (excluded from snapshot), peer ACLs, view
  definitions, channels, subscriptions, config, snapshot log. Small
  (MBs). `synchronous=FULL`; real migrations forever; the CAS snapshot
  is a second serialization acting as a compatibility net.

Doctrine made mechanical: sole truth may only live in state.db, and
state.db must round-trip through the snapshot encoder. Cross-file
consistency is eventual (recovery already assumes it). Both: WAL, STRICT
tables, FK on, daemon-local disk. Hashes are 32-byte BLOBs; graph tables
use integer surrogate keys (8 B FKs at 40M+ edge rows).

## Store index (cache.db)

`blob` (hash unique, size, namespace data/meta, residency
resident/evicted-covered/absent, verified_at, obao state, last_access,
pinned_reason) · `alias` ((algo, digest, blob_id) PK — multi-hit
tolerant per D2) · `recipe` (op, **seek_class** affine/manifest/opaque
for D27, verify state machine `pending → verified → replayed-local`
(only replayed-local licenses drops, D25) | `failed` = permanent poison
w/ error+peer, source) · `recipe_input` (position, role) /
`recipe_output` (ordinal, claimed size, name) — output index is the D21
OR-graph entry point · `source_file` rescan cache (path, mtime, size →
blob) for O(changed) rescans.

**Grounding (D21)** is an application-driven loop of set-based rounds
(the ∀-inputs-grounded condition is not a monotone recursive CTE): seed
temp table with resident literals; repeatedly insert outputs of
replayed-local recipes whose inputs are all grounded; converges in ≤ DAG
depth. Evictability of X = X still grounded with X's literal removed
from the seed, plus the D27 opaque/pinned-snapshot rule. Run per planner
batch, never whole-store sweeps.

## Dat model (cache.db)

Per 60-dats.md: `dat_source` (provider, system, current pointer) →
`dat_revision` (CAS blob ref, format, header JSONB, detector ref,
**materialized** flag) → `entry` (name, stable_key = No-Intro id,
parent name+resolved refs, flags, **attrs JSONB** for the long tail:
sourcefile, device_ref[], softlist parts/dataareas, unknown attrs) →
`rom_claim` (kind rom/disk/sample, partial hash tuple as written,
status/mia/optional, merge_name, identity ref, attrs JSONB) ·
`release` rows · `detector` (parsed skipper JSONB) · `annotation`
((entry, layer) — re-runnable name-parse/retool passes) ·
`content_identity` (merged partial tuple + strength; **no UNIQUE** —
sha1 collisions legal; unification in code: no conflicts + strong-hash
match, crc+size ⇒ probable) · `identity_blob` (multi-hit, basis
strength).

JSONB-over-EAV rationale: the long tail is preserved-not-queried
(losslessness lives in the CAS dat blob anyway); audit-path fields are
real columns; SQLite JSONB + generated columns = index-later escape
hatch. EAV rejected (join explosion, no types).

**Revision retention (D38)**: full rows only for materialized revisions —
default current + previous per source; older demote to header-only via
plain DELETE, re-importable on demand from CAS (deletion as archival,
courtesy of D15). Revision diff: match on COALESCE(stable_key, name),
second pass on claim-identity-set fingerprints to classify renames.

## Audit (cache.db)

Two-stage recomputed rollups (never triggers): `identity_status` (best
availability per identity) → `entry_audit` (per-entry counters).
Recomputed on events: ingest batch end, recipe state change, revision
import, channel update. **Six states (D39)**: have-verified /
have-claimed / **probable** (crc+size-only basis) / available-from-peer
/ missing / unknown — honoring nodump (per forcenodump), baddump, mia,
optional; non-merged scope per D31 with romof/device_ref captured so
closure queries are additive later. `peer` + `peer_have` cache holdings
channels; peer_have-as-bitmap is the deferred mitigation if mirror-scale
peers multiply.

## Scale (sanity-checked)

~12–18 GB total at full-everything scale (blob 10–15M rows, alias
40–60M, recipe+edges 10–25M, claims ~8–12M with current+previous).
Comfortable on local NVMe. Watch items: alias is pure cache (crc32 rows
droppable if it annoys); grounding temp ~100 MB at 10M residents (batch,
don't sweep).

## Migration posture

`user_version` + `application_id` on both files. cache.db: attempt cheap
in-place, else rebuild from CAS (needs state.db or snapshot first for
tags/source list — matches D15 recovery ordering). state.db: real
migrations, snapshot restore as the worst-case path.
