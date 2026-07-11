/**
 * Small display formatters shared by the shelf and the bench. Pure data
 * transforms — outputs are either glyphs/numbers/units (never translated;
 * the design treats `4 MB` and `3f9a…c2` as data, spec §6 note) or raw
 * fragments a component composes INTO a translatable string.
 */

/**
 * Hash rendering per spec §3.2: 5 hex chars, ellipsis, last 2 —
 * `3f9a4c2…c2`-style truncation (`3f9a4…c2`). Short inputs pass through.
 */
export function shortHash(hash: string): string {
  if (hash.length <= 7) {
    return hash;
  }
  return `${hash.slice(0, 5)}…${hash.slice(-2)}`;
}

/**
 * Region parsed from the entry name's first parenthetical (spec §5.15):
 * `Alpha (USA)` → `USA`. The dat name is the source of truth — there is
 * no region field anywhere else. `null` when the name has none.
 */
export function parseRegion(name: string): string | null {
  const match = name.match(/\(([^)]+)\)/);
  return match ? match[1] : null;
}

/**
 * Byte counts in the comps' register (`4 MB`, sizes column). Whole
 * numbers below GB (the column is 46px), one decimal at GB+.
 */
export function fmtSize(bytes: number): string {
  const KB = 1024;
  const MB = KB * 1024;
  const GB = MB * 1024;
  if (bytes < KB) {
    return `${bytes} B`;
  }
  if (bytes < MB) {
    return `${Math.round(bytes / KB)} KB`;
  }
  if (bytes < GB) {
    return `${Math.round(bytes / MB)} MB`;
  }
  return `${(bytes / GB).toFixed(1)} GB`;
}

/** Unix seconds → `YYYY-MM-DD` (UTC), the drawer's verified-date render. */
export function fmtDate(unixSecs: number): string {
  return new Date(unixSecs * 1000).toISOString().slice(0, 10);
}

/**
 * Snapshot age in the comps' register (`snap #a41f · 2h`): minutes under
 * an hour, hours under a day, then days. Unit letters are data (spec §6
 * note), like the size units above. Clock skew clamps to `0m`.
 */
export function fmtAge(unixSecs: number, nowMs: number = Date.now()): string {
  const secs = Math.max(0, Math.floor(nowMs / 1000 - unixSecs));
  if (secs < 3600) {
    return `${Math.floor(secs / 60)}m`;
  }
  if (secs < 86400) {
    return `${Math.floor(secs / 3600)}h`;
  }
  return `${Math.floor(secs / 86400)}d`;
}

/** Snapshot id chip (spec §6 `snap #a41f`): `#` + first 4 hex chars. */
export function snapShort(hash: string): string {
  return `#${hash.slice(0, 4)}`;
}
