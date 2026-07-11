import { render, screen } from '@testing-library/svelte';
import { loadLocale } from 'wuchale/load-utils';
import { expect, test } from 'vitest';
import App from './App.svelte';

// happy-dom emulates the client, so catalogs load the client way: register
// the loaders (App imports them) and await the locale before rendering.
await loadLocale('en');

test('renders the shell with wuchale-transformed strings', async () => {
  render(App);

  // {#await loadLocale(locale)} resolves on a microtask; let it settle.
  expect(await screen.findByText('The shelf')).toBeTruthy();

  // Strings that went through extraction (these are catalog lookups at
  // runtime, not literals — the source language round-trips).
  expect(screen.getByText('verified')).toBeTruthy();
  expect(screen.getByText('claimed')).toBeTruthy();
  expect(screen.getByText('missing')).toBeTruthy();
  expect(screen.getByText('no dump')).toBeTruthy();
  expect(screen.getByText('bytes rebuildable, not yet re-verified')).toBeTruthy();

  // Context-disambiguated nav item ("view" = compiled shelf).
  expect(screen.getByText('Views')).toBeTruthy();

  // The wordmark is @wc-ignore'd but still renders.
  expect(screen.getByText('datboi')).toBeTruthy();
});
