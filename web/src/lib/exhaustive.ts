/**
 * Closed-union render guard. Call from the last `{:else}` of a template
 * branch chain (or a switch `default`) over a generated union: while
 * every variant has its own branch the argument narrows to `never` and
 * this is dead code, but when the contract grows a variant it lands
 * here and the call stops typechecking — `npm run check` fails until
 * the render says something true about the new case, instead of the
 * old `{:else}` quietly wearing the last label as a confident lie.
 * (Where wuchale allows, a `Record<Union, …>` lookup — STATE_GLYPHS in
 * state.ts — does the same job; this is for the i18n-constrained
 * branch chains.)
 */
export function assertNever(value: never): never {
  throw new Error(`unhandled variant: ${String(value)}`);
}
