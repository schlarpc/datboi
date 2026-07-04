# Recipe format & execution

*From design pass R6, ratified with amendments 2026-07-03. Governing
decisions: D3–D5, D18, D21, D23–D25.*

## Model

A **recipe = exactly one operation application**: `op × ordered inputs ×
claimed outputs (+ params)`. No pipelines inside objects — composition
happens through the CAS (a recipe's input may be another recipe's
output); "sequencing" is the emergent DAG. Why single-application:
uniform verification unit; shared subtrees dedupe on intermediate hashes;
planner/GC remain plain SQL graph recursion.

**Multi-output recipes are first-class** (solid archives produce N streams
in one pass; extracting member 899 of a solid 7z decompresses 0–898
regardless). One execution verifies all outputs; index maps each output
hash → (recipe, ordinal). GC nuance: recipe retained if any output
rooted; inputs rooted transitively only for outputs lacking literals.

**Chunk manifests are not a separate type** — a chunked file is an
`assemble` recipe over chunk blobs. Chunker identity (FastCDC-NC2 etc.) is
provenance, not replay input: replay only needs concat, so chunker tuning
never invalidates old recipes.

## Object format

```
datboi/recipe/1\n
<strict canonical CBOR body>
```

RFC 8949 §4.2.1 deterministic encoding; integer map keys; no floats;
encoder rejects non-canonical input rather than normalizing. Identity =
blake3 of the whole blob including prefix.

Fields: op (builtin name+major | wasm {component blake3, WIT world,
export}), ordered inputs [{hash, role?}], ordered outputs [{hash, size,
name?}], params (nested canonical-CBOR bstr, schema owned+versioned by the
op). Output size is claimed so planning/serving can answer length
questions without materializing. Input `role` (e.g. "keys") is
documentation + UI/ACL affordance; positions are normative.

**Amendments over the R6 draft:**

- Large op params (e.g. wild-zip structural skeletons: member names,
  timestamps, extra fields) above ~1 KiB move into their own blob,
  referenced as an input with role "skeleton" — skeletons dedupe across
  near-identical containers and recipes stay uniformly tiny.
- Accepted risk, stated: params canonicalization is only *enforced* for
  ops the local executor understands; peers can mint
  semantically-identical recipes with different bytes → OR-graph
  fragmentation, never incorrectness.

## Builtins (spec-determined only, frozen forever once shipped)

- **`assemble@1`** — ordered segments: `{blob_ix, offset, len}` |
  `{fill: byte, len}` | `{lit: bstr ≤4 KiB}`. Covers concat, slice,
  header add, pad, zero-fill, splice, chunk reassembly.
- **`swap@1`** — bitswap/byteswap/wordswap/wordbyteswap over a segment
  (amendment: promoted to builtin because the header-skipper DSL (D9)
  treats these as core ops; skipper-derived ingest claims are
  engine-level, not long-tail).
- **`zstd-decompress@1`, `xz-decompress@1`, `deflate-decompress@1`** —
  decompression of a valid stream is spec-determined. **Compression is
  never builtin** (encoder output varies by version); anything reproducing
  compressed bytes pins a wasm component. Decompressor params carry an
  optional input **window** `{1: offset, 2: len}` (ratified at M1
  implementation): a zip member is one windowed recipe over the container
  blob rather than a slice-recipe pair with an intermediate
  compressed-slice identity — halves recipe rows at MAME scale and the
  pattern generalizes to seekable-zstd frames.

Everything else is a pinned wasm component (D5/D6).

## Recipes vs policies (D23)

Recipes: pure replayable claims, bit-exact forever, shareable, verified by
hash; worst failure = wasted CPU. Policies: programs that decide (ingest
strategy, 1G1R, view layout); arbitrary and non-deterministic; they *emit*
recipes; never required for integrity. Policy tiers: declarative config →
`datboi:policy@1` wasm components. No embedded scripting language.

## Direction

Rebuild recipes (`original = f(stored pieces)`) license dropping
originals; derive recipes (`piece = g(original)`) serve outputs from
stored sources. Same object type; ingest typically mints both. Ingest
provenance (path, mtime, who, when) is history in the DB/snapshot, never
in recipes — recipes are timeless. One-way transforms: rebuild direction
exists only when discarded data is expressible (`assemble` fill segments
for pad-trims) or stored; the grounding invariant automatically forbids
dropping sole copies of truly lossy cases.

## Execution

Pull-based operator tree; O(chunk-buffer) memory per node; inputs are
recursively-resolved pull streams (D-streaming: nothing buffers whole
blobs). Seekability is a per-node declared capability:

- `assemble`/`swap`: seekable when segment sources are (range reads
  translate arithmetically).
- Stream decompressors: sequential-only.
- Transforms may declare `random-access` inputs → host provides
  `read-at(offset,len)` handles (CHD rebuild, zip central directory).

**Spill rule**: random access demanded on a non-seekable derived input ⇒
executor materializes that intermediate to a bounded temp file.
Correctness first; planner treats spills as cost. Verification is a tee on
every materialization (D4); bao outboards for derived outputs computed on
first full materialization, cached, recomputable (D15).

## Safety & edges

- **Drop safety (D25)**: literals dropped only after successful local
  replay of the rebuild recipe; whole mechanism deterministic.
- **Cycles**: honest creation can't cycle (hash-before-create); malicious
  claim cycles are unresolvable, not dangerous — resolution requires
  grounding in resident bytes (D21); resolver uses DFS + visited set.
- **Depth**: no format limit; executor resource guard (~1024 nodes,
  configurable). Deep chains compactable: mint a flatter recipe for the
  same output; GC eats the old chain (identities never change).
- **Peer recipe fails verification**: poison record
  `failed(error, at, peer)`; prevents re-verify loops; feeds future
  per-peer reputation (D8). Distinct from `pending` (missing inputs/wasm).
- **Late nondeterminism** (scrub finds a previously-verified recipe now
  failing): alarm-level; planner re-pins all literals depending on the
  implicated component hash until resolved.

## Worked examples (abbreviated)

- iNES: `assemble[{header,0,16},{body,0,N}] → headered` (header is a real
  blob — iNES headers recur and dedupe).
- Wild zip stored extracted: rebuild via `xf-zip-build@1` with skeleton
  blob input; no rebuild recipe minted if trial recompression failed (D24)
  — container stays literal, members still extracted.
- Chunked 4 GB ISO: one ~30 KB `assemble` recipe over 900 chunk blobs.
- Switch NSP: derive `decrypt(NSP, keys{role:keys}) → D`; rebuild
  `xf-nca-crypt@1/encrypt(D, keys, params{sections,nonces}) → NSP`;
  planner drops NSP literal, keeps D (D12: keys are ordinary blobs).
- CHD: raw track image `R` stored; rebuild pins one chdman-port component
  forever; container sha1 aliases the rebuild output, softlist internal
  sha1 aliases `R`. Version drift dead by construction.
