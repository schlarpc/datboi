import { describe, expect, test } from 'vitest';
import { fmtDate, fmtSize, parseRegion, shortHash } from './format';

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
  test('unix seconds → UTC date', () => {
    expect(fmtDate(0)).toBe('1970-01-01');
    expect(fmtDate(1_780_000_000)).toBe('2026-05-28');
  });
});
