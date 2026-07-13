# datboi — in-browser emulation

*Status: design ratified 2026-07-12 as D84; nothing shipped yet. This
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
  asset with a hashed filename (cache-forever), **lazy-loaded** —
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

**Worker protocol** (per session): `load(rom, bios?)` →
running · frame pump lives *inside* the worker · video frames out as
transferable ArrayBuffers · audio chunks out, scheduled on the main
thread with drift correction · `setInput(buttons: bitmask, pointer:
{x, y, down} | null)` in · pause / resume / dispose. No
SharedArrayBuffer required for v1 (dust's proven shape); the
AudioWorklet + SAB ring buffer is the low-latency endgame and the
headers below keep it available.

**Input mapping v1**: fixed default keyboard map; Gamepad API polled
once per frame (standard mapping); pointer events → the
pointer-capable screen. No rebinding (out of scope, and it needs a
ruling against D78 when it arrives — per-device config is arguably
not a preference toggle, but that argument gets made in a D entry,
not assumed).

## ROM and BIOS i/o

**ROM**: the browser fetches decoded payload bytes from the existing
verified byte surfaces (`/snap/{hash}` / `/view/{name}/…`),
whole-file into a `Uint8Array`, transferred into the worker. Typical
DS ROMs are 8–128 MB; the 512 MB ceiling is fine in wasm32. Likely
zero new `/v1` endpoints; if entry metadata can't resolve "the
playable payload hash for this entry," that one resolver endpoint is
the only API addition (full D69 ceremony).

**BIOS**: v1 ships **no BIOS story at all** — dust's HLE direct-boot
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

COOP `same-origin` and CORP `same-origin` are already stamped on
every response (D70, hardening.rs). Two additions ride the spike:

- **COEP `require-corp`** joins the D70 set (plus vite dev-server
  parity). It is free today — CSP already forbids every cross-origin
  load — and it buys cross-origin isolation (SharedArrayBuffer,
  AudioWorklet ring) before anything needs it. Watch item: a future
  box-art provider must proxy third-party images anyway (CSP), so no
  collision.
- **`'wasm-unsafe-eval'`** added to CSP script-src —
  `script-src 'self'` blocks `WebAssembly.compile` in Chromium.
  Same-origin worker scripts already pass under `'self'`.

## The v1 spike

Deliverable: **an owner clicks ▶ Play on a .nds library entry and
plays a commercial 2D DS game at full speed in desktop
Chrome/Firefox, with sound, keyboard controls, and mouse-as-stylus
on the bottom screen.** Owner-only; the friend-facing play ACL is a
deferred design item.

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
fast-forward/rewind, cheats, netplay, BIOS-from-CAS, mobile
performance promises.
