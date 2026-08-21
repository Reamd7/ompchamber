import { describe, expect, test } from 'bun:test';
import { createEnabledModelsMatcher } from './enabledModels';

describe('createEnabledModelsMatcher (01 GAP-10)', () => {
  test('empty pattern list disables filtering', () => {
    expect(createEnabledModelsMatcher([])).toBe(null);
  });

  test('exact provider/id and bare ids match case-insensitively', () => {
    const matcher = createEnabledModelsMatcher(['Anthropic/Claude-X', 'gpt-4.1']);
    expect(matcher!.allows('anthropic', 'claude-x')).toBe(true);
    expect(matcher!.allows('Anthropic', 'Claude-X')).toBe(true);
    expect(matcher!.allows('openai', 'gpt-4.1')).toBe(true);
    expect(matcher!.allows('openai', 'gpt-4o')).toBe(false);
  });

  test('globs match against provider/id', () => {
    const matcher = createEnabledModelsMatcher(['openai/*', '*/*opus*']);
    expect(matcher!.allows('openai', 'anything')).toBe(true);
    expect(matcher!.allows('anthropic', 'claude-opus-4-8')).toBe(true);
    expect(matcher!.allows('anthropic', 'claude-sonnet')).toBe(false);
  });

  test('thinking suffix is stripped before matching', () => {
    const matcher = createEnabledModelsMatcher(['openai/gpt-5:high']);
    expect(matcher!.allows('openai', 'gpt-5')).toBe(true);
  });
});
