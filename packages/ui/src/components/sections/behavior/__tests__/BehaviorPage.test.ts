import { describe, expect, test } from 'bun:test';

import { resolveInitialPrompt } from '@/lib/behaviorPrompt';

describe('resolveInitialPrompt (AGENTS.md file is authoritative)', () => {
  test('an existing file wins over the stored settings copy', () => {
    expect(resolveInitialPrompt('stale settings copy', { exists: true, content: 'file body' }))
      .toBe('file body');
  });

  test('an existing but empty file is still authoritative — the copy must not resurrect', () => {
    expect(resolveInitialPrompt('stale settings copy', { exists: true, content: '' }))
      .toBe('');
  });

  test('a missing file seeds the editor from the stored copy (migration path)', () => {
    expect(resolveInitialPrompt('stored copy', { exists: false, content: '' }))
      .toBe('stored copy');
  });

  test('no file info (endpoint failed) falls back to the stored copy', () => {
    expect(resolveInitialPrompt('stored copy', null)).toBe('stored copy');
  });

  test('nothing stored and no file yields an empty editor', () => {
    expect(resolveInitialPrompt(undefined, { exists: false, content: '' })).toBe('');
    expect(resolveInitialPrompt(undefined, null)).toBe('');
  });
});
