/**
 * Input mapping (D84 v1): one fixed keyboard map and the standard
 * gamepad layout — rebinding is out of scope AND owes a D-ruling
 * against D78 before it exists (open-questions § emulation).
 * Pure functions over the descriptor's button table, so a second
 * core with different buttons needs no host changes here.
 */

import type { Descriptor } from './protocol';

/**
 * KeyboardEvent.code → descriptor button name. Positional (physical
 * key), so the layout survives non-QWERTY keyboards: face buttons on
 * the right hand (X/Z/S/A mirror a DS's A/B/X/Y), shoulders on Q/W,
 * d-pad on arrows.
 */
export const KEYBOARD_MAP: Record<string, string> = {
  KeyX: 'a',
  KeyZ: 'b',
  KeyS: 'x',
  KeyA: 'y',
  KeyQ: 'l',
  KeyW: 'r',
  Enter: 'start',
  Backspace: 'select',
  ArrowUp: 'up',
  ArrowDown: 'down',
  ArrowLeft: 'left',
  ArrowRight: 'right',
};

/** The bit a key contributes, or 0 if unmapped for this core. */
export function keyBit(descriptor: Descriptor, code: string): number {
  const name = KEYBOARD_MAP[code];
  return name === undefined ? 0 : (descriptor.buttons[name] ?? 0);
}

/**
 * Standard-mapping gamepad button index → descriptor button name.
 * Nintendo positions: east=a, south=b, north=x, west=y (a DS player's
 * muscle memory, not the Xbox label under the thumb).
 */
const GAMEPAD_MAP: [number, string][] = [
  [1, 'a'],
  [0, 'b'],
  [3, 'x'],
  [2, 'y'],
  [4, 'l'],
  [5, 'r'],
  [8, 'select'],
  [9, 'start'],
  [12, 'up'],
  [13, 'down'],
  [14, 'left'],
  [15, 'right'],
];

/** Left-stick threshold that counts as a d-pad press. */
const AXIS_DEAD_ZONE = 0.5;

/** Current bitmask contributed by one gamepad (0 when idle). */
export function gamepadBits(descriptor: Descriptor, pad: Gamepad): number {
  let bits = 0;
  for (const [index, name] of GAMEPAD_MAP) {
    if (pad.buttons[index]?.pressed) bits |= descriptor.buttons[name] ?? 0;
  }
  // Left stick doubles as the d-pad — the DS has no analog input.
  const [x, y] = pad.axes;
  if (x !== undefined && x < -AXIS_DEAD_ZONE) bits |= descriptor.buttons['left'] ?? 0;
  if (x !== undefined && x > AXIS_DEAD_ZONE) bits |= descriptor.buttons['right'] ?? 0;
  if (y !== undefined && y < -AXIS_DEAD_ZONE) bits |= descriptor.buttons['up'] ?? 0;
  if (y !== undefined && y > AXIS_DEAD_ZONE) bits |= descriptor.buttons['down'] ?? 0;
  return bits;
}
