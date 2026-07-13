import { expect, test } from 'vitest';
import { gamepadBits, keyBit } from './input';
import type { Descriptor } from './protocol';
import { coreForPath } from './registry';

/** The nds descriptor's button table (asset/descriptor.json). */
const NDS: Descriptor = {
  id: 'nds',
  name: 'Nintendo DS',
  worker: 'worker.js',
  screens: [
    { width: 256, height: 192 },
    { width: 256, height: 192 },
  ],
  pointerScreen: 1,
  audioSampleRate: 32768,
  frameRate: 59.8261,
  buttons: {
    a: 1,
    b: 2,
    select: 4,
    start: 8,
    right: 16,
    left: 32,
    up: 64,
    down: 128,
    r: 256,
    l: 512,
    x: 65536,
    y: 131072,
  },
  romExtensions: ['.nds'],
  biosSlots: [],
};

const pad = (pressed: number[], axes: number[] = [0, 0]): Gamepad =>
  ({
    buttons: Array.from({ length: 16 }, (_, i) => ({ pressed: pressed.includes(i) })),
    axes,
  }) as unknown as Gamepad;

test('keyboard: the fixed map speaks descriptor bits', () => {
  expect(keyBit(NDS, 'KeyX')).toBe(1); // A
  expect(keyBit(NDS, 'KeyS')).toBe(65536); // X (the high bits)
  expect(keyBit(NDS, 'ArrowDown')).toBe(128);
  expect(keyBit(NDS, 'KeyP')).toBe(0); // unmapped
});

test('gamepad: standard mapping lands on Nintendo positions', () => {
  // east (index 1) is A, south (index 0) is B
  expect(gamepadBits(NDS, pad([1]))).toBe(1);
  expect(gamepadBits(NDS, pad([0]))).toBe(2);
  expect(gamepadBits(NDS, pad([9, 12]))).toBe(8 | 64); // start + up
});

test('gamepad: the left stick is a d-pad past the dead zone', () => {
  expect(gamepadBits(NDS, pad([], [-1, 0]))).toBe(32); // left
  expect(gamepadBits(NDS, pad([], [0.3, 0.9]))).toBe(128); // down only
  expect(gamepadBits(NDS, pad([], [0.3, -0.2]))).toBe(0); // inside the zone
});

test('registry: extension gating is case-insensitive and tail-anchored', () => {
  expect(coreForPath('Games/Alpha (USA).nds')?.id).toBe('nds');
  expect(coreForPath('Games/ALPHA.NDS')?.id).toBe('nds');
  expect(coreForPath('Games/alpha.nds.zip')).toBeNull();
  expect(coreForPath('Games/alpha.gba')).toBeNull();
});
