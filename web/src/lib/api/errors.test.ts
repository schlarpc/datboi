import { loadLocale } from 'wuchale/load-utils';
import { expect, test } from 'vitest';
import '../../locales/main.loader.svelte.js';
import { ApiError } from './client';
import { describeError } from './errors.svelte';

await loadLocale('en');

test('a coded error renders the translated line, not server prose', () => {
  expect(describeError(new ApiError(403, 'owner only', 'owner_only'))).toBe(
    'only the owner can do this',
  );
  expect(describeError(new ApiError(401, 'invalid credentials', 'invalid_credentials'))).toBe(
    'wrong username or password',
  );
});

test('detailed codes append the server diagnostic in parentheses', () => {
  expect(describeError(new ApiError(500, 'db exploded', 'internal'))).toBe(
    'the daemon hit an internal error (db exploded)',
  );
});

test('unknown codes and plain errors fall back to the raw message', () => {
  // A newer daemon can mint codes this build doesn't know.
  expect(describeError(new ApiError(418, 'teapot detail', 'from_the_future' as never))).toBe(
    'teapot detail',
  );
  expect(describeError(new ApiError(404, 'no such blob'))).toBe('no such blob');
  expect(describeError(new Error('boom'))).toBe('boom');
  expect(describeError('raw string')).toBe('raw string');
});
