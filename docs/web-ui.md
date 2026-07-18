# datboi — web UI principles

*Status: ratified 2026-07-12 (usability review session; D78–D82 came
out of the same pass). Governs every web surface; the comps and this
doc disagree → this doc wins until the comps catch up.*

## Why this doc exists

Every subsystem with a doc has a philosophy; the web UI didn't, and
it drifted CAS-ward — screens narrated by the engineer who built the
storage engine, not by the product. The persona is **someone who
wants the best rom manager ever**, not someone who wants to admire a
content-addressed store. The emotional jobs are trust (my collection
is provably intact), pride (completeness, space savings), and
effortlessness. The CAS is the engine; these principles are the body
panels.

## The four principles

1. **The subject is the game, not the blob.** Internals appear only
   where the user goes looking for them (the storage section), and
   even there they answer a collector's questions ("what's eating my
   disk?", "is this taking real space?"), not a CAS author's.
   Meaning is computed from the edges — claims, recipe DAG,
   provenance — and leads; hashes are metadata, never headlines
   (D79).

2. **Rename, don't explain.** Every `(?)` bubble, meaning-line, and
   explainer string is an apology for a representation that failed.
   If a label needs a footnote, fix the label. Corollary: no
   dev-facing scaffolding in the render tree — roadmap notes
   ("metadata provider, later") and disabled future-feature buttons
   are not UI; ship the feature when it exists.

3. **Management by exception.** Quiet when healthy. Maintenance
   status (quarantine, orphans, scrub), job activity, and error
   surfaces earn screen space in proportion to the attention they
   need right now — one quiet line when empty, a real surface when
   something wants a human (D82).

4. **Structure over glyph, edges over bytes.** Visual state marks
   are CSS-drawn (deterministic metrics on every OS), never unicode
   glyphs riding font-fallback roulette. Color is never the only
   legend — every state mark travels with its word. And a blob's
   displayed identity derives from the graph around it, per D79.

## Vocabulary

One name per concept, product language, used everywhere:

| engine term        | UI term        | note                                    |
| ------------------ | -------------- | --------------------------------------- |
| resident           | on disk        |                                         |
| evicted_covered    | rebuildable    | this is a *brag* — space saved, provably reproducible |
| absent             | not here       | known hash, no bytes                    |
| literal-only blob  | not yet optimized | "shrinkable" is engine-speak         |
| unattributed blob  | unattached     | narrowed by D79 to connected-to-nothing-claimed |

**Hashes:** short form is the first 8 hex chars, no ellipsis, always
— the git-short-SHA mental model (`shortHash` in format.ts is the
single implementation). Full hash appears exactly once per screen at
most, wears a copy affordance, and copy must work on plain-HTTP LAN
(the primary deployment). Hashes rendered in tables are links to the
blob page; a link to the page you're already on renders as a
non-link "this blob".

**Entry states:** verified / claimed / missing / no dump keep their
words; the marks are a single CSS-drawn component (filled, half,
outline, dash), colored from tokens, word alongside.

## The nav

Ruled during the M5 build (2026-07-11, recorded here): the nav is
**Library · Views · Ingest · Storage · Admin**. Audit is the
drill-down under Library, not a tab — the hi-fi comps' "Dats" tab
variant was rejected as redundant with it. The friend-facing surface
ships as part of this web product (it is what invites + ACLs exist
for); the M6 "Friends" plane is iroh daemon-to-daemon, a different
thing. A naming pass over the Library/Browse/Shelves/Views overlap is
owed when the next screen gets added (flagged in open-questions.md),
not before.

## One canonical home per concept

A concept is *owned* by one screen; everywhere else summarizes and
links. Duplicated micro-renderings of the same internals (the old
drawer "storage internals" fold) are how drift starts.

- **Library shelf** owns per-system completeness.
- **Audit list** (the drill-down under Library) owns entry states
  and the collection browse; its drawer summarizes one entry and
  links each rom's blob to the blob page.
- **Blob page** owns storage internals: identity, digests, routes,
  claims, provenance, pins.
- **Storage** owns totals, breakdowns, and maintenance status.
- **Activity page** owns job history (the D74 ledger); the header
  indicator only says "something is running".
- Ingest feedback is the exception that proves the rule: progress
  renders inline where the user acted, because it's feedback, not
  history.

## Standing constraints

- Desktop-first (comps are 1160px) but layouts must degrade without
  degenerate states: grids use `minmax(0, 1fr)`, names truncate with
  ellipsis + title or stack — `overflow-wrap: anywhere` is for
  hashes only.
- Zero preference toggles (D78). Preferences return one at a time
  when a real need forces them.
- Long lists virtualize against the known total — the scrollbar
  tells the truth about collection size; "load more" buttons don't
  belong in a browse surface. Scrollbars in themed containers are
  themed via tokens.
- Aggregates before enumerations: a 74-chunk recipe is "assemble of
  74 chunks · 18.4 MB" first, rows on demand.
