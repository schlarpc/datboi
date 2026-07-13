/**
 * The shipped cores (D84). Extensions live here — not in a fetched
 * descriptor — so row-level ▶ gating costs nothing; everything else
 * about a core is runtime data (descriptor.json) so the registry
 * never drifts from the asset it points at.
 */

export type Core = {
  id: string;
  /** Asset base: descriptor.json and worker.js live under here. */
  base: string;
  /** Lowercase, dot-included; matched against the manifest path tail. */
  extensions: string[];
};

export const CORES: Core[] = [{ id: 'nds', base: '/emu/nds', extensions: ['.nds'] }];

/** The core that can play this manifest path, or null. */
export function coreForPath(path: string): Core | null {
  const lower = path.toLowerCase();
  return CORES.find((core) => core.extensions.some((ext) => lower.endsWith(ext))) ?? null;
}

/** SPA route to play a view file (the /play/{view}/{path} screen). */
export const playUrl = (view: string, path: string): string =>
  `/play/${encodeURIComponent(view)}/${path.split('/').map(encodeURIComponent).join('/')}`;
