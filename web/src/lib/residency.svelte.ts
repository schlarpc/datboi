/**
 * Residency words, catalog-routed. One Record over the generated union
 * — a new residency variant fails typecheck here until it has a word —
 * shared by every renderer. Replaces format.ts's untranslated
 * residencyLabel and EntryDrawer's private consts, which had drifted
 * into two spellings of the same wire value ('evicted covered' vs
 * 'evicted (covered)').
 *
 * Thunks because module scope evaluates before the locale catalog
 * loads (the errors.svelte.ts pattern).
 */
import type { ResidencyState } from './api/types';

// Lowercase copy, forced into the catalog at statement level.
// @wc-include
const resident = () => 'resident';
// @wc-include
const evictedCovered = () => 'evicted (covered)';
// @wc-include
const absent = () => 'absent';

const LABELS: Record<ResidencyState, () => string> = {
  resident,
  evicted_covered: evictedCovered,
  absent,
};

export const residencyLabel = (residency: ResidencyState): string => LABELS[residency]();
