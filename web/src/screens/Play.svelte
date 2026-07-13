<script lang="ts">
  /**
   * Play (D84, docs/88-emulation.md): something running in a browser
   * emulator core. Two byte sources (PlaySrc): a view file — reached
   * from the Browse entry panel's ▶, play rights are exactly download
   * rights, the ROM comes from the same verified /view/{name}/{path}
   * surface the download anchor uses, so a view a session can't
   * download from 404s here too (D68, D84 amendment) — or a raw blob
   * by hash, reached from the audit drawer's ▶ over the owner-only
   * /v1 bytes surface (D85).
   *
   * This screen owns presentation and input collection; the worker
   * lifecycle, audio scheduling, and protocol live in lib/emu/session.
   */
  import { blobBytesUrl, viewFileUrl } from '../lib/api/client';
  import Link from '../lib/components/Link.svelte';
  import LoadError from '../lib/components/LoadError.svelte';
  import type { PlaySrc } from '../lib/router.svelte';
  import { session as auth } from '../lib/session.svelte';
  import { keyBit, gamepadBits } from '../lib/emu/input';
  import type { Descriptor, Touch } from '../lib/emu/protocol';
  import { coreForPath } from '../lib/emu/registry';
  import { EmuSession } from '../lib/emu/session';
  import TouchCluster from '../lib/emu/TouchCluster.svelte';

  let { src }: { src: PlaySrc } = $props();

  // App keys this screen on the source, so props never change
  // in-place — $derived anyway so the compiler agrees.
  const filename = $derived(src.kind === 'view' ? src.path : src.name);
  const core = $derived(coreForPath(filename));
  const basename = $derived(filename.split('/').at(-1) ?? filename);

  type Phase =
    | { st: 'loading' }
    | { st: 'error'; msg: string }
    | { st: 'booting' }
    | { st: 'running' };
  let phase = $state<Phase>({ st: 'loading' });
  let attempt = $state(0);
  let audioUnlocked = $state(false);

  let canvas = $state<HTMLCanvasElement | null>(null);
  let mainEl = $state<HTMLElement | null>(null);
  let session: EmuSession | null = null;
  // $state because the touch deck renders from it (its button table
  // decides which controls each cluster draws).
  let descriptor = $state<Descriptor | null>(null);

  // Presentation state, sized once the descriptor arrives.
  let ctx: CanvasRenderingContext2D | null = null;
  let image: ImageData | null = null;
  let image32: Uint32Array | null = null;
  let width = $state(0);
  let height = $state(0);
  /** y offset of the pointer screen within the stacked canvas. */
  let pointerTop = 0;
  let pointerHeight = 0;

  // Input state: keyboard bits accumulate from events; gamepad bits
  // are re-polled every frame (the Gamepad API is poll-only); each
  // touch-deck cluster pushes its own bitmask as pointers change.
  let keyBits = 0;
  let deckLeftBits = 0;
  let deckRightBits = 0;
  let touch: Touch = null;

  // Touch deck gate (D86): capability, never preference — the deck
  // renders while the PRIMARY pointer is coarse (a finger) and
  // follows the media query live (a tablet docking a trackpad flips
  // it off). `any-pointer: coarse` would wrongly catch touchscreen
  // laptops, whose primary pointer is fine.
  const coarsePointer = window.matchMedia('(pointer: coarse)');
  let touchDevice = $state(coarsePointer.matches);
  $effect(() => {
    const onchange = () => (touchDevice = coarsePointer.matches);
    coarsePointer.addEventListener('change', onchange);
    return () => coarsePointer.removeEventListener('change', onchange);
  });

  function draw(video: Uint32Array) {
    if (ctx === null || image === null || image32 === null) return;
    // Force alpha opaque — the core leaves it undefined.
    for (let i = 0; i < video.length; i++) image32[i] = video[i] | 0xff000000;
    ctx.putImageData(image, 0, 0);
  }

  function sendInput() {
    let bits = keyBits | deckLeftBits | deckRightBits;
    // Optional-chained: iOS Safari omits the Gamepad API on insecure
    // origins (plain-HTTP LAN is a supported deployment, D70).
    for (const pad of navigator.getGamepads?.() ?? []) {
      if (pad !== null && descriptor !== null) bits |= gamepadBits(descriptor, pad);
    }
    session?.setInput(bits, touch);
  }

  function boot() {
    phase = { st: 'loading' };
    session?.dispose();
    session = null;
    const load = async () => {
      if (core === null) throw new Error('no core plays this file');
      const romUrl = src.kind === 'view' ? viewFileUrl(src.view, src.path) : blobBytesUrl(src.hash);
      const [descResp, romResp] = await Promise.all([
        fetch(`${core.base}/descriptor.json`),
        fetch(romUrl),
      ]);
      if (!descResp.ok) throw new Error(`emulator core missing (${descResp.status})`);
      if (!romResp.ok) throw new Error((await romResp.text()) || `rom fetch failed (${romResp.status})`);
      const desc = (await descResp.json()) as Descriptor;
      const rom = await romResp.arrayBuffer();
      // BIOS-from-CAS (88-emulation.md): try each slot's accepted
      // hashes against the raw-blob surface; any miss (not ingested,
      // friend's 403) leaves the slot empty and the core on HLE.
      const sysFiles: Record<string, ArrayBuffer> = {};
      await Promise.all(
        desc.biosSlots.map(async (slot) => {
          for (const hash of slot.hashes) {
            const resp = await fetch(blobBytesUrl(hash));
            if (resp.ok) {
              sysFiles[slot.name] = await resp.arrayBuffer();
              return;
            }
          }
        }),
      );
      descriptor = desc;
      width = Math.max(...desc.screens.map((s) => s.width));
      height = desc.screens.reduce((a, s) => a + s.height, 0);
      if (desc.pointerScreen !== null) {
        pointerTop = desc.screens.slice(0, desc.pointerScreen).reduce((a, s) => a + s.height, 0);
        pointerHeight = desc.screens[desc.pointerScreen].height;
      }
      phase = { st: 'booting' };
      // The console's firmware nickname is the datboi user (loopback
      // owners have no username — they're "datboi").
      // @wc-ignore — a name, not copy.
      const nickname = auth.username ?? 'datboi';
      session = new EmuSession(core.base, desc, rom, sysFiles, nickname, {
        onloaded: () => (phase = { st: 'running' }),
        onerror: (msg) => (phase = { st: 'error', msg }),
        onframe: (video) => {
          draw(video);
          sendInput(); // gamepad poll rides the frame clock
        },
      });
    };
    load().catch((e: unknown) => {
      phase = { st: 'error', msg: e instanceof Error ? e.message : String(e) };
    });
  }

  $effect(() => {
    void attempt;
    boot();
    return () => {
      session?.dispose();
      session = null;
    };
  });

  // The canvas mounts when the descriptor sizes it; (re)acquire the
  // 2d context and the reused ImageData whenever that happens.
  $effect(() => {
    if (canvas === null || width === 0) return;
    ctx = canvas.getContext('2d');
    image = new ImageData(width, height);
    image32 = new Uint32Array(image.data.buffer);
  });

  // Background tab: stop burning a core and stop the audio clock; come
  // back where you left off (in-session state lives in the worker).
  $effect(() => {
    const onvisibility = () => {
      if (document.hidden) session?.pause();
      else session?.resume();
    };
    document.addEventListener('visibilitychange', onvisibility);
    return () => document.removeEventListener('visibilitychange', onvisibility);
  });

  // Autoplay unlock belongs to the WHOLE screen, not just the canvas:
  // any first gesture anywhere is the player asking for sound, and on
  // a phone the canvas tap path must not be the only unlock route.
  $effect(() => {
    const gesture = () => unlock();
    window.addEventListener('pointerdown', gesture);
    window.addEventListener('keydown', gesture);
    return () => {
      window.removeEventListener('pointerdown', gesture);
      window.removeEventListener('keydown', gesture);
    };
  });

  // Fullscreen (D87): one immersive flag, two mechanisms. The CSS
  // takeover (fixed, inset 0, chrome hidden) always applies; element
  // fullscreen rides along where the platform has it — iPhone Safari
  // doesn't, so the takeover IS the fallback and the flag never lies.
  let immersive = $state(false);

  function enterImmersive() {
    immersive = true;
    // Best-effort: a missing API or a rejection leaves the CSS
    // takeover as the whole feature.
    mainEl?.requestFullscreen?.().catch(() => {});
  }

  function exitImmersive() {
    immersive = false;
    if (document.fullscreenElement !== null) {
      document.exitFullscreen().catch(() => {});
    }
  }

  // The browser exits native fullscreen on its own (Esc, system UI,
  // app switch); the flag must follow or the takeover would linger.
  $effect(() => {
    const onchange = () => {
      if (document.fullscreenElement === null && immersive) immersive = false;
    };
    document.addEventListener('fullscreenchange', onchange);
    return () => document.removeEventListener('fullscreenchange', onchange);
  });

  /**
   * EVERY gesture re-asserts audio, not just the first: iOS moves the
   * AudioContext to interrupted/suspended on app switches, and only a
   * fresh user gesture may resume it — a once-flag here left sound
   * permanently dead after the first backgrounding. unlockAudio() is
   * idempotent; the flag only hides the hint line.
   */
  function unlock() {
    if (session === null) return;
    session.unlockAudio();
    audioUnlocked = true;
  }

  function onkeydown(event: KeyboardEvent) {
    if (event.metaKey || event.ctrlKey || event.altKey) return;
    // Native fullscreen already maps Escape; this covers the CSS
    // takeover (which is all iOS has).
    // @wc-ignore
    if (event.key === 'Escape' && immersive) {
      exitImmersive();
      return;
    }
    unlock();
    const bit = descriptor === null ? 0 : keyBit(descriptor, event.code);
    if (bit === 0) return;
    event.preventDefault();
    keyBits |= bit;
    sendInput();
  }

  // Lowercase attribute copy, forced at statement level (house
  // pattern — the attribute heuristic won't pick it up).
  // @wc-include
  const exitFullscreenLabel = 'exit fullscreen';

  function onkeyup(event: KeyboardEvent) {
    const bit = descriptor === null ? 0 : keyBit(descriptor, event.code);
    if (bit === 0) return;
    event.preventDefault();
    keyBits &= ~bit;
    sendInput();
  }

  /**
   * Pointer → stylus on the pointer screen's native coordinates.
   * Pen state is tracked here rather than read from `event.buttons`:
   * WebKit reports buttons=0 for touch-driven pointermove, which
   * silently eats stylus drags on iOS.
   */
  let penDown = false;

  function pointer(event: PointerEvent, down: boolean) {
    if (canvas === null || descriptor === null || descriptor.pointerScreen === null) return;
    penDown = down;
    // object-fit: contain letterboxes the pixels inside the element
    // box when the CSS constraints break the aspect ratio (narrow
    // phones) — map against the rendered content rect, not the box.
    const rect = canvas.getBoundingClientRect();
    const scale = Math.min(rect.width / width, rect.height / height);
    const ox = rect.left + (rect.width - width * scale) / 2;
    const oy = rect.top + (rect.height - height * scale) / 2;
    const x = Math.floor((event.clientX - ox) / scale);
    const y = Math.floor((event.clientY - oy) / scale) - pointerTop;
    touch = down && x >= 0 && x < width && y >= 0 && y < pointerHeight ? { x, y } : null;
    sendInput();
  }

  function onpointerdown(event: PointerEvent) {
    // Keep the browser from synthesizing mouse events / tap gestures
    // out of the same touch; the canvas is pure game surface.
    event.preventDefault();
    unlock();
    // Touch pointers are implicitly captured, and iOS Safari has
    // thrown on redundant capture calls — never let capture failure
    // take the input path down with it.
    try {
      canvas?.setPointerCapture(event.pointerId);
    } catch {
      // implicit capture is enough
    }
    pointer(event, true);
  }
</script>

<svelte:window {onkeydown} {onkeyup} />

<main bind:this={mainEl} class:immersive>
  <div class="head">
    {#if src.kind === 'view'}
      <Link class="back" href={`/shelf/${encodeURIComponent(src.view)}`}>← {src.view}</Link>
    {:else}
      <!-- Blob play arrives from an audit drawer; the route doesn't
           know which system, so ← lands on the library home (the
           browser's Back still returns to the exact list). -->
      <Link class="back" href="/">← library</Link>
    {/if}
    <span class="title">{basename}</span>
    {#if width > 0}
      <button class="fs" onclick={enterImmersive}>fullscreen</button>
    {/if}
  </div>

  {#if immersive}
    <!-- The one piece of chrome immersive keeps (D87); Escape works
         too, natively or via the handler above. -->
    <button class="fs-exit" onclick={exitImmersive} aria-label={exitFullscreenLabel}>✕</button>
  {/if}

  {#if core === null}
    <p class="line">no core plays this file</p>
  {:else if phase.st === 'error'}
    <div class="line"><LoadError msg={phase.msg} onretry={() => (attempt += 1)} /></div>
  {:else}
    {#if phase.st === 'loading'}
      <p class="line">loading…</p>
    {:else if phase.st === 'booting'}
      <p class="line">starting…</p>
    {:else if !audioUnlocked}
      <!-- The one autoplay exception worth a line; disappears on the
           first input, which is also what enables the sound. -->
      <p class="line">sound starts with your first input</p>
    {/if}
    {#if width > 0}
      <!-- Touch deck (D86): never overlays the pointer screen — the
           clusters own the space letterboxing wastes (below the
           screens in portrait, the side gutters in landscape), so the
           bottom screen stays a pure stylus surface and buttons +
           stylus work simultaneously. -->
      <div class="stage" class:deck={touchDevice}>
        <canvas
          bind:this={canvas}
          {width}
          {height}
          {onpointerdown}
          onpointermove={(e) => {
            if (penDown) pointer(e, true);
          }}
          onpointerup={(e) => pointer(e, false)}
          onpointercancel={(e) => pointer(e, false)}
        ></canvas>
        {#if touchDevice && descriptor !== null}
          <div class="pad pad--l">
            <TouchCluster
              side="left"
              {descriptor}
              onbits={(bits) => {
                deckLeftBits = bits;
                sendInput();
              }}
            />
          </div>
          <div class="pad pad--r">
            <TouchCluster
              side="right"
              {descriptor}
              onbits={(bits) => {
                deckRightBits = bits;
                sendInput();
              }}
            />
          </div>
        {/if}
      </div>
    {/if}
  {/if}
</main>

<style>
  main {
    flex: 1;
    display: flex;
    flex-direction: column;
    min-height: 0;
    padding: 12px var(--pad-x);
    /* A game surface, not a document: touch play (mashing near the
       title, dragging off the stylus screen) keeps triggering text
       selection and the iOS long-press callout. Nothing here is
       worth selecting. */
    user-select: none;
    -webkit-user-select: none;
    -webkit-touch-callout: none;
  }

  .head {
    display: flex;
    align-items: baseline;
    gap: 14px;
    padding-bottom: 10px;
  }

  .head :global(.back) {
    font: 400 0.78125rem var(--font-data);
    color: var(--faint);
    text-decoration: none;
  }

  .head :global(.back:hover) {
    /* --text, not the never-defined --fg this rule used to name (the
       hover only worked via the invalid-var inheritance accident). */
    color: var(--text);
  }

  .title {
    font: 600 0.9rem var(--font-data);
  }

  .fs {
    all: unset;
    cursor: pointer;
    margin-left: auto;
    font: 400 0.78125rem var(--font-data);
    color: var(--faint);
  }

  .fs:hover {
    color: var(--text);
  }

  .line {
    font: 400 0.78125rem var(--font-data);
    color: var(--faint);
    padding: 4px 0;
  }

  .stage {
    flex: 1;
    display: flex;
    align-items: flex-start;
    justify-content: center;
    min-height: 0;
    padding-top: 6px;
  }

  canvas {
    /* Integer-ish upscale, pixels stay pixels; height is the scarce
       axis for a stacked dual screen. */
    image-rendering: pixelated;
    height: min(100%, 768px);
    max-width: 100%;
    /* keep the aspect ratio the descriptor declared */
    object-fit: contain;
    touch-action: none;
    background: #000;
  }

  /* ---- fullscreen takeover (D87) ---- */
  main.immersive {
    position: fixed;
    inset: 0;
    z-index: 30;
    background: var(--bg);
    /* Clear notches and the iOS home indicator. */
    padding: max(10px, env(safe-area-inset-top)) max(10px, env(safe-area-inset-right))
      max(10px, env(safe-area-inset-bottom)) max(10px, env(safe-area-inset-left));
  }

  main.immersive .head {
    display: none;
  }

  /* Fullscreen exists to buy pixels: the windowed 768px cap goes.
     (Deck mode's own canvas rules out-specify this, correctly.) */
  main.immersive canvas {
    height: 100%;
  }

  .fs-exit {
    all: unset;
    cursor: pointer;
    position: absolute;
    top: max(8px, env(safe-area-inset-top));
    right: max(10px, env(safe-area-inset-right));
    z-index: 31;
    padding: 4px 9px;
    font-size: 0.9rem;
    color: var(--faint);
    background: color-mix(in srgb, var(--bg) 65%, transparent);
    border-radius: var(--r-pill);
  }

  .fs-exit:hover {
    color: var(--text);
  }

  /* ---- touch deck layout (D86) ---- */
  /* The canvas shrinks before the deck does: playable beats big.
     Portrait: screens on top, clusters side by side below.
     Longhands, not the grid-template shorthand — the shorthand with
     clamp() track sizes silently failed to apply on iOS Safari,
     collapsing the rows and floating the deck over the canvas. */
  .stage.deck {
    display: grid;
    gap: 10px;
    grid-template-areas:
      'cnv cnv'
      'padl padr';
    /* Two declarations on purpose: if an engine rejects clamp()/dvh
       as a track size, the plain-pixel row above survives instead of
       the rows collapsing wholesale. */
    grid-template-rows: minmax(0, 1fr) 200px;
    grid-template-rows: minmax(0, 1fr) clamp(150px, 30dvh, 240px);
    grid-template-columns: minmax(0, 1fr) minmax(0, 1fr);
    align-items: stretch;
    justify-content: stretch;
  }

  .stage.deck canvas {
    grid-area: cnv;
    width: 100%;
    height: 100%;
    justify-self: center;
    /* Grid items' automatic minimum floors a replaced element at its
       intrinsic size (384px tall here) — on a phone the 1fr row is
       smaller than that, so without this the canvas bleeds into the
       deck band below. overflow: hidden is the spec-guaranteed kill
       switch for the same rule (auto minimums only apply to
       overflow: visible items), belt to min-height's braces. */
    min-width: 0;
    min-height: 0;
    overflow: hidden;
  }

  .pad {
    display: flex;
    min-width: 0;
    min-height: 0;
  }

  .pad--l {
    grid-area: padl;
  }

  .pad--r {
    grid-area: padr;
  }

  /* Landscape: the stacked dual screen is height-bound, so the deck
     takes the gutters letterboxing would have left black. */
  @media (orientation: landscape) {
    .stage.deck {
      grid-template-areas: 'padl cnv padr';
      grid-template-rows: minmax(0, 1fr);
      grid-template-columns: minmax(110px, 1fr) auto minmax(110px, 1fr);
    }

    .stage.deck canvas {
      width: auto;
      max-width: 100%;
    }
  }
</style>
