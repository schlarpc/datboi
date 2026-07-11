/**
 * Per-system band colors (spec §1.1 / §3.1). RULING: the API carries no
 * band color, so the client assigns one deterministically — systems the
 * spec named get their exact tokens (GBA purple, SNES terracotta);
 * everything else hashes its system slug into a small palette of band
 * hues (tokens.css: --band-teal/moss/plum/brass) so a system keeps its
 * color across sessions and machines without any stored state.
 */

const KNOWN: Record<string, string> = {
  gba: 'var(--band-gba)',
  snes: 'var(--band-snes)',
};

const PALETTE = [
  'var(--band-gba)',
  'var(--band-snes)',
  'var(--band-teal)',
  'var(--band-moss)',
  'var(--band-plum)',
  'var(--band-brass)',
];

/** FNV-1a over the lowercased slug — stable, dependency-free. */
function fnv1a(text: string): number {
  let hash = 0x811c9dc5;
  for (let i = 0; i < text.length; i++) {
    hash ^= text.charCodeAt(i);
    hash = Math.imul(hash, 0x01000193) >>> 0;
  }
  return hash;
}

/** CSS color value (a `var(--…)` reference) for a system's band. */
export function bandFor(system: string): string {
  const slug = system.toLowerCase();
  return KNOWN[slug] ?? PALETTE[fnv1a(slug) % PALETTE.length];
}
