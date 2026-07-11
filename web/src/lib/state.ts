/**
 * The four-state entry language (spec §1.4), used identically everywhere.
 * Pure data + math — no DOM, no i18n (the translated state words live in
 * components so wuchale extracts them with their disambiguation context).
 */

export const ENTRY_STATES = ['verified', 'claimed', 'missing', 'nodump'] as const;

/**
 * - `verified`: bytes on hand, hash checked against the catalog
 * - `claimed`: bytes rebuildable, not yet re-verified
 * - `missing`: no blob or claim names this hash
 * - `nodump`: the catalog marks this entry as never dumped — nothing to have
 */
export type EntryState = (typeof ENTRY_STATES)[number];

/** Row glyphs (bench register). Symbols, not copy — never translated. */
export const STATE_GLYPHS: Record<EntryState, string> = {
  verified: '●',
  claimed: '◐',
  missing: '○',
  nodump: '–',
};

export type StateCounts = Record<EntryState, number>;

export function totalEntries(counts: StateCounts): number {
  return counts.verified + counts.claimed + counts.missing + counts.nodump;
}

/**
 * Completeness percentage: `round(100 × verified / (total − nodump))`.
 * No-dump entries are excluded from the denominator — they are impossible to
 * have, so they can't count against completeness. A set with nothing
 * obtainable (empty, or all no-dump) is vacuously 100% complete.
 */
export function completenessPct(counts: StateCounts): number {
  const denominator = totalEntries(counts) - counts.nodump;
  if (denominator <= 0) {
    return 100;
  }
  return Math.round((100 * counts.verified) / denominator);
}

export interface BarSegments {
  /** Width of the verified segment, percent of the full track. */
  verified: number;
  /** Width of the claimed segment, percent of the full track. */
  claimed: number;
}

/**
 * Stacked-bar segment widths. Unlike {@link completenessPct}, segments size
 * against the FULL total (no-dump included), so the empty remainder of the
 * track reads as missing + no-dump. An empty set renders an empty track.
 */
export function barSegments(counts: StateCounts): BarSegments {
  const total = totalEntries(counts);
  if (total === 0) {
    return { verified: 0, claimed: 0 };
  }
  return {
    verified: (100 * counts.verified) / total,
    claimed: (100 * counts.claimed) / total,
  };
}
