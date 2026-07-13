<script lang="ts">
  /**
   * One side of the touch deck (D86): CSS-drawn controls feeding the
   * same absolute-input bitmask as every other input source. All the
   * geometry and press semantics live in touch.ts (pure, tested);
   * this component maps pointer events into unit space, latches
   * pointer roles, and draws.
   *
   * Press semantics (D86):
   * - intent-of-press: everything happens on pointerdown/-move, never
   *   on click — no synthesized-click latency, no tap gestures.
   * - role latch: a pointer that lands on the d-pad IS the d-pad
   *   until it lifts, steering from the pad center even past the pad
   *   edge; a pointer that lands on buttons re-hit-tests as it moves
   *   (rolling B→A never needs a lift); a miss stays inert so a
   *   resting grip presses nothing.
   *
   * aria-hidden: the deck duplicates the keyboard map, which remains
   * the accessible input — exposing dozens of synthetic buttons to AT
   * would be noise, not access.
   */
  import type { Descriptor } from './protocol';
  import {
    CLUSTER_H,
    CLUSTER_W,
    DPAD,
    clusterControls,
    controlAt,
    dpadDirs,
    type Control,
  } from './touch';

  let {
    side,
    descriptor,
    onbits,
  }: {
    side: 'left' | 'right';
    descriptor: Descriptor;
    /** This cluster's whole bitmask, re-emitted on every change. */
    onbits: (bits: number) => void;
  } = $props();

  const controls = $derived(clusterControls(side, new Set(Object.keys(descriptor.buttons))));

  type Role = 'dpad' | 'buttons' | 'none';
  const pointers = new Map<number, { role: Role; x: number; y: number }>();

  /** Currently-pressed control names (dirs stand in for the d-pad
   * arms) — drives the emitted bits AND the pressed visuals. */
  let pressed = $state<ReadonlySet<string>>(new Set());
  let prevBits = 0;

  let el = $state<HTMLElement | null>(null);

  /** Client → unit coordinates. CSS aspect-ratio pins the element to
   * the unit box's shape, so this is one proportional scale. */
  function unitPoint(event: PointerEvent, rect: DOMRect): { x: number; y: number } {
    return {
      x: ((event.clientX - rect.left) / rect.width) * CLUSTER_W,
      y: ((event.clientY - rect.top) / rect.height) * CLUSTER_H,
    };
  }

  function recompute() {
    const names = new Set<string>();
    for (const p of pointers.values()) {
      if (p.role === 'dpad') {
        for (const dir of dpadDirs(p.x - DPAD.cx, p.y - DPAD.cy, DPAD.r)) names.add(dir);
      } else if (p.role === 'buttons') {
        const control = controlAt(controls, p.x, p.y);
        if (control !== null && control.name !== 'dpad') names.add(control.name);
      }
    }
    pressed = names;
    let bits = 0;
    for (const name of names) bits |= descriptor.buttons[name] ?? 0;
    // Haptic tick on the rising edge only — silently absent where the
    // platform has no motor API (iOS Safari).
    if (bits & ~prevBits) navigator.vibrate?.(8);
    prevBits = bits;
    onbits(bits);
  }

  function onpointerdown(event: PointerEvent) {
    // The deck is pure game input: no synthesized mouse events, no
    // scroll, no long-press callout.
    event.preventDefault();
    if (el === null) return;
    // Same posture as the Play canvas: implicit touch capture is
    // enough when the explicit call throws (iOS Safari has).
    try {
      el.setPointerCapture(event.pointerId);
    } catch {
      // implicit capture is enough
    }
    const { x, y } = unitPoint(event, el.getBoundingClientRect());
    const control = controlAt(controls, x, y);
    const role: Role = control === null ? 'none' : control.name === 'dpad' ? 'dpad' : 'buttons';
    pointers.set(event.pointerId, { role, x, y });
    recompute();
  }

  function onpointermove(event: PointerEvent) {
    const p = pointers.get(event.pointerId);
    if (p === undefined || el === null) return;
    const { x, y } = unitPoint(event, el.getBoundingClientRect());
    p.x = x;
    p.y = y;
    recompute();
  }

  function onpointerup(event: PointerEvent) {
    if (pointers.delete(event.pointerId)) recompute();
  }

  /** Percentage placement within the aspect-pinned cluster box. */
  function place(control: Control): string {
    const [w, h] =
      control.shape === 'circle' ? [control.r * 2, control.r * 2] : [control.w, control.h];
    const left = (((control.shape === 'circle' ? control.cx - control.r : control.cx - w / 2) /
      CLUSTER_W) *
      100).toFixed(2);
    const top = (((control.shape === 'circle' ? control.cy - control.r : control.cy - h / 2) /
      CLUSTER_H) *
      100).toFixed(2);
    return `left:${left}%;top:${top}%;width:${((w / CLUSTER_W) * 100).toFixed(2)}%;height:${((h / CLUSTER_H) * 100).toFixed(2)}%`;
  }
</script>

<div
  class="cluster"
  bind:this={el}
  aria-hidden="true"
  {onpointerdown}
  {onpointermove}
  {onpointerup}
  onpointercancel={onpointerup}
>
  {#each controls as control (control.name)}
    {#if control.name === 'dpad'}
      <!-- Four CSS-drawn arms in a plus (87-web-ui: structure over
           glyph); diagonals read as two arms lit at once. -->
      <div class="dpad" style={place(control)}>
        <div class="arm arm--up" class:on={pressed.has('up')}></div>
        <div class="arm arm--down" class:on={pressed.has('down')}></div>
        <div class="arm arm--left" class:on={pressed.has('left')}></div>
        <div class="arm arm--right" class:on={pressed.has('right')}></div>
      </div>
    {:else}
      <div
        class="btn"
        class:round={control.shape === 'circle'}
        class:on={pressed.has(control.name)}
        style={place(control)}
      >
        <!-- Console button names, not copy — never extracted. -->
        {control.name.toUpperCase()}
      </div>
    {/if}
  {/each}
</div>

<style>
  .cluster {
    /* The unit box's shape, so unit math never letterboxes. Sized by
       whichever constraint binds; auto margins center in the cell. */
    aspect-ratio: 160 / 230;
    max-width: 100%;
    max-height: 100%;
    margin: auto;
    position: relative;
    touch-action: none;
    user-select: none;
    -webkit-user-select: none;
    -webkit-touch-callout: none;
  }

  .btn {
    position: absolute;
    box-sizing: border-box;
    display: flex;
    align-items: center;
    justify-content: center;
    background: var(--panel);
    border: 1.5px solid var(--edge);
    border-radius: var(--r-pill);
    color: var(--mut);
    font: 600 0.7rem var(--font-data);
    letter-spacing: 0.04em;
  }

  .btn.round {
    border-radius: 50%;
  }

  .btn.on {
    background: var(--ink);
    border-color: var(--ink);
    color: var(--bg);
  }

  .dpad {
    position: absolute;
  }

  .arm {
    position: absolute;
    width: 32%;
    height: 32%;
    box-sizing: border-box;
    background: var(--panel);
    border: 1.5px solid var(--edge);
    border-radius: 22%;
  }

  .arm.on {
    background: var(--ink);
    border-color: var(--ink);
  }

  .arm--up {
    left: 34%;
    top: 1%;
  }

  .arm--down {
    left: 34%;
    bottom: 1%;
  }

  .arm--left {
    left: 1%;
    top: 34%;
  }

  .arm--right {
    right: 1%;
    top: 34%;
  }
</style>
