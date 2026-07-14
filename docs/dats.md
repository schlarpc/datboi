# DAT model & update pipeline

*From research pass R1. Formats verified against the Logiqx DTD, clrmamepro
xmlheaders.txt, MAME docs.*

## Canonical internal model

Dats are **claims about content**; datboi is content-addressed. So identity
(hashes) is split from naming (dat entries):

- **DatSource** — provider + system ("No-Intro / Nintendo - Game Boy").
  Stable across revisions.
- **DatRevision** — one imported dat file (itself a CAS object): source
  ref, version, date, format, raw header, emitter hints (forcemerging,
  forcepacking, skipper reference). Immutable; "current" is a pointer.
- **Entry** — game/machine/software within a revision: name, description,
  structured fields, flags (isbios/isdevice), parent refs
  (cloneof/romof/sampleof + No-Intro id/cloneofid), releases[], extensible
  attrs map (sourcefile, board, rebuildto, softwarelist part/dataarea,
  device_ref…).
- **RomClaim** — per Entry: name-in-set, size, **partial hash tuple**
  {crc32?, md5?, sha1?, sha256?}, status (good|baddump|nodump|verified),
  merge name, kind (rom|disk|sample), mia flag, attrs
  (offset/region/loadflag/bios).
- **ContentIdentity** — deduplicated (size + hash tuple) node that
  RomClaims from *any* dat/revision point at; the join point to CAS blobs.
  Unification: claims unify when no hash conflicts and a strong hash
  matches; size+crc-only unification is "probable", never authoritative.
- **Detector** — parsed header-skipper rules, referenced by dat headers.
- Name-derived metadata (region/language/version parsed from No-Intro/TOSEC
  naming conventions) and retool clonelists are **separate re-runnable
  annotation layers** keyed on Entry — never baked into canonical records.

## Header skippers = recipes

The clrmamepro detector XML is a tiny closed DSL: rules (start/end offset,
operation ∈ none|bitswap|byteswap|wordswap|wordbyteswap) gated by tests
(data/and/or/xor/file-size). First matching rule wins. A detector is
literally a declarative input transform producing the headerless variant —
ingest emits claims for both hashes in one pass. byteswap/wordswap ops also
cover N64 orderings.

## Update sources (condensed)

| Provider | Automatable | Notes |
|---|---|---|
| Redump | **yes** | stable `redump.org/datfile/<system>/` URLs; some packs need login; no history |
| MAME / pleasuredome | **yes** | GitHub-hosted per-release dats; listxml→dat is a *derivation* (merge modes) |
| MAME software lists | **yes** | git `hash/*.xml`, fully versioned |
| libretro-database | **yes** | git; better as enrichment than authority |
| TOSEC | semi | ~yearly release zips |
| No-Intro (DAT-o-MATIC) | **contested** | CAPTCHA, scraper bans. Manual daily-pack drop is a first-class flow; polite fetcher + third-party mirrors are opt-in postures |
| retool clonelists | **yes** | git, CC-licensed JSON annotation layer |

Every fetched artifact enters CAS first; import is a deterministic function
of the CAS blob (replayable, auditable, p2p-shareable). Fetchers are
per-provider plugins with politeness (rate limits, conditional GET).

## Gotchas that shape the schema

1. **Revisions rename games constantly** — entry identity ≠ name. Use
   No-Intro ids where present, else rom-content overlap. Revision diff
   (added/removed/renamed/rehashed) is a first-class operation and the
   "what changed today" UX.
2. **Hash coverage is uneven** (MAME: crc+sha1; older dats: crc-only;
   sha256 rare). Hash tuples are partial; our ingest computes the full
   tuple always.
3. **CHD `disk` entries hash the internal data, not the file**; container
   versions change bytes without changing content. Distinguish "satisfies
   the dat claim" from "byte-identical file".
4. **Zero-byte roms are legal**; identical hashes under multiple names in
   one game are common — claims→identity is many-to-one, name-carrying.
5. **nodump/baddump/mia/optional** must be respected in completeness math
   (nodump can never be satisfied; honor forcenodump).
6. **Merge modes (split/merged/non-merged) are output transforms**, not
   storage. Store flat; render any layout. MAME needs device_ref closure +
   bios handling to render correctly.
7. Headered systems have dual identities (headered/headerless hash), and
   dats disagree on which they list — ingest claims both, tagged by
   detector+rule.
8. Real dats exceed the DTD (No-Intro id/cloneofid, sha256, mia, serial) —
   parse unknown attributes preservingly; attrs maps are the losslessness
   escape hatch.
