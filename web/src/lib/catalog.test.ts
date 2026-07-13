import { readFileSync } from 'node:fs';
import { join } from 'node:path';
import { describe, expect, test } from 'vitest';

// The PO catalog is the source of truth for translators (D67). These tests
// pin the msgctxt round-trip: `@wc-context` at the call site must surface as
// a real gettext context, keeping colliding English senses apart.
// (vitest runs with cwd = the project root; import.meta.url is not a file:
// URL under happy-dom, so resolve from cwd.)
const catalog = readFileSync(join(process.cwd(), 'src/locales/en.po'), 'utf8');

/** All `msgctxt`s a given msgid appears under (no msgctxt line = undefined). */
function contextsOf(msgid: string): (string | undefined)[] {
  const contexts: (string | undefined)[] = [];
  let pending: string | undefined;
  for (const line of catalog.split('\n')) {
    const ctx = line.match(/^msgctxt "(.*)"$/);
    if (ctx) {
      pending = ctx[1];
      continue;
    }
    if (line === `msgid "${msgid}"`) {
      contexts.push(pending);
    }
    if (!line.startsWith('#')) {
      pending = undefined;
    }
  }
  return contexts;
}

describe('en.po msgctxt round-trip', () => {
  test.each(['verified', 'claimed', 'missing', 'no dump'])(
    'state word %j is disambiguated as a storage state',
    (word) => {
      expect(contextsOf(word)).toContain('storage state');
    },
  );

  // Marks are CSS-drawn now (87-web-ui.md) — the rail labels are the
  // bare words, still disambiguated as storage states.
  test.each(['Verified', 'Claimed', 'Missing', 'No dump'])(
    'rail label %j is a storage state',
    (label) => {
      expect(contextsOf(label)).toContain('storage state');
    },
  );

  test.each(['{0} verified', '{0} claimed', '{0} missing'])(
    'home-card count %j is a storage state',
    (msgid) => {
      expect(contextsOf(msgid)).toContain('storage state');
    },
  );

  test('"Views" is disambiguated as a compiled shelf', () => {
    expect(contextsOf('Views')).toContain('compiled shelf');
  });

  test('unambiguous prose carries no context', () => {
    expect(contextsOf('no recipe consumes this blob')).toEqual([undefined]);
  });

  test('composed strings keep their glyphs inside the msgid (spec §6)', () => {
    for (const msgid of ['⬇ missing-list', 'done ✓']) {
      expect(contextsOf(msgid).length, msgid).toBeGreaterThan(0);
    }
  });

  test('ignored strings stay out of the catalog', () => {
    // The wordmark (@wc-ignore), the pre-catalog loading message, the
    // CLI incantation, and key-comparison literals. (The brand does
    // appear inside the missing-list header msgid — that one is copy.)
    expect(contextsOf('datboi')).toEqual([]);
    expect(catalog).not.toContain('Loading translations');
    expect(catalog).not.toContain('datboi dat import');
    expect(contextsOf('Escape')).toEqual([]);
    expect(contextsOf('Enter')).toEqual([]);
  });
});
