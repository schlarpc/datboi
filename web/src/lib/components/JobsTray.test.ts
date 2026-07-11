import { render, screen } from '@testing-library/svelte';
import { loadLocale } from 'wuchale/load-utils';
import { afterEach, expect, test, vi } from 'vitest';
import '../../locales/main.loader.svelte.js';
import { installFetch } from '../../test/mock-api';
import JobRow from './JobRow.svelte';
import JobsTray from './JobsTray.svelte';

await loadLocale('en');

afterEach(() => vi.unstubAllGlobals());

test('idle tray renders the truthful collapsed state', async () => {
  installFetch({ jobs: [] });
  render(JobsTray);
  expect(await screen.findByText('▸ jobs (0)')).toBeTruthy();
  expect(screen.getByText('activity ▾')).toBeTruthy();
});

test('a running job gets a row with name, bar, and percent', async () => {
  // The registry doesn't exist server-side yet; this exercises the
  // forward-written row path against the tray's rendering contract.
  installFetch({ jobs: [{ id: 391, name: 'analyzer-sweep', progress: 61 }] });
  render(JobsTray);
  expect(await screen.findByText('▸ jobs (1)')).toBeTruthy();
  expect(screen.getByText('analyzer-sweep')).toBeTruthy();
  expect(screen.getByText('61%')).toBeTruthy();
  const fill = document.querySelector<HTMLElement>('.fill');
  expect(fill?.style.width).toBe('61%');
});

test('a finished job row flips its label to done ✓', () => {
  render(JobRow, { job: { id: 392, name: 'ingest #391', progress: 100 } });
  expect(screen.getByText('done ✓')).toBeTruthy();
  expect(screen.queryByText('100%')).toBeNull();
});
