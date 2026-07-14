# Worlds — the component ABI: lanes, semver, vending, publishing

*Status: RATIFIED as D89 (2026-07-13); the break LANDED 2026-07-14
(every world re-cut, guests on the vending crates, fixtures re-blessed,
dev stores wiped, `nix flake check` green). This doc is the canonical
home for the wit ABI and its distribution. Implementation notes that
refine the design are recorded in §landed notes at the end.*

## The diagnosis: one integer, two jobs

The old worlds (`datboi:transform@1`, `@2`, `datboi:extractor@1`)
used the major version as a **profile registry**: @1 = whole-buffer,
@2 = streaming, "@3 reserved for wasip3." The integer was doing two
jobs — profile identity and contract revision — and they don't
compose: a fix to the whole-buffer contract would have become @3,
numerically "above" the streaming @2 it is shape-incompatible with,
and crate vending against "transform 3" would tell an author nothing.
This is the D88 disease (positional numbers pretending to be names)
in the ABI namespace.

**The ruling: profile identity goes in the package NAME; the version
number does only semver within one shape.** A package name is a
*lane*; every published version of a lane is immutable forever (D51's
freeze, restated per-version — it was always really per-version; the
old reading conflated versions with worlds).

## What routes around the ABI (and what doesn't)

`op` is a string and `params` is opaque canonical CBOR, so new
operations, parameters, and components never touch the wit. A world
changes only when the **host↔guest calling convention** changes —
which is why three worlds covered four milestones. The design's job
is to make the rare calling-convention change graceful and stop
everything else from leaking into the ABI. Two leaks existed:

- `descriptor` (wit record + enum) — adding one advisory field or one
  seek-class variant was a structural break: component-model types
  are matched structurally, there is no unknown-field tolerance.
- `member` (six-field wit record) — mtime, unix mode, or link
  metadata would each have forced a new extractor major.

**Ruling: vocabulary surfaces are canonical CBOR, exactly like
params.** `describe` and `enumerate` return `result<list<u8>,
string>` carrying canonical-CBOR maps whose schemas live with the
params schemas. New keys are schema evolution, not ABI changes. The
`result` wrapper also gives `describe` the error channel it never had
(unknown op previously had no polite refusal).

**The advisory-keys rule (load-bearing, because of D64):** newer
components run under older cores, so an old host WILL meet CBOR keys
it doesn't know. Unknown keys are ignored, therefore **added keys
must be advisory-only** — anything a host must understand to execute
correctly is a real lane minor/major, never a new key. Violating this
turns "ignore unknown keys" into a correctness hole.

## Semver policy

Wasmtime's component linker does semver-aware import resolution (the
WASI 0.2.x mechanism): a guest importing `@1.0` instantiates against
a host providing `@1.2`; never the reverse. Instance subtyping means
a host offering extra functions still satisfies a guest compiled
against fewer. So minors have real teeth:

| Change                                            | Bump  |
|---------------------------------------------------|-------|
| Doc comment in the wit                            | patch |
| New method on a host resource / new host interface| minor |
| New export (host PROBES, never requires)          | minor |
| Record field, enum variant, signature, any shape  | major |

Majors are new frozen worlds; the host keeps every old major's linker
forever (support matrix is APPEND-ONLY — never "clean up" an old
linker; module per major in datboi-runtime). Every published version
is immutable; the publish gate enforces it (below).

## The lanes

Directory layout is package-named (D66 amendment): `wit/<lane>/v<n>/`.

### `datboi:streams@1` — the shared stream contract

`source` (sequential pull), `file` (random access), `sink` (push) —
one definition, imported by every lane. Previously the exact-read /
unconditional-write doctrine was copy-pasted between transform@2 and
extractor@1; the determinism-critical contract gets ONE home. The
coupling is correct: if the stream contract changes shape, every lane
IS affected (a streams major cascades lane majors, deliberately).
Contract unchanged from the proven @2 text: `read(n)` returns exactly
`n` bytes, short only at end-of-stream; `read-at` short only past
EOF; `write` accepts every chunk unconditionally — backpressure is
host-side fiber suspension the guest cannot observe. That
unobservability is layer 2 of D5; weaken it and guest-visible bytes
could legally vary.

### `datboi:transform@1` — streaming shape, fresh epoch

Today's @2 interaction model, carried forward whole: guest pulls
inputs (`list<input>`, the `sequential`/`random-access` variant),
pushes outputs (`list<sink>`), `run` to completion, plus
`serve-range` (all inputs arrive random-access there; opaque ops
return an error). `describe` goes CBOR-result as above; seek-class
and random-access-inputs move into the CBOR schema.

**The whole-buffer world DIES.** Interrogating what the host ever did
with "this transform is definitely not streaming": nothing — the
planner's signals are seek-class and random-access-inputs, which live
in the descriptor either way. @1's shape existed for author
convenience and M1-era host simplicity. Author convenience moves to
the guest crate: `datboi_guest_transform::buffered(|inputs| -> ...)`,
a read-all/write-all adapter. Cost, eyes open: D5's "empty import
surface" layer no longer exists for simple transforms — every guest
links the stream imports, and layer 1 is uniformly D46's "the only
imports are host-implemented stream resources."

### `datboi:extractor@1` — reshaped, fresh epoch

Three changes, two demanded by shipping code:

1. **Containers are `list<file>`.** The recipe model was already
   plural (`inputs: vec![InputRef]`); the world was the only place in
   the pipeline hard-coding one container. Multi-volume sets
   (`.part1.rar` — rom collections are full of them) become POLICY
   ("refuse for now") instead of ABI. Single-volume passes a
   one-element list.
2. **`extract` takes a batch**: `list<extract-request>` where a
   request pairs `ix` with its `sink` (resource-in-record is fine;
   the shipping `input` variant already carries resources — parallel
   lists are the fallback if a bindgen chafes). The single-member
   signature made solid-archive ingest O(n²): each call decodes and
   discards all solid predecessors, and the host cannot amortize from
   outside because every run is a fresh deterministic instance.
   Replay stays per-member (a recipe rebuilds one member with a
   one-request batch). New determinism clause, gate-tested with
   subset properties: **member bytes are a pure function of
   (containers, ix) regardless of the request set.**
3. **Both exports gain `params: list<u8>`.** The world had no params
   channel — `ExtractorParams { member_ix }` existed on the recipe
   and was smuggled around the wit. Passwords and charset hints later
   become CBOR keys, not increments. `member_ix` moves from recipe
   params into the request; recipe schema changes ride the wipe.

`enumerate` returns CBOR-result member lists; `member.ix` keeps its
identity contract verbatim in the schema doc ("stable within this
container blob, files only, listed order" — load-bearing for derive
recipes, D4). The trap doctrine (trap = refuse the whole archive; a
result error is the polite twin) carries over unchanged.

### wasip3 / component-model async: declined

Not maturity alone (the target and experimental wasmtime support
exist): native async makes guests OBSERVE readiness, importing host
scheduling into guest-visible state — the exact nondeterminism class
D5 makes unrepresentable. Our stream contract is engineered so
backpressure is invisible; async's point is the opposite. Whether a
deterministic subset exists is a research project, and freezing on an
in-flux binary encoding contradicts freeze-forever besides. What it
would buy is host cost (fibers burn a stack per running guest), not
capability — a future `streams@2` + lane majors, taken only when the
encoding is stable AND the determinism story is proven.

## Vending (crates)

One crate per lane, named `datboi-guest-<lane>` (house grammar:
family prefix then member, like `datboi-xf-*`; crates.io prefix
search surfaces the authoring family; scales to lanes that don't
exist yet):

- `datboi-guest-transform = "1.0"` ⇄ `datboi:transform@1.0`
- `datboi-guest-extractor = "1.0"` ⇄ `datboi:extractor@1.0`

Crate `major.minor` MIRRORS the world version it binds; patch is the
crate's own (adapter fixes, docs). Contents: pregenerated bindings
(`wit_bindgen::generate!` with `pub_export_macro`, wit shipped inside
the crate — consumers add one dep, implement one trait, one
`export!(T with_types_in ...)` line, never see wit-bindgen), the
`buffered()` adapter (transform crate), typed CBOR builders for
descriptor/params/member (where the type safety the CBOR ruling
"loses" comes back for Rust authors). No streams crate: bindgen
generates each world's import glue into its lane crate.

Known gotcha, documented in each crate: bindings generated against
world 1.1 produce components that REQUIRE host ≥1.1 even if the new
surface is unused (semver matching is one-directional), so authors
pin `"1.0"` unless they need a 1.1 feature, and the host's
world-mismatch error names both versions. Non-Rust authors: the
wkg-published wit is the source of truth for jco/componentize-py/
etc.; the highest-leverage artifact is an examples dir, not more
infrastructure.

## Publishing (wkg → GHCR, through nix)

- **Pure**: `wkg` (nixpkgs) encodes each wit package into its wasm
  package encoding — flake outputs, deterministic, hashable (they can
  even live in our own CAS: a datboi can serve its own ABI).
- **Impure**: `nix run .#publish-wit` wraps `wkg publish` to GHCR.
  The script CHECKS-THEN-REFUSES: if the version tag exists remotely,
  abort — immutability enforced at the publish gate. Publishing is a
  job in the EXISTING container workflow, behind the same
  `nix flake check` gate, using the `packages: write` grant already
  there.
- **Signing**: keyless cosign in the same workflow — the GHA OIDC
  identity signs the OCI manifest digests (wit packages AND the
  container image; one setup covers both), certificate to the Rekor
  transparency log. This covers the one edge our hash-pinning cannot:
  a stranger's first fetch, against mutable GHCR tags. Crates.io
  needs nothing extra (immutable versions, trusted publishing).
- Consumers map the `datboi` namespace to the registry in wkg config;
  "curl the raw .wit" stays a legitimate channel — each world is one
  self-contained file plus the streams dep.

## Byte churn & stamps

D54 stamps are UNCHANGED: `revision` = git tree hash of the crate
dir, content-scoped (unrelated commits cannot churn bytes),
verifiable with git alone. Residual churn — cosmetic edits inside a
guest crate — is accepted; a component that proves its own source
outranks hash stability across comment edits. Mitigation is a
scripted repin (rebuild `.#transforms`, rewrite golden constants),
not a weaker stamp. One empirical item rides the break: determine
whether a wit DOC-COMMENT edit changes component bytes (the wit tree
is outside every crate dir, so it's not in the stamp; whether
wit-bindgen's embedded component-type section preserves docs decides
it) and pin the answer with a gate either way.

## Break logistics

Nothing outside dev stores pins the old worlds; the epoch reuses the
clean names (`datboi:transform@1.0.0` etc.) — nothing was ever
published to a registry, and D89 owns the reuse explicitly. The work
list: re-cut the three wit packages; rewrite the "immutable forever"
world headers to the per-version form; port guests (buffered ones to
the sugar) and the runtime (module per major); re-bless the
checked-in determinism/streaming vectors and `unstamped.wasm`;
re-pin goldens; rename `World::Extractor1`-era recipe schema; wipe
dev stores; retire runtime.md §ABI to a pointer here; then the
vending crates and publish jobs. Old-world wit text survives in git
history only.

## Landed notes (2026-07-14, the break as built)

- **WIT doc comments are part of the frozen bytes.** Measured, not
  assumed: a one-line doc edit in streams.wit changed every component's
  bytes (wit-bindgen embeds the doc-bearing encoded wit in the
  component-type custom section). So a wit typo fix is a FORMAT EVENT —
  wit text freezes with the version it documents, and the gates' pinned
  `COMPONENT_BLAKE3` constants are the tripwire that catches anyone
  forgetting this.
- **Stamp format**: `revision` = `tree:<crate-tree>;guest:<guest-tree>`
  (both `git write-tree` hashes, verifiable with
  `git rev-parse <commit>:crates/<crate>` / `:crates/datboi-guest-<lane>`).
- **The buffered sugar is a trait + macro** (`BufferedGuest` +
  `export_buffered!`), not a closure — exports are static trait impls,
  so a closure had nowhere to live. Same author surface otherwise;
  xf-reference is the in-tree proof and the determinism gate runs it.
- **Extractor recipe params are host-interpreted**: the recipe's params
  bstr carries member selection (`ExtractorParams { member_ix }`) which
  the host turns into the one-request batch; the WORLD call passes an
  empty params bstr. When world-level params arrive (passwords), the
  recipe schema grows a forwarded subset — schema evolution, not ABI.
- **Ingest batches at 128 requests per extract pass** — bounds consumer
  threads; solid decode restarts once per batch (accepted cap cost).
- **The wit packages encode with `wasm-tools component wit --wasm`**
  (same binary encoding `wkg wit build` emits; wkg's builder wants a
  registry for cross-package deps, wasm-tools resolves the local
  `deps/` symlinks). `wkg oci push` still does the publishing.
- **Guest crates are no_std + alloc** so ex-unrar (which owns its panic
  handler and C heap) links them; std consumers are unaffected.
