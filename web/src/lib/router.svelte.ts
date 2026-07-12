/**
 * Hand-rolled history-API router. The route table is six flat paths and
 * one parameterized drill-down — a routing dependency would be all
 * ceremony. The daemon SPA-fallbacks every non-API path to index.html,
 * so deep links land here on first load.
 *
 * Nav ruling (docs/open-questions.md § raised 2026-07-11):
 * `Library · Views · Ingest · Storage · Admin`, with the audit screen
 * as the system drill-down under Library (`/library/{systemId}`).
 */

export type Route =
  | { screen: 'library' }
  | { screen: 'audit'; systemId: string }
  | { screen: 'views' }
  | { screen: 'ingest' }
  | { screen: 'storage' }
  /** Blob inspector, the storage drill-down (hash = blake3 hex). */
  | { screen: 'blob'; hash: string }
  | { screen: 'admin' }
  | { screen: 'login' }
  | { screen: 'invite' }
  /** Friend browse (spec §4.3); owners have `/view/{name}/` instead. */
  | { screen: 'browse'; view: string }
  | { screen: 'notfound' };

/**
 * Total percent-decode: a malformed sequence ('/library/abc%') is null,
 * never a thrown URIError — matchPath runs at module scope, so a throw
 * here would white-screen the app instead of rendering notfound.
 */
function safeDecode(segment: string): string | null {
  try {
    return decodeURIComponent(segment);
  } catch {
    return null;
  }
}

/** Pure path → route match, unit-testable without a window. Total: never throws. */
export function matchPath(pathname: string): Route {
  switch (pathname) {
    case '/':
      return { screen: 'library' };
    case '/views':
      return { screen: 'views' };
    case '/ingest':
      return { screen: 'ingest' };
    case '/storage':
      return { screen: 'storage' };
    case '/admin':
      return { screen: 'admin' };
    case '/login':
      return { screen: 'login' };
    case '/invite':
      // The invite token rides location.hash, not the path (admin.rs:
      // fragments never appear in server logs or Referer headers).
      return { screen: 'invite' };
    default: {
      const audit = pathname.match(/^\/library\/([^/]+)$/);
      if (audit) {
        const systemId = safeDecode(audit[1]);
        if (systemId !== null) return { screen: 'audit', systemId };
      }
      const shelf = pathname.match(/^\/shelf\/([^/]+)$/);
      if (shelf) {
        const view = safeDecode(shelf[1]);
        if (view !== null) return { screen: 'browse', view };
      }
      const blob = pathname.match(/^\/storage\/blob\/([^/]+)$/);
      if (blob) {
        const hash = safeDecode(blob[1]);
        if (hash !== null) return { screen: 'blob', hash };
      }
      return { screen: 'notfound' };
    }
  }
}

const current = $state({ route: matchPath(window.location.pathname) });

export const router = {
  get route(): Route {
    return current.route;
  },
  /** Link clicks: push a history entry and swap the screen. */
  navigate(path: string): void {
    window.history.pushState({}, '', path);
    this.sync();
  },
  /** Redirects (auth bounces): swap without polluting history. */
  replace(path: string): void {
    window.history.replaceState({}, '', path);
    this.sync();
  },
  /** Re-read location (popstate, or after external history changes). */
  sync(): void {
    current.route = matchPath(window.location.pathname);
  },
};

window.addEventListener('popstate', () => router.sync());
