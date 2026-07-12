import { afterEach, expect, test, vi } from 'vitest';
import { copyText } from './clipboard';

function stubClipboard(value: unknown) {
  Object.defineProperty(navigator, 'clipboard', { value, configurable: true });
}

afterEach(() => {
  vi.restoreAllMocks();
});

test('a working clipboard copies and answers true', async () => {
  const writeText = vi.fn(() => Promise.resolve());
  stubClipboard({ writeText });
  await expect(copyText('http://shelf.local/view/gba/')).resolves.toBe(true);
  expect(writeText).toHaveBeenCalledWith('http://shelf.local/view/gba/');
});

test('no navigator.clipboard (LAN http, no secure context) answers false — not a throw', async () => {
  stubClipboard(undefined);
  await expect(copyText('anything')).resolves.toBe(false);
});

test('a rejecting writeText (permission denied) answers false — not a throw', async () => {
  stubClipboard({ writeText: vi.fn(() => Promise.reject(new Error('denied'))) });
  await expect(copyText('anything')).resolves.toBe(false);
});
