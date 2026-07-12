/**
 * One fetched resource, one value. A resource is always exactly ONE of
 * loading/error/ready — the old screen shape held `data` and a
 * never-reset `error` side by side, so one stale failure could blank a
 * fully-rendered screen forever. Transitions assign a whole new value,
 * render code narrows on `st`, and every resource on a screen gets its
 * own Remote so a failed card never blanks its neighbors.
 */

export type Remote<T> =
  | { st: 'loading' }
  | { st: 'error'; msg: string }
  | { st: 'ready'; data: T };

export const loading = <T>(): Remote<T> => ({ st: 'loading' });
export const ready = <T>(data: T): Remote<T> => ({ st: 'ready', data });
export const failed = <T>(msg: string): Remote<T> => ({ st: 'error', msg });

import { describeError } from './api/errors.svelte';

/** The one spelling for rejection → message: coded ApiErrors map to
 * translated copy (errors.svelte.ts, D77); anything else stringifies. */
export const errorText = describeError;

/**
 * Settle a promise into a Remote through `set`. The caller's `live`
 * closure is the generation guard, and BOTH arms pass through it — a
 * stale slow failure can no more overwrite a newer success than a
 * stale success could.
 */
export function settle<T>(
  promise: Promise<T>,
  set: (value: Remote<T>) => void,
  live: () => boolean = () => true,
): void {
  promise.then(
    (data) => {
      if (live()) set(ready(data));
    },
    (e: unknown) => {
      if (live()) set(failed(errorText(e)));
    },
  );
}
