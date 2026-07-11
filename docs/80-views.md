# Filesystem views

*From design pass R7, amended per discussion 2026-07-03. Governing:
D23 (views are policies), D27 (seekability-aware residency), D32
(userspace-only serving).*

## Three layers

- **View definition** (policy, mutable, named):
  `query × selection × per-item transform chain × layout`. May reference
  "current dat revision" — deliberately not deterministic over time.
  Authoritative small state (SQLite + D15 snapshot).
- **ViewSnapshot** (fact, immutable, content-addressed
  `datboi/viewsnap/1`): result of evaluating a view at a moment — a
  canonical manifest of `(path, output_hash, size, attrs)` rows, each
  backed by a blob or verified recipe, recording which dat revisions were
  used (reproducible even though the view says "current"). Diffable,
  pinnable (GC roots), p2p-shareable ("my curated GBA set" = a ticket).
- **Serving surface**: protocol adapter presenting snapshots only —
  consoles never see a tree mutate mid-read; updates are atomic snapshot
  flips.

Boundary rule: anything that names "current" lives in policy; anything
hashable lives in CAS.

## Seekability taxonomy (drives everything)

Every transform declares its class in WIT metadata:

1. **Offset-affine** — range reads translate arithmetically (assemble,
   swap, header ops, trim/pad, sector re-layout, XISO windowing). Serve
   fully lazy; cost ≈ underlying read.
2. **Manifest-seekable** — random access via content-addressed index
   (chunked zstd frame tables, CSO block index). Lazy + block cache.
3. **Opaque** — whole-stream only (solid compression, chained-IV
   encryption, TorrentZip). Eager-materialize into cache tier at snapshot
   activation.

Decided per file at snapshot time, recorded in the snapshot row; surfaces
never guess. Feeds D27: opaque-covered literals referenced by pinned
snapshots are never evicted.

## Serving surfaces (D32: userspace, cross-platform)

| Surface | Status |
|---|---|
| HTTP Range + WebDAV (axum + dav-server) | day one |
| In-process userspace NFSv3 (nfsserve/nfs3_server lineage, VFS trait) | primary mount, phase 2 |
| FUSE (fuser) | optional where available, never required |
| SMB | sidecar Samba (generated smb.conf, NT1 only on isolated retro share) initially; **own read-only memory-safe SMB1 server** for OPL/OG-Xbox is an accepted future workstream (narrow, documented op subset; safer than NT1-in-Samba) |
| FAT32 image synthesis (fatfs) | day-one-ish output transform; virtual-image mode maps FAT data extents affinely onto recipe outputs — stream a full SD image without materializing files |
| SD sync (`view sync <view> /media/sd`) | cheap, day-one-ish; flashcart users sync, not mount |
| iSCSI | rejected for now (no credible Rust server; virtual-image machinery covers block-device needs if ever exported via nbd/ublk) |

Retro reality check: PS2 OPL and OG-Xbox netboot require SMB1/NT1; no
Rust SMB server exists today; console loaders issue random range reads
into ISOs mid-game — hence the affine path mattering so much.

## View definitions

```
view "gba-everdrive" {
  query:     dat("No-Intro/GBA", current) ∩ have(verified)
  selection: 1g1r(regions=[USA,EU,JP], langs=[en], retool_clonelists=true)
  transform: [xf-trim, xf-sram-patch]
  layout:    template "{alpha_bucket}/{name}.gba", profile fat32-everdrive
}
```

Constraint **profiles** (curated, overridable: everdrive, opl-smb,
xbox-fatx ≤42 chars, mister) enforce filename charset/length, FAT32
4 GiB−1 cap (auto-split or fail per policy), max_dir_entries (console UIs
choke long before FAT32's 65k), deterministic collision disambiguation.
Knobs live in profiles, not scattered — anti-RetroArch clause.

Length caps are enforced by a **name-fitting pipeline**, not skipping:
each profile carries an ordered, deterministic list of rewrite rules
applied until the name fits — strip noise prefixes ("2 Games in 1! - "
and kin), compress region tags ((USA)→(U), (Europe)→(E), (Japan)→(J)),
trim trailing junk — then truncate with suffix reserve, and only
skip+count if the name still collides after disambiguation. A ROM
dropped for a 103-char dat name is a real loss; the same ROM as "(U)"
is not. Likewise max_dir_entries is mitigated (alpha-bucket the
template, `#/` for non-alpha), not just reported. (Both lifted from
the 2021 Python prototype's EZ-Flash Omega mutator — the earliest
per-projection constraint code in this project's lineage. Device data
point preserved with it: EZ-Flash Omega = max 512 files/dir, max
99-char filenames — its own profile, distinct from everdrive.)
Shipped status (2026-07-10): the 07-09 profiles skip oversize rows and
only report overfull dirs — the fitting pipeline + auto-bucketing are
an owed M4 work item.

## Materialization

Hybrid, snapshot-driven by seekability class (above). Eager
materialization = residency planner told "these outputs need literals
while snapshot S is pinned" — no new machinery. Readahead on detected
sequential runs.

## Open

- Snapshot refresh policy (auto-flip local surfaces vs manual promotion)
  — under discussion.
