import { svelte } from '@sveltejs/vite-plugin-svelte';
import { svelteTesting } from '@testing-library/svelte/vite';
import { defineConfig } from 'vitest/config';
import { wuchale } from 'wuchale/vite';

export default defineConfig({
  // wuchale must run before the svelte compiler: it rewrites user-facing
  // strings into catalog lookups at transform time (D67 i18n-first).
  plugins: [wuchale(), svelte({ compilerOptions: { runes: true } }), svelteTesting()],
  // Default cacheDir is node_modules/.vite — read-only when node_modules is
  // a symlink into /nix/store (devshell link hook + nix build both do this).
  cacheDir: '.vite',
  server: {
    fs: {
      // Serve through the store-symlinked node_modules in the nix dev flow.
      allow: ['.', '/nix/store'],
    },
    // Dev-loop proxy: `npm run dev` serves the SPA with HMR while the API
    // and content routes come from a locally running daemon
    // (`datboi serve`, default 127.0.0.1:2352). Loopback is implicitly
    // owner (D68), so local dev needs no auth setup.
    proxy: {
      '/v1': 'http://127.0.0.1:2352',
      '/view': 'http://127.0.0.1:2352',
      '/snap': 'http://127.0.0.1:2352',
    },
  },
  build: {
    outDir: 'dist',
  },
  test: {
    environment: 'happy-dom',
  },
});
