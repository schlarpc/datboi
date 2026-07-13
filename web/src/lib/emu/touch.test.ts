import { describe, expect, test } from 'vitest';
import {
  DPAD,
  DPAD_DEAD,
  HIT_SLOP,
  clusterControls,
  controlAt,
  dpadDirs,
  hitScore,
} from './touch';

// The DS button set, as descriptor.json declares it.
const DS = new Set([
  'a',
  'b',
  'select',
  'start',
  'right',
  'left',
  'up',
  'down',
  'r',
  'l',
  'x',
  'y',
]);

// A NES-shaped core: no shoulders, no second face pair.
const NES = new Set(['a', 'b', 'select', 'start', 'right', 'left', 'up', 'down']);

describe('clusterControls', () => {
  test('the DS left cluster is shoulder + dpad + select', () => {
    expect(clusterControls('left', DS).map((c) => c.name)).toEqual(['l', 'dpad', 'select']);
  });

  test('the DS right cluster is shoulder + diamond + start', () => {
    expect(clusterControls('right', DS).map((c) => c.name)).toEqual([
      'r',
      'x',
      'y',
      'a',
      'b',
      'start',
    ]);
  });

  test('a core without X/Y/L/R just loses those controls (contract generalizes)', () => {
    expect(clusterControls('left', NES).map((c) => c.name)).toEqual(['dpad', 'select']);
    expect(clusterControls('right', NES).map((c) => c.name)).toEqual(['a', 'b', 'start']);
  });
});

describe('controlAt', () => {
  const right = clusterControls('right', DS);

  test('a press inside a button lands on it', () => {
    const a = right.find((c) => c.name === 'a')!;
    expect(controlAt(right, a.cx, a.cy)?.name).toBe('a');
  });

  test('hit zones are larger than the visuals: just past the edge still presses', () => {
    const a = right.find((c) => c.name === 'a');
    if (a?.shape !== 'circle') throw new Error('a is a circle');
    // 1.2 radii out, straight right — outside the drawn circle,
    // inside the slop, and nothing else is nearer.
    expect(controlAt(right, a.cx + a.r * 1.2, a.cy)?.name).toBe('a');
  });

  test('between two buttons the nearest wins — slop overlap never double-presses', () => {
    const a = right.find((c) => c.name === 'a');
    const x = right.find((c) => c.name === 'x');
    if (a?.shape !== 'circle' || x?.shape !== 'circle') throw new Error('circles');
    // Slightly toward A on the segment between the two centers.
    const hit = controlAt(right, (a.cx + x.cx) / 2 + 2, (a.cy + x.cy) / 2 + 2);
    expect(hit?.name).toBe('a');
  });

  test('a press on empty space is a miss, not a nearest-anything', () => {
    // The cluster's top-right far corner sits beyond every slop zone.
    expect(controlAt(clusterControls('left', NES), 159, 1)).toBeNull();
  });

  test('hitScore is <= 1 exactly inside the drawn shape', () => {
    const select = clusterControls('left', DS).find((c) => c.name === 'select');
    if (select?.shape !== 'rect') throw new Error('select is a rect');
    expect(hitScore(select, select.cx + select.w / 2, select.cy)).toBeCloseTo(1);
    expect(hitScore(select, select.cx, select.cy)).toBe(0);
    expect(hitScore(select, select.cx + select.w, select.cy)).toBeGreaterThan(HIT_SLOP);
  });
});

describe('dpadDirs', () => {
  const r = DPAD.r;

  test('the center dead zone is neutral', () => {
    expect(dpadDirs(0, 0, r)).toEqual([]);
    expect(dpadDirs(r * DPAD_DEAD * 0.9, 0, r)).toEqual([]);
  });

  test.each([
    [r, 0, ['right']],
    [-r, 0, ['left']],
    [0, -r, ['up']],
    [0, r, ['down']],
  ] as const)('cardinal (%d, %d) → %j', (dx, dy, dirs) => {
    expect(dpadDirs(dx, dy, r)).toEqual(dirs);
  });

  test('diagonals light both directions', () => {
    expect(dpadDirs(r, -r, r).sort()).toEqual(['right', 'up']);
    expect(dpadDirs(-r, r, r).sort()).toEqual(['down', 'left']);
  });

  test('sector boundaries sit at 22.5°: 20° stays cardinal, 25° goes diagonal', () => {
    const a20 = (20 * Math.PI) / 180;
    expect(dpadDirs(r * Math.cos(a20), r * Math.sin(a20), r)).toEqual(['right']);
    const a25 = (25 * Math.PI) / 180;
    expect(dpadDirs(r * Math.cos(a25), r * Math.sin(a25), r).sort()).toEqual(['down', 'right']);
  });

  test('no outer bound: a latched pointer far past the pad edge keeps steering', () => {
    expect(dpadDirs(r * 5, 0, r)).toEqual(['right']);
  });
});
