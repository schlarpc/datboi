/**
 * A read-through view of `source()` that trails rapid changes by `ms`.
 *
 * For fetch effects keyed off free-typing inputs: the effect reads
 * `dq()` from `const dq = debounced(() => q)` instead of `q`, so a
 * burst of keystrokes issues ONE server query when the typist pauses
 * instead of one per key. Trailing-edge: intermediate values are never
 * observed, so no fetch for "pok" ever fires on the way to "pokemon".
 * The input itself stays bound to the raw state — only the fetch lags.
 *
 * Must be called during component init (it owns an $effect).
 */
export function debounced<T>(source: () => T, ms = 200): () => T {
  let value = $state(source());
  $effect(() => {
    const next = source();
    // Reading `value` here re-runs the effect when the timer lands;
    // the equality check makes that pass (and any echo) a no-op.
    if (next === value) return;
    const timer = setTimeout(() => (value = next), ms);
    return () => clearTimeout(timer);
  });
  return () => value;
}
