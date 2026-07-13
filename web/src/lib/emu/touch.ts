/**
 * Touch deck geometry (D86): the pure math under the CSS-drawn touch
 * controls, DOM-free so the press semantics are unit-testable. A
 * cluster (one side of the deck) is declared in a fixed abstract unit
 * box; the component pins the on-screen element to the same shape
 * with CSS aspect-ratio, so unit distances stay isotropic and the
 * event mapping is one proportional scale. Controls are filtered by
 * the descriptor's button set, so a second core (NES: no X/Y/L/R)
 * reuses everything unchanged.
 */

export type Control =
  | { name: string; shape: 'circle'; cx: number; cy: number; r: number }
  | { name: string; shape: 'rect'; cx: number; cy: number; w: number; h: number };

/** The cluster unit box. Portrait-ish: shoulder, pad/diamond, pill. */
export const CLUSTER_W = 160;
export const CLUSTER_H = 230;

/**
 * Hit zones are larger than the visuals (D86): a control claims a
 * press this far past its drawn edge, in units of its own size.
 * Nearest-wins below, so overlapping slop zones can't double-press.
 */
export const HIT_SLOP = 1.35;

/** Neutral radius at the d-pad center, as a fraction of the pad radius. */
export const DPAD_DEAD = 0.28;

/** The d-pad's place in the left cluster — exported because latched
 * pointers steer relative to this center for their whole life. */
export const DPAD = { cx: 80, cy: 118, r: 62 };

/** Face-button diamond (right cluster): Nintendo positions. */
const DIAMOND = { cx: 80, cy: 118, off: 38, r: 26 };

/**
 * One side's controls, keeping only what the core's button set names.
 * 'dpad' is the synthetic control the component latches (it resolves
 * to directions via dpadDirs, never to a bit of its own).
 */
export function clusterControls(
  side: 'left' | 'right',
  buttons: ReadonlySet<string>,
): Control[] {
  const controls: Control[] = [];
  if (side === 'left') {
    if (buttons.has('l')) controls.push({ name: 'l', shape: 'rect', cx: 80, cy: 24, w: 140, h: 40 });
    if (buttons.has('up')) controls.push({ name: 'dpad', shape: 'circle', ...DPAD });
    if (buttons.has('select'))
      controls.push({ name: 'select', shape: 'rect', cx: 80, cy: 204, w: 92, h: 32 });
  } else {
    if (buttons.has('r')) controls.push({ name: 'r', shape: 'rect', cx: 80, cy: 24, w: 140, h: 40 });
    const { cx, cy, off, r } = DIAMOND;
    if (buttons.has('x')) controls.push({ name: 'x', shape: 'circle', cx, cy: cy - off, r });
    if (buttons.has('y')) controls.push({ name: 'y', shape: 'circle', cx: cx - off, cy, r });
    if (buttons.has('a')) controls.push({ name: 'a', shape: 'circle', cx: cx + off, cy, r });
    if (buttons.has('b')) controls.push({ name: 'b', shape: 'circle', cx, cy: cy + off, r });
    if (buttons.has('start'))
      controls.push({ name: 'start', shape: 'rect', cx: 80, cy: 204, w: 92, h: 32 });
  }
  return controls;
}

/**
 * Distance from a control in units of its own size: <= 1 inside the
 * drawn shape, growing linearly outside. The shared currency that
 * makes nearest-wins comparable across circles and rects.
 */
export function hitScore(control: Control, x: number, y: number): number {
  if (control.shape === 'circle') {
    return Math.hypot(x - control.cx, y - control.cy) / control.r;
  }
  return Math.max(
    Math.abs(x - control.cx) / (control.w / 2),
    Math.abs(y - control.cy) / (control.h / 2),
  );
}

/** The control a press at (x, y) lands on: nearest within slop, or null. */
export function controlAt(controls: readonly Control[], x: number, y: number): Control | null {
  let best: Control | null = null;
  let bestScore = HIT_SLOP;
  for (const control of controls) {
    const score = hitScore(control, x, y);
    if (score < bestScore) {
      best = control;
      bestScore = score;
    }
  }
  return best;
}

export type Dir = 'up' | 'down' | 'left' | 'right';

/** sin 22.5° — 45° cardinal sectors with 45° diagonal windows between. */
const DIAG = Math.sin(Math.PI / 8);

/**
 * 8-way d-pad resolution for a pointer at (dx, dy) relative to the
 * pad center. Inside the dead zone nothing is active; outside it a
 * direction is active when its axis component exceeds sin 22.5° of
 * the pointer's distance. Deliberately no outer bound: a latched
 * pointer keeps steering after sliding past the pad edge (D86).
 */
export function dpadDirs(dx: number, dy: number, r: number): Dir[] {
  const d = Math.hypot(dx, dy);
  if (d < r * DPAD_DEAD) return [];
  const dirs: Dir[] = [];
  if (dy < -d * DIAG) dirs.push('up');
  if (dy > d * DIAG) dirs.push('down');
  if (dx < -d * DIAG) dirs.push('left');
  if (dx > d * DIAG) dirs.push('right');
  return dirs;
}
