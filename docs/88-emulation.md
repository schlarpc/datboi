# datboi — in-browser emulation

*Status: design ratified 2026-07-12 as D84; spike milestones 1–3
shipped the same day, e2e-verified against a live daemon (shelf →
entry panel → ▶ → running homebrew). M3: datboi-server embeds the
core asset (D66 pattern, `DATBOI_EMU_DS`) and serves it at
`/emu/nds/`; CSP script-src gains `'wasm-unsafe-eval'` (the one M4
piece M3 cannot run without); `web/src/lib/emu/` is the host
(worker client + AudioContext drift scheduling + keyboard/gamepad/
stylus input), `/play/{view}/{path}` renders in both chromes, and
the ▶ sits in the Browse entry panel UNGATED — play rights are
download rights (D84 amendment), which also made `/shelf/{view}`
an owner-reachable deep link. M1: `nix build .#emu-ds` (nightly pinned
2025-12-20; the 2026-02 nightlies already break dust's portable_simd
usage, proving the pin-by-last-green rule below) produces the wasm +
glue, and the bare test page boots devkitPro homebrew with both
screens rendering, headless-verified. M2: the worker protocol ships
inside the core asset (asset/worker.js + descriptor.json — the
postMessage boundary IS the GPL line), verified end-to-end at a
steady 60 fps / 32768 audio samples per second, 1558 fps flat-out.
One design delta from the sketch below: audio is a PULL API riding
the frame message, not a wasm-held JS callback — passing a Function
into the instance hung create inside a Worker on Chromium 148
headless (fine on the main thread, fine with a debugger attached; an
engine heisenbug we design out rather than debug further, see
src/audio.rs). Watch item logged: dust's homebrew heuristic
(open-questions). This
doc is the design record for the emulator lane; [87-web-ui.md]
(87-web-ui.md) governs the Play surface it produces. The vision line
it serves: "in-browser emulator cores for direct play"
(00-vision.md), pulled forward from the 90-roadmap M7+ frontier list
because the M5 web surface reserved its slot and the enabling
patterns (D58 C-source-to-wasm, D66/D67 embedding) now exist.*

## The third wasm lane (D84)

Everything wasm in this repo before now is a determinism-contract
component: exact-hash-pinned, recipe-referenced, run in wasmtime
under the empty-linker sandbox (D5/D6/D42/D46/D64). Emulator cores
violate every clause of that on purpose — they run in the *browser*,
they want a clock and audio and input, and nothing downstream ever
depends on their byte-exactness. So they are a third lane:

- **Built like unrar** (D58 lane): standalone `datboi-emu-*` crate
  with its own lockfile, upstream source fetched + pinned + patched
  via nix, wasm32 target.
- **Consumed like the web dist** (D66/D67 lane): flake package →
  `DATBOI_*` env var → embedded in datboi-server, served as a static
  asset, **lazy-loaded** — (hashed-filename cache-forever serving is
  still owed; today the names are stable and the files revalidate) —
  a DS core is multiple MB and must not ride the initial bundle.
- **Not like a transform**: no WIT world, no wasmtime, no recipe
  pinning, no determinism gate, no attribution stamping requirement.
  Target is `wasm32-unknown-unknown` + **wasm-bindgen**, not the
  component model.

Without this ruling the repo's gravity pulls every wasm blob through
the component lane; that lane's entire contract is wrong here.

## Core selection: DS first, via dust-core

DS is the first console deliberately — it forces the hard questions
NES would dodge: multi-screen layout, pointer/touch input, worker
architecture under real interpreter performance pressure, and
multi-hundred-MB ROM delivery. The core is **dust**
(kelpsyberry/dust, Rust): the only accuracy-credible DS core that is
a real library crate (`dust-core`, zero frontend deps), already
proven in the browser (its web frontend's architecture — core in a
Web Worker pumping frames, transferable ArrayBuffers to the UI
thread, soft-3D in a second worker, main-thread scheduled
AudioContext, bottom-screen stylus mapping — is the crib sheet), and
able to direct-boot commercial ROMs with **zero Nintendo files** via
built-in HLE BIOS.

Known costs, accepted:

- **Build friction**: dust needs nightly Rust + `-Zbuild-std` + git
  dependencies and is not on crates.io. Pin the exact nightly,
  vendor the git deps. Same class of pain as the unrar toolchain;
  it is the spike's first milestone precisely because the schedule
  risk lives there.
- **Bus factor one** (last main-branch commit 2025-12): treat it
  exactly like unrar — a fetched, pinned snapshot we are prepared to
  patch ourselves (D54 posture, already house doctrine).
- **License**: dust is GPL-3.0; the workspace is MIT. The emu crate
  and its wasm+glue output are GPL regardless of the worker
  boundary. Posture: per-crate license (the `LicenseRef-unRAR`
  precedent), source availability satisfied by the in-repo nix fetch
  recipe + patches. The MIT-licensed hedge if this ever becomes
  unacceptable is SkyEmu (C, DS at beta quality, web-proven).
- **Interpreter performance**: no DS core JITs on wasm — everyone
  interprets. Evidence says typical 2D and moderate 3D titles run
  full speed on ordinary desktops; heavy 3D and low-end mobile drop
  frames. Full-speed desktop DS is the promise; phones are not.

The generalization test is a cheap **second core**: `tetanes-core`
(NES; MIT/Apache, crates.io, explicitly headless, wasm-proven). A
follow-up, not spike scope. GBA rung: rustboyadvance-ng (MIT, core/
frontend split).

## The host contract

Codified up front, deliberately **unfrozen** — cores are not CAS
artifacts, so this contract may churn freely until the second core
exists. It is a TypeScript-level contract in two layers, one
reusable host module in `web/src/lib/emu/`:

**Core descriptor** (static, per core):
- screens: one or more `{width, height}` — DS declares two 256×192;
  the host composes layouts (vertical / horizontal / focus-one) and
  scales nearest-neighbor. The core never knows about layout.
- input capabilities: button set; which screen (if any) accepts
  pointer input.
- audio sample rate.
- accepted ROM extensions/formats (gates the ▶ Play button).
- named BIOS slots, each with a hard-coded list of accepted content
  hashes (empty in v1 — see BIOS below).

**Worker protocol** (per session, shipped M2 — asset/worker.js is
the reference): `load(rom, bios?)` → running · frame pump lives
*inside* the worker, drift-corrected to the DS refresh · each frame
message carries video (256×384 RGBA, transferred) AND that frame's
audio (interleaved f32, pulled from the core via `take_audio` — a JS
callback held inside the wasm instance hangs in a Worker on Chromium
148, so nothing JS ever crosses into wasm) · input is ABSOLUTE state
(`{buttons: bitmask, touch: {x,y}|null}`, diffed worker-side so a
lost message can't wedge a key) · pause / resume / dispose · a
`stress` message runs N frames flat-out for CI throughput checks. No
SharedArrayBuffer required for v1 (dust's proven shape); the
AudioWorklet + SAB ring buffer is the low-latency endgame and the
headers below keep it available.

**Input mapping v1**: fixed default keyboard map; Gamepad API polled
once per frame (standard mapping); pointer events → the
pointer-capable screen. No rebinding (out of scope, and it needs a
ruling against D78 when it arrives — per-device config is arguably
not a preference toggle, but that argument gets made in a D entry,
not assumed).

**Touch deck (D86, shipped post-spike)**: coarse-pointer devices get
CSS-drawn control clusters that never overlay the pointer screen —
they own the space letterboxing wastes (below the stacked screens in
portrait, the side gutters in landscape), so the DS bottom screen
stays a pure stylus surface and buttons + stylus work at once. Press
semantics live in `lib/emu/touch.ts` (pure, unit-tested):
intent-of-press on pointerdown, per-pointer role latch (a d-pad
pointer steers from the pad center for its whole life, 8-way 45°
sectors, no outer bound), slide-to-roll face buttons, hit zones
larger than visuals, haptic tick on rising edges. Cluster layouts
are declared in an abstract unit space and filtered by the
descriptor's button set — the NES core will reuse them unchanged.
Fullscreen rides alongside (D87): one immersive flag, CSS takeover
everywhere, `requestFullscreen()` where the platform has it.

## ROM and BIOS i/o

**ROM**: the browser fetches decoded payload bytes from the existing
verified byte surfaces (`/snap/{hash}` / `/view/{name}/…`),
whole-file into a `Uint8Array`, transferred into the worker. Typical
DS ROMs are 8–128 MB; the 512 MB ceiling is fine in wasm32. Likely
zero new `/v1` endpoints; if entry metadata can't resolve "the
playable payload hash for this entry," that one resolver endpoint is
the only API addition (full D69 ceremony). *Resolved by D85: no
resolver endpoint. The library's audit drawer plays the blob that
satisfies a rom claim directly — `/play/blob/{hash}/{name}` fetching
`GET /v1/blobs/{hash}/bytes` (the endpoint BIOS already added) — so
both Play sources (view path for shelves and friends, raw blob for
the owner's library) ship with zero new API.*

**BIOS**: shipped ahead of schedule, because reality demanded it —
dust's HLE BIOS cannot boot MKDS-class commercial titles (verified
against dust's own web frontend too), so the designed slots-from-CAS
flow landed the same day: descriptor `biosSlots` name accepted
blake3 hashes, the Play screen tries `GET /v1/blobs/{hash}/bytes`
(the one new /v1 endpoint the emu lane ever needed — owner-only, so
a friend's 403 falls back to HLE cleanly), and the dumps are
ordinary ingested blobs — the hash list IS the verification. The
original v1 posture, kept below for the record: v1 ships **no BIOS
story at all** — dust's HLE direct-boot
handles commercial ROMs. The documented later design: each core's
descriptor carries named BIOS slots with accepted content hashes; at
launch the host asks the server which of those hashes exist in CAS
and fetches them. BIOS dumps then need zero special handling — they
are blobs the user ingested, and the hash list *is* the
verification. No upload UI, no BIOS-manager screen (management by
exception). One v1 edge handled anyway: KEY1-encrypted secure-area
dumps (rare; ties into the D83 secure-area lane) refuse HLE boot —
that is a clear error string, never a hang.

## Headers

Both shipped (M3 carried the CSP piece, M4 the rest; e2e-verified
`crossOriginIsolated === true` with the emulator running):

- **COEP `require-corp`** joined the D70 set, with vite dev-server
  parity. It was free — CSP already forbade every cross-origin load —
  and it banks cross-origin isolation (SharedArrayBuffer, AudioWorklet
  ring, threaded 3D) before anything needs it. Watch item: a future
  box-art provider must proxy third-party images anyway (CSP), so no
  collision.
- **`'wasm-unsafe-eval'`** in CSP script-src —
  `script-src 'self'` blocks `WebAssembly.compile` in Chromium.
  Same-origin worker scripts pass under `'self'` unchanged.

## The v1 spike

Deliverable: **an owner clicks ▶ Play on a .nds library entry and
plays a commercial 2D DS game at full speed in desktop
Chrome/Firefox, with sound, keyboard controls, and mouse-as-stylus
on the bottom screen.** (Amended: not owner-only — play rights are
download rights, so the ▶ shows wherever the download anchor does.)

Milestones, in order:

1. **Nix lane** (all the schedule risk; prove it before any Svelte):
   dust fetched + pinned + patched, pinned nightly toolchain,
   vendored git deps, `packages.emu-ds` producing wasm + glue via
   wasm-bindgen, booting a homebrew ROM in a bare test page.
2. **`crates/datboi-emu-ds`**: standalone workspace wrapping
   `dust-core` behind the worker protocol.
3. **Web**: descriptor + protocol types, the `lib/emu/` host module
   (worker lifecycle, canvas compositor with dual-screen layouts,
   audio scheduling, input collection), Play screen/route, ▶ Play
   gated on format. wuchale extraction for new strings (D67).
4. **Headers**: COEP + `'wasm-unsafe-eval'`, dev parity.

Stretch, not gating: touch button overlay for phones; gamepad.

## Explicitly out of v1

Saves persistence (in-session save RAM lives in emulator memory and
evaporates on close — the UI says so honestly, once; the eventual
answer ties into the "writes are ingests" overlay design, not a
novel mechanism), save states, control rebinding, video filters,
fast-forward/rewind, cheats, netplay, mobile performance promises.
(BIOS-from-CAS was on this list and shipped anyway — see above.)
Future quality items recorded for when filters arrive: dust's
wgpu-3d hi-res renderer is the natural first one (WebGPU-in-worker
caveats apply), and the AudioWorklet + SAB audio ring is the
latency endgame the shipped COEP headers keep available.
