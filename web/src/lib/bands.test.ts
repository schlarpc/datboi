import { describe, expect, test } from 'vitest';
import { bandFor } from './bands';

const PALETTE = [
  'var(--band-gba)',
  'var(--band-snes)',
  'var(--band-teal)',
  'var(--band-moss)',
  'var(--band-plum)',
  'var(--band-brass)',
];

describe('bandFor', () => {
  test('spec-named systems get their exact tokens, case-insensitively', () => {
    expect(bandFor('gba')).toBe('var(--band-gba)');
    expect(bandFor('GBA')).toBe('var(--band-gba)');
    expect(bandFor('snes')).toBe('var(--band-snes)');
  });

  test('unknown systems hash deterministically into the palette', () => {
    const color = bandFor('psx');
    expect(color).toBe(bandFor('psx'));
    expect(color).toBe(bandFor('PSX'));
    expect(PALETTE).toContain(color);
  });

  test('every assignment is a token reference, never a literal', () => {
    for (const system of ['gba', 'snes', 'psx', 'n64', 'saturn', 'dreamcast']) {
      expect(bandFor(system)).toMatch(/^var\(--band-[a-z]+\)$/);
    }
  });
});
