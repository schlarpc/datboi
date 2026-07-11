import { fireEvent, render, screen, waitFor } from '@testing-library/svelte';
import { loadLocale } from 'wuchale/load-utils';
import { afterEach, expect, test, vi } from 'vitest';
import '../locales/main.loader.svelte.js';
import type { AdminUsersBody, ViewDetail } from '../lib/api/types';
import { installClipboard, installFetch } from '../test/mock-api';
import Admin from './Admin.svelte';

await loadLocale('en');

/** Grid columns come from /v1/views — only the name matters here. */
const gbaView: ViewDetail = {
  name: 'gba-everdrive',
  snapshot: null,
  definition: null,
  endpoints: { http: '/view/gba-everdrive/', dav: '/dav/gba-everdrive/' },
  image: null,
};

const admin: AdminUsersBody = {
  users: [
    { username: 'sam', role: 'friend', created_at: 1000, grants: [], sessions: 2 },
    { username: 'riley', role: 'owner', created_at: 1000, grants: [], sessions: 1 },
  ],
  invites: [
    { token_hash: 'ab12'.repeat(16), role: 'friend', expires_at: 9_999_999_999, created_by: 'riley' },
  ],
};

afterEach(() => vi.unstubAllGlobals());

const cellBtn = () => document.querySelector<HTMLButtonElement>('.cell-btn')!;

test('non-owner (403) gets the owner-only empty state', async () => {
  installFetch({ adminStatus: 403, views: [gbaView] });
  render(Admin);
  expect(await screen.findByText(/owner-only/)).toBeTruthy();
  expect(screen.queryByText('+ mint invite URL')).toBeNull();
});

test('grid renders users, owner rows inert, pending invite row dashed', async () => {
  installFetch({ admin, views: [gbaView] });
  render(Admin);

  expect((await screen.findAllByText('sam')).length).toBe(2); // grid row + sessions row
  // Owner row: inert ✓ cell (owners see everything), no toggle button.
  expect(screen.getAllByText('riley').length).toBe(2);
  expect(document.querySelectorAll('.owner-cell').length).toBe(1);
  // Friend row: exactly one toggleable cell for the one view.
  expect(document.querySelectorAll('.cell-btn').length).toBe(1);
  // Pending invite: greyed row + dashed inert cell.
  expect(screen.getByText(/invite ab12a…12 · pending friend/)).toBeTruthy();
  expect(document.querySelectorAll('.pending-cell').length).toBe(1);
  expect(
    screen.getByText('friends see only ✓ views · dashboard/audit stay owner-only'),
  ).toBeTruthy();
});

test('grant toggle is optimistic and sticks on success', async () => {
  installFetch({ admin: structuredClone(admin), views: [gbaView] });
  render(Admin);
  await screen.findAllByText('sam');

  expect(cellBtn().classList.contains('granted')).toBe(false);
  await fireEvent.click(cellBtn());
  await waitFor(() => expect(cellBtn().classList.contains('granted')).toBe(true));
});

test('grant toggle reverts when the server refuses', async () => {
  let release!: () => void;
  const gate = new Promise<void>((resolve) => (release = resolve));
  installFetch({
    admin: structuredClone(admin),
    views: [gbaView],
    grantFail: true,
    grantGate: gate,
  });
  render(Admin);
  await screen.findAllByText('sam');

  await fireEvent.click(cellBtn());
  // Optimistic: the cell shows granted while the request is in flight.
  expect(cellBtn().classList.contains('granted')).toBe(true);
  release();
  // Server said no → revert.
  await waitFor(() => expect(cellBtn().classList.contains('granted')).toBe(false));
});

test('mint invite flow renders the one-time absolute URL with copy', async () => {
  const writeText = installClipboard();
  installFetch({
    admin: structuredClone(admin),
    views: [gbaView],
    minted: { url_path: '/invite#tok123', expires_at: 9_999_999_999 },
  });
  render(Admin);
  await screen.findAllByText('sam');

  expect(screen.queryByText(/shown once/)).toBeNull();
  await fireEvent.click(screen.getByText('+ mint invite URL'));
  await fireEvent.click(screen.getByText('mint'));

  const url = `${location.origin}/invite#tok123`;
  expect(await screen.findByText(url)).toBeTruthy();
  expect(screen.getByText(/shown once — the server keeps only a hash/)).toBeTruthy();

  await fireEvent.click(screen.getByText('⎘ copy'));
  expect(writeText).toHaveBeenCalledWith(url);
});

test('session revoke zeroes the active count', async () => {
  installFetch({ admin: structuredClone(admin), views: [gbaView] });
  render(Admin);
  await screen.findAllByText('sam');

  expect(screen.getByText('2 active')).toBeTruthy();
  const revokes = screen.getAllByText('revoke');
  await fireEvent.click(revokes[0]); // sam's row
  await waitFor(() => expect(screen.queryByText('2 active')).toBeNull());
  expect(screen.getAllByText('0 active').length).toBe(1);
});
