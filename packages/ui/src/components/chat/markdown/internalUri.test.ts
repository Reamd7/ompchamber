import { describe, expect, test } from 'bun:test';
import {
  INTERNAL_URI_SCHEMES,
  URI_V1_ENABLED_SCHEMES,
  activeInternalUriSchemeOf,
  findInternalUriMatches,
  internalUriSchemeOf,
  withInternalUriSchemes,
} from './internalUri';

describe('internalUriSchemeOf', () => {
  test('classifies every internal scheme and rejects everything else', () => {
    for (const scheme of INTERNAL_URI_SCHEMES) {
      expect(internalUriSchemeOf(`${scheme}://ref.md`)).toBe(scheme);
    }
    expect(internalUriSchemeOf('https://example.test')).toBeNull();
    expect(internalUriSchemeOf('file:///tmp/a.md')).toBeNull();
    expect(internalUriSchemeOf('localization-guide.md')).toBeNull();
    expect(internalUriSchemeOf('')).toBeNull();
  });
});

describe('activeInternalUriSchemeOf (capability scope)', () => {
  test('returns null outside a scope — no unconditional whitelist', () => {
    expect(activeInternalUriSchemeOf('local://notes.md')).toBeNull();
  });

  test('returns the scheme only while its scope is active', () => {
    const inside = withInternalUriSchemes(['local'], () => activeInternalUriSchemeOf('local://notes.md'));
    expect(inside).toBe('local');
  });

  test('a scheme outside the enabled set stays inert (P1: local only)', () => {
    const inside = withInternalUriSchemes(URI_V1_ENABLED_SCHEMES, () => ({
      local: activeInternalUriSchemeOf('local://a.md'),
      history: activeInternalUriSchemeOf('history://Anna'),
    }));
    expect(inside.local).toBe('local');
    expect(inside.history).toBeNull();
  });

  test('an empty scheme set behaves like no scope', () => {
    const inside = withInternalUriSchemes([], () => activeInternalUriSchemeOf('local://a.md'));
    expect(inside).toBeNull();
  });

  test('scopes restore on exit and survive exceptions', () => {
    try {
      withInternalUriSchemes(['local'], () => { throw new Error('boom'); });
    } catch { /* expected */ }
    expect(activeInternalUriSchemeOf('local://a.md')).toBeNull();
    // A nested no-scope render (e.g. the viewer's own markdown) must not see
    // the outer scope, and the outer scope must be restored after it exits.
    const nested = withInternalUriSchemes(['local'], () =>
      withInternalUriSchemes(null, () => activeInternalUriSchemeOf('local://a.md')),
    );
    expect(nested).toBeNull();
    expect(withInternalUriSchemes(['local'], () => activeInternalUriSchemeOf('local://a.md'))).toBe('local');
  });
});

describe('findInternalUriMatches (bare-text recognition, spec 04 §5.2.5)', () => {
  test('finds enabled scheme URIs among surrounding text', () => {
    expect(findInternalUriMatches('draft saved at local://scratch/notes.md, see it', ['local'])).toEqual([
      { url: 'local://scratch/notes.md', start: 15, end: 39 },
    ]);
  });

  test('ignores schemes not in the enabled set', () => {
    expect(findInternalUriMatches('see history://Anna and local://a.md', ['local'])).toEqual([
      { url: 'local://a.md', start: 23, end: 35 },
    ]);
    expect(findInternalUriMatches('see history://Anna', [])).toEqual([]);
  });

  test('requires a scheme boundary — no mid-word matches', () => {
    expect(findInternalUriMatches('xlocal://a.md', ['local'])).toEqual([]);
    expect(findInternalUriMatches('(local://a.md)', ['local'])).toEqual([
      { url: 'local://a.md', start: 1, end: 13 },
    ]);
  });

  test('drops trailing sentence punctuation and bare scheme forms', () => {
    expect(findInternalUriMatches('open local://plan.md.', ['local'])).toEqual([
      { url: 'local://plan.md', start: 5, end: 20 },
    ]);
    expect(findInternalUriMatches('root is local://.', ['local'])).toEqual([]);
    expect(findInternalUriMatches('nothing here', ['local'])).toEqual([]);
  });

  test('finds multiple matches on one text node', () => {
    const matches = findInternalUriMatches('local://a.md then local://b/c.json end', ['local']);
    expect(matches.map((m) => m.url)).toEqual(['local://a.md', 'local://b/c.json']);
  });
});
