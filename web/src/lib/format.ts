/**
 * Small display formatters shared by the shelf and the bench. Pure data
 * transforms — outputs are either glyphs/numbers/units (never translated;
 * the design treats `4 MB` and `3f9a…c2` as data, spec §6 note) or raw
 * fragments a component composes INTO a translatable string.
 */


/**
 * Hash short form (web-ui.md vocabulary): the first 8 hex chars, no
 * ellipsis — the git-short-SHA mental model. One implementation for
 * every truncated hash; the spec §3.2 middle-ellipsis form is dead
 * (proving both ends match is a CAS author's instinct, not a need).
 * Short inputs pass through.
 */
export function shortHash(hash: string): string {
  return hash.slice(0, 8);
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

/** Unix seconds → `YYYY-MM-DD` in the VIEWER'S timezone. The old UTC
 * render put events near midnight on the wrong calendar day for anyone
 * off UTC — and disagreed with fmtAge below, which is local. The ISO
 * shape stays; only the day boundary moves. */
export function fmtDate(unixSecs: number): string {
  const d = new Date(unixSecs * 1000);
  const pad = (n: number) => String(n).padStart(2, '0');
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}`;
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

/**
 * Elapsed span in the fmtAge register: seconds under a minute, then
 * minutes, then hours. For "took 40s" on finished jobs — unit letters
 * are data (spec §6 note). Negative spans clamp to 0s.
 */
export function fmtDuration(secs: number): string {
  const s = Math.max(0, Math.floor(secs));
  if (s < 60) {
    return `${s}s`;
  }
  if (s < 3600) {
    return `${Math.floor(s / 60)}m`;
  }
  return `${Math.floor(s / 3600)}h`;
}

/** Snapshot id chip (spec §6 `snap #a41f`): `#` + first 4 hex chars. */
export function snapShort(hash: string): string {
  return `#${hash.slice(0, 4)}`;
}
