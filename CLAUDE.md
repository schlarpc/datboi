# CLAUDE.md

Conventions for working in this repo. The docs are the real record —
this file points at them and covers what an agent trips over.

## Commits

- **Commit as you go**: one commit per logically complete, test-green
  unit of work. Never accumulate a multi-task diff — it can't be
  reverted piecewise and it entangles with in-flight human edits.
- Commit directly to `main` (house workflow; no PR ceremony here).
- Message style: lowercase `area: what changed — why it matters`,
  citing D-numbers where a decision is involved. Read
  `git log --oneline -15` and match. Examples:
  - `web: dates render in the viewer's timezone`
  - `all: maintenance goes ambient and remembers — D71–D74`
- If the working tree has human edits when you start, note them and
  keep your commits from absorbing their files where possible.

## The decision log (docs/)

- `docs/decisions.md` is a lightweight ADR log: `## D<n> — title
  (date)`, a tight paragraph of what/why, and a `*Rejected:*` list.
  **It is authoritative** — open-questions.md flags can lag it
  (recorded lesson, see the D54/D55 note there).
- Anything that overturns a prior ruling, changes an architectural
  posture, or a future reader might re-litigate gets a D entry BEFORE
  the code lands. Follow-up tweaks to a fresh decision are
  `*Amendment (same day):*` paragraphs under the entry.
- `docs/open-questions.md` holds deferred items, watch items, and
  per-session position notes ("pick up here").
- The subsystem docs are per-subsystem design records —
  `docs/README.md` is the index and encodes the reading order (D88:
  filenames are stable names, never positional numbers).
  `docs/web-ui.md` governs every web surface (persona: someone who
  wants the best rom manager ever, not a CAS admirer).
  Code comments cite D-numbers and doc sections liberally — keep that up.

## Build & test

- Everything is nix. The devshell (direnv) provides cargo, node, and
  the linked `web/node_modules`.
- **New files must be `git add`ed before any nix build sees them** —
  flake sources are the git-tracked set, and `cargo build` of
  datboi-server runs `nix build .#web` in its build.rs.
- Rust: `cargo test --workspace`. Web (from `web/`): `npx vitest run`
  and `npm run check` (svelte-check + tsc).
- Full hermetic proof when flake/deps/build.rs change:
  `nix build .#datboi`.
- Embedded artifacts (wasm components, web dist, magic.mgc) ride
  `DATBOI_*` env vars: crane sets them; dev builds' build.rs falls
  back to `nix build .#<output>` itself (D66 pattern).

## API contract (D69)

- `/v1` shapes live ONLY in `crates/datboi-api` (the one crate allowed
  serde derives). After changing them:
  1. `cargo run -p datboi-api --bin datboi-gen-openapi` (a staleness
     test pins the checked-in `openapi.json` byte-for-byte),
  2. `cd web && npm run generate` (regenerates `schema.d.ts`).
- A new endpoint registers in BOTH `datboi-api/src/paths.rs` (the
  OpenAPI list) and `datboi-server/src/http.rs` (the router) — parity
  is tested.

## Web UI

- Strings are wuchale-extracted (D67): after adding/changing/removing
  user-visible copy, run `npm run extract` from `web/` — `en.po` is
  generated-but-checked-in. Learn the `@wc-context` / `@wc-include` /
  `@wc-ignore` comment patterns from existing call sites before
  writing new strings.
- Design rules live in `docs/web-ui.md` (vocabulary, hash short
  form, CSS-drawn state marks, one-canonical-home-per-concept,
  management by exception). When a change and the old comps disagree,
  the doc wins.
- Zero preference toggles (D78). Don't add one without a ruling.

## Logging & style

- Daemon crates (`datboi-server`, `datboi-index`, `datboi-catalog`,
  `datboi-ingest`) log via `tracing` (D81: INFO job boundaries, WARN
  self-heals, ERROR dead subsystems, DEBUG per-item verdicts). The CLI
  crate's `eprintln!` is user-facing output, not logging — leave it.
- Comments in this codebase explain WHY and cite rulings; match that
  register. Analyzer verdicts: deterministic conclusions about bytes
  are `Negative` with detail; `Err` is environmental only (D81).
