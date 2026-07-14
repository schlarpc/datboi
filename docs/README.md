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
5. [p2p.md](p2p.md) — friends, trust, and byte exchange.
6. [infra.md](infra.md) — nix, builds, deployment.
7. [dats.md](dats.md) — dat ingestion and the naming authority.
8. [schema.md](schema.md) — the index/database schema.
9. [recipes.md](recipes.md) — recipe semantics, verification, serving.
10. [views.md](views.md) — projections, shares, reified images.
11. [cli.md](cli.md) — the operator surface.
12. [web-ui.md](web-ui.md) — **governs every web surface** (persona,
    vocabulary, design rules).
13. [emulation.md](emulation.md) — in-browser cores; the third wasm lane.
14. [saves.md](saves.md) — save persistence, lineage & attribution
    (design pass open).

Cross-cutting records, always current:

- [decisions.md](decisions.md) — the ADR log (D-numbers).
  **Authoritative** over everything, including this index.
- [open-questions.md](open-questions.md) — deferred items, watch items,
  per-session position notes.
- [roadmap.md](roadmap.md) — milestones; read last, it assumes the tour.
