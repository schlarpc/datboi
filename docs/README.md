# docs — reading order

Filenames are stable, citation-friendly names; **this index is the one
place that encodes order** (D88 — positional numbers renumber, indexes
don't). Read top to bottom for the architecture tour; jump by name when
a citation sends you.

1. [vision.md](vision.md) — why datboi exists; the product thesis.
2. [cas.md](cas.md) — the content-addressed store: layout, blob typing,
   recipes as the OR-graph, residency.
3. [transforms.md](transforms.md) — byte transforms and the
   determinism contract.
4. [runtime.md](runtime.md) — the wasm sandbox and execution model.
5. [worlds.md](worlds.md) — the component ABI: lanes, semver, vending,
   publishing (D89).
6. [p2p.md](p2p.md) — friends, trust, and byte exchange.
7. [midstates.md](midstates.md) — fetching by dat hash: midstate
   chains, checkpoints, fetch/serve policy (design pass open).
8. [infra.md](infra.md) — nix, builds, deployment.
9. [dats.md](dats.md) — dat ingestion and the naming authority.
10. [schema.md](schema.md) — the index/database schema.
11. [recipes.md](recipes.md) — recipe semantics, verification, serving.
12. [views.md](views.md) — projections, shares, reified images.
13. [cli.md](cli.md) — the operator surface.
14. [web-ui.md](web-ui.md) — **governs every web surface** (persona,
    vocabulary, design rules).
15. [emulation.md](emulation.md) — in-browser cores; the third wasm lane.
16. [saves.md](saves.md) — save persistence, lineage & attribution
    (design pass open).

Cross-cutting records, always current:

- [decisions.md](decisions.md) — the ADR log (D-numbers).
  **Authoritative** over everything, including this index.
- [open-questions.md](open-questions.md) — deferred items, watch items,
  per-session position notes.
- [roadmap.md](roadmap.md) — milestones; read last, it assumes the tour.
