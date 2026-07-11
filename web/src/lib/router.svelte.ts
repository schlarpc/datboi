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
  | { screen: 'admin' }
  | { screen: 'login' }
  | { screen: 'invite' }
  | { screen: 'notfound' };

/** Pure path → route match, unit-testable without a window. */
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
        return { screen: 'audit', systemId: decodeURIComponent(audit[1]) };
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
