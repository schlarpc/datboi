// @ts-check
import { adapter as svelte } from '@wuchale/svelte';
import { defineConfig } from 'wuchale';

// D67: i18n is first-class from the first commit. wuchale extracts every
// user-facing string in src/**/*.svelte (+ *.svelte.{js,ts}) into gettext PO
// catalogs at src/locales/<locale>.po and compiles them into the bundle.
// English is the source locale; adding a locale = appending it here, running
// `npm run extract`, and translating the new PO file.
//
// Ambiguous English carries a call-site context (`<!-- @wc-context: ... -->`),
// which round-trips as gettext msgctxt — "claimed" is a storage state, not a
// person's claim; "view" is a compiled shelf, not a UI view.
export default defineConfig({
  locales: ['en'],
  adapters: {
    main: svelte({ loader: 'svelte' }),
  },
  // No LLM auto-translation: extraction must be deterministic and offline
  // (it runs inside `vite build` in the nix sandbox).
  ai: null,
});
