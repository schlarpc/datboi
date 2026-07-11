import { describe, expect, test } from 'vitest';
import { barSegments, completenessPct, totalEntries, type StateCounts } from './state';

const counts = (
  verified: number,
  claimed: number,
  missing: number,
  nodump: number,
): StateCounts => ({ verified, claimed, missing, nodump });

describe('completenessPct', () => {
  test('counts only verified over the obtainable set', () => {
    // 3 verified of (8 total - 2 nodump) = 3/6
    expect(completenessPct(counts(3, 2, 1, 2))).toBe(50);
  });

  test('excludes no-dump entries from the denominator', () => {
    // Every obtainable entry is verified: 100%, despite the no-dumps.
    expect(completenessPct(counts(4, 0, 0, 6))).toBe(100);
  });

  test('rounds to the nearest integer', () => {
    // 2/3 = 66.67 -> 67
    expect(completenessPct(counts(2, 1, 0, 0))).toBe(67);
    // 1/3 = 33.33 -> 33
    expect(completenessPct(counts(1, 2, 0, 0))).toBe(33);
  });

  test('an empty set is vacuously complete', () => {
    expect(completenessPct(counts(0, 0, 0, 0))).toBe(100);
  });

  test('an all-no-dump set is vacuously complete (no division by zero)', () => {
    expect(completenessPct(counts(0, 0, 0, 5))).toBe(100);
  });

  test('nothing verified is 0%', () => {
    expect(completenessPct(counts(0, 3, 4, 1))).toBe(0);
  });
});

describe('barSegments', () => {
  test('sizes segments against the FULL total, no-dump included', () => {
    // Same shelf as the 50% case above: the bar tells a different story
    // than the percentage, by design (spec §1.4).
    const seg = barSegments(counts(3, 2, 1, 2));
    expect(seg.verified).toBeCloseTo(37.5);
    expect(seg.claimed).toBeCloseTo(25);
  });

  test('missing + no-dump are the empty remainder of the track', () => {
    const c = counts(1, 1, 1, 1);
    const seg = barSegments(c);
    expect(seg.verified + seg.claimed).toBeCloseTo(50);
  });

  test('an empty set renders an empty track (no division by zero)', () => {
    expect(barSegments(counts(0, 0, 0, 0))).toEqual({ verified: 0, claimed: 0 });
  });
});

describe('totalEntries', () => {
  test('sums all four states', () => {
    expect(totalEntries(counts(1, 2, 3, 4))).toBe(10);
  });
});
