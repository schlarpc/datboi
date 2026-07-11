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
  },
  build: {
    outDir: 'dist',
  },
  test: {
    environment: 'happy-dom',
  },
});
