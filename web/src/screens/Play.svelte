<script lang="ts">
  /**
   * Play (D84, docs/88-emulation.md): a view file running in a browser
   * emulator core. Reached from the Browse entry panel's ▶; play
   * rights are exactly download rights — the ROM bytes come from the
   * same verified /view/{name}/{path} surface the download anchor
   * uses, so a view a session can't download from 404s here too (D68,
   * D84 amendment).
   *
   * This screen owns presentation and input collection; the worker
   * lifecycle, audio scheduling, and protocol live in lib/emu/session.
   */
  import { viewFileUrl } from '../lib/api/client';
  import Link from '../lib/components/Link.svelte';
  import LoadError from '../lib/components/LoadError.svelte';
  import { keyBit, gamepadBits } from '../lib/emu/input';
  import type { Descriptor, Touch } from '../lib/emu/protocol';
  import { coreForPath } from '../lib/emu/registry';
  import { EmuSession } from '../lib/emu/session';

  let { view, path }: { view: string; path: string } = $props();

  // App keys this screen on view/path, so props never change in-place —
  // $derived anyway so the compiler agrees.
  const core = $derived(coreForPath(path));
  const basename = $derived(path.split('/').at(-1) ?? path);

  type Phase =
    | { st: 'loading' }
    | { st: 'error'; msg: string }
    | { st: 'booting' }
    | { st: 'running' };
  let phase = $state<Phase>({ st: 'loading' });
  let attempt = $state(0);
  let audioUnlocked = $state(false);

  let canvas = $state<HTMLCanvasElement | null>(null);
  let session: EmuSession | null = null;
  let descriptor: Descriptor | null = null;

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
  // are re-polled every frame (the Gamepad API is poll-only).
  let keyBits = 0;
  let touch: Touch = null;

  function draw(video: Uint32Array) {
    if (ctx === null || image === null || image32 === null) return;
    // Force alpha opaque — the core leaves it undefined.
    for (let i = 0; i < video.length; i++) image32[i] = video[i] | 0xff000000;
    ctx.putImageData(image, 0, 0);
  }

  function sendInput() {
    let bits = keyBits;
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
      const [descResp, romResp] = await Promise.all([
        fetch(`${core.base}/descriptor.json`),
        fetch(viewFileUrl(view, path)),
      ]);
      if (!descResp.ok) throw new Error(`emulator core missing (${descResp.status})`);
      if (!romResp.ok) throw new Error((await romResp.text()) || `rom fetch failed (${romResp.status})`);
      const desc = (await descResp.json()) as Descriptor;
      const rom = await romResp.arrayBuffer();
      descriptor = desc;
      width = Math.max(...desc.screens.map((s) => s.width));
      height = desc.screens.reduce((a, s) => a + s.height, 0);
      if (desc.pointerScreen !== null) {
        pointerTop = desc.screens.slice(0, desc.pointerScreen).reduce((a, s) => a + s.height, 0);
        pointerHeight = desc.screens[desc.pointerScreen].height;
      }
      phase = { st: 'booting' };
      session = new EmuSession(core.base, desc, rom, {
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

  /** First gesture unlocks audio (browser autoplay policy). */
  function unlock() {
    if (session === null || audioUnlocked) return;
    session.unlockAudio();
    audioUnlocked = true;
  }

  function onkeydown(event: KeyboardEvent) {
    if (event.metaKey || event.ctrlKey || event.altKey) return;
    unlock();
    const bit = descriptor === null ? 0 : keyBit(descriptor, event.code);
    if (bit === 0) return;
    event.preventDefault();
    keyBits |= bit;
    sendInput();
  }

  function onkeyup(event: KeyboardEvent) {
    const bit = descriptor === null ? 0 : keyBit(descriptor, event.code);
    if (bit === 0) return;
    event.preventDefault();
    keyBits &= ~bit;
    sendInput();
  }

  /** Pointer → stylus on the pointer screen's native coordinates. */
  function pointer(event: PointerEvent, down: boolean) {
    if (canvas === null || descriptor === null || descriptor.pointerScreen === null) return;
    unlock();
    const rect = canvas.getBoundingClientRect();
    const x = Math.floor((event.clientX - rect.left) * (width / rect.width));
    const y = Math.floor((event.clientY - rect.top) * (height / rect.height)) - pointerTop;
    touch = down && x >= 0 && x < width && y >= 0 && y < pointerHeight ? { x, y } : null;
    sendInput();
  }
</script>

<svelte:window {onkeydown} {onkeyup} />

<main>
  <div class="head">
    <Link class="back" href={`/shelf/${encodeURIComponent(view)}`}>← {view}</Link>
    <span class="title">{basename}</span>
  </div>

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
      <div class="stage">
        <canvas
          bind:this={canvas}
          {width}
          {height}
          onpointerdown={(e) => {
            canvas?.setPointerCapture(e.pointerId);
            pointer(e, true);
          }}
          onpointermove={(e) => {
            if (e.buttons !== 0) pointer(e, true);
          }}
          onpointerup={(e) => pointer(e, false)}
          onpointercancel={(e) => pointer(e, false)}
        ></canvas>
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
    color: var(--fg);
  }

  .title {
    font: 600 0.9rem var(--font-data);
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
</style>
