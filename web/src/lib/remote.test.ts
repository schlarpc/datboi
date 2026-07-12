import { describe, expect, test } from 'vitest';
import { errorText, failed, loading, ready, settle, type Remote } from './remote';

describe('constructors', () => {
  test('each state is a whole value — no data/error mixing', () => {
    expect(loading()).toEqual({ st: 'loading' });
    expect(ready(42)).toEqual({ st: 'ready', data: 42 });
    expect(failed('nope')).toEqual({ st: 'error', msg: 'nope' });
  });
});

describe('errorText', () => {
  test('Error carries its message; anything else stringifies', () => {
    expect(errorText(new Error('boom'))).toBe('boom');
    expect(errorText('raw')).toBe('raw');
    expect(errorText(404)).toBe('404');
  });
});

describe('settle', () => {
  test('fulfillment lands as ready', async () => {
    let value: Remote<string> = loading();
    settle(Promise.resolve('hi'), (v) => (value = v));
    await tick();
    expect(value).toEqual({ st: 'ready', data: 'hi' });
  });

  test('rejection lands as error with the message', async () => {
    let value: Remote<string> = loading();
    settle(Promise.reject(new Error('down')), (v) => (value = v));
    await tick();
    expect(value).toEqual({ st: 'error', msg: 'down' });
  });

  test('a stale fulfillment is dropped by the guard', async () => {
    let value: Remote<string> = failed('newer answer already here');
    settle(
      Promise.resolve('stale'),
      (v) => (value = v),
      () => false,
    );
    await tick();
    expect(value).toEqual({ st: 'error', msg: 'newer answer already here' });
  });

  test('a stale slow FAILURE cannot overwrite a newer success', async () => {
    let value: Remote<string> = ready('fresh');
    settle(
      Promise.reject(new Error('slow old failure')),
      (v) => (value = v),
      () => false,
    );
    await tick();
    expect(value).toEqual({ st: 'ready', data: 'fresh' });
  });
});

/** Let the promise callbacks in flight run. */
const tick = () => new Promise((resolve) => setTimeout(resolve, 0));
