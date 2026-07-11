# datboi web UI

The Svelte 5 + Vite SPA for datboi (D17/D67). This is a standalone npm
project — its own `package-lock.json` is the dependency boundary (D54/D66
lockfile discipline): rust edits never invalidate the web build and vice
versa. The built `dist/` is a nix derivation (`nix build .#web`) that the
datboi binary will embed and serve with an SPA fallback, exactly like the
wasm components (D66). Assets are fully self-hosted (fonts via @fontsource) —
the UI must work offline on a NAS.

## Dev workflow

```sh
npm run dev      # vite dev server (real screens will proxy the daemon's API)
npm run build    # vite build -> dist/
npm run check    # svelte-check + tsc over the node-side configs
npm test         # vitest (happy-dom)
npm run extract  # wuchale: (re)extract strings into src/locales/*.po
```

In the nix devshell, `node_modules` is a symlink into the store built from
`package-lock.json` (`.npmrc` has `package-lock-only=true`, so `npm install`
only edits the lockfile; re-enter the shell to realize new deps). Outside
nix, `npm install --package-lock-only=false` gives a plain local install.

## Nix build

```sh
nix build .#web          # dist/ -> $out (index.html + hashed assets)
nix build .#checks.x86_64-linux.web-test   # npm run check + npm test
```

Only `web/` files are inputs to these derivations (`lib.fileset`), and
`node_modules` comes from `importNpmLock.buildNodeModules` — no vendored
hash to maintain. New files must be `git add`ed before `nix build` sees them.

## i18n (D67 — first-class from the first commit)

Every user-facing string flows through [wuchale](https://wuchale.dev):
write natural English in markup, and the vite plugin extracts it into
gettext PO catalogs (`src/locales/en.po`, the source locale) and compiles
catalogs into the bundle. No keys, no wrapper calls.

**Adding a string:** just write it. If its English collides across meanings,
disambiguate at the call site with a context — it becomes a real `msgctxt`:

```svelte
<!-- @wc-context: storage state -->
<span>claimed</span>
```

Established contexts (D67): `storage state` (verified/claimed/missing/no
dump), `compiled shelf` ("view" the noun, D33). Brand names and pre-catalog
text take `<!-- @wc-ignore -->`.

**Extracting:** happens automatically under `npm run dev` / `npm run build`;
`npm run extract` runs it standalone. Commit the `.po` files — they are the
source of truth for translators. Everything else in `src/locales/` is
regenerated at startup and gitignored.

**Adding a locale:** append it to `locales` in `wuchale.config.js`, run
`npm run extract` (creates `src/locales/<locale>.po`), translate the PO
file, commit both.
