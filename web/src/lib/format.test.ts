import { loadLocale } from 'wuchale/load-utils';
import { describe, expect, test } from 'vitest';
import '../locales/main.loader.svelte.js';
import { fmtAge, fmtDate, fmtSize, parseRegion, shortHash, snapShort } from './format';
import { residencyLabel } from './residency.svelte';

await loadLocale('en');

describe('shortHash', () => {
  test('renders 5 hex + ellipsis + last 2 (spec §3.2)', () => {
    expect(shortHash('3f9a4c2b1d3f9a4c2b1d3f9a4c2b1d3f9a4c2b1d')).toBe('3f9a4…1d');
  });

  test('short values pass through untouched', () => {
    expect(shortHash('abc')).toBe('abc');
  });
});

describe('parseRegion', () => {
  test('reads the first parenthetical from the dat name', () => {
    expect(parseRegion('Alpha (USA)')).toBe('USA');
    expect(parseRegion('Beta (Japan) (Rev 1)')).toBe('Japan');
  });

  test('names without a parenthetical have no region', () => {
    expect(parseRegion('Alpha')).toBeNull();
  });
});

describe('fmtSize', () => {
  test('matches the comps register', () => {
    expect(fmtSize(17)).toBe('17 B');
    expect(fmtSize(4 * 1024)).toBe('4 KB');
    expect(fmtSize(4 * 1024 * 1024)).toBe('4 MB');
    expect(fmtSize(1.5 * 1024 * 1024 * 1024)).toBe('1.5 GB');
  });
});

describe('fmtDate', () => {
  test("unix seconds → the viewer's local calendar day", () => {
    // Built from local components, so the expectation holds in any TZ.
    const ts = new Date(2026, 6, 12, 15, 0, 0).getTime() / 1000;
    expect(fmtDate(ts)).toBe('2026-07-12');
  });
});

describe('fmtAge', () => {
  const now = 1_780_000_000_000; // ms

  test('minutes under an hour, hours under a day, then days (spec `2h`)', () => {
    expect(fmtAge(1_780_000_000 - 30, now)).toBe('0m');
    expect(fmtAge(1_780_000_000 - 5 * 60, now)).toBe('5m');
    expect(fmtAge(1_780_000_000 - 2 * 3600, now)).toBe('2h');
    expect(fmtAge(1_780_000_000 - 50 * 3600, now)).toBe('2d');
  });

  test('clock skew clamps to 0m instead of going negative', () => {
    expect(fmtAge(1_780_000_000 + 3600, now)).toBe('0m');
  });
});

describe('snapShort', () => {
  test('renders # + first 4 hex (spec `snap #a41f`)', () => {
    expect(snapShort(`a41f${'0'.repeat(60)}`)).toBe('#a41f');
  });
});

describe('residencyLabel', () => {
  test('wire words render as display copy, not underscores', () => {
    expect(residencyLabel('resident')).toBe('resident');
    expect(residencyLabel('evicted_covered')).toBe('evicted (covered)');
    expect(residencyLabel('absent')).toBe('absent');
  });
});
