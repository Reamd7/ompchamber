/**
 * resolveAgentToolAction（actions.js）的测试套件。
 *
 * 覆盖三条路径：工具内裸名解析（含跨工具歧义、工具内无歧义的 delete）、
 * 未识别调用者的全表唯一匹配，以及解析失败时错误文案必须列出该工具
 * 实际可用的动作而不是笼统的"不支持"。
 */
import { describe, expect, test } from 'bun:test';

import { resolveAgentToolAction } from './actions.js';

/**
 * Both cases here are from one real conversation: the model called `read` and
 * then `get` on `ompchamber_memory`, having dropped the namespace its own tool
 * name appeared to supply, and gave up after the second bare "unsupported".
 */
// 工具名已隐含命名空间时的解析：裸名在发起工具内闭合，不越界。
describe('a namespace the tool name already implies', () => {
  test('resolves a bare action inside the calling tool', () => {
    expect(resolveAgentToolAction('read', 'ompchamber_memory')).toEqual({ action: 'memory.read' });
    expect(resolveAgentToolAction('save', 'ompchamber_memory')).toEqual({ action: 'memory.save' });
  });

  test('resolves a bare name that is ambiguous only across tools', () => {
    // `delete` belongs to schedule and to memory; inside one tool it is plain.
    expect(resolveAgentToolAction('delete', 'ompchamber_memory')).toEqual({ action: 'memory.delete' });
    expect(resolveAgentToolAction('delete', 'ompchamber')).toEqual({ action: 'schedule.delete' });
  });

  test('keeps a fully qualified action as it is', () => {
    expect(resolveAgentToolAction('memory.read', 'ompchamber_memory')).toEqual({ action: 'memory.read' });
  });

  test('does not reach outside the tool that asked', () => {
    // The memory tool asking for `open` must fail, not drive the browser.
    expect(resolveAgentToolAction('open', 'ompchamber_memory').action).toBeUndefined();
  });
});

// 未识别调用工具（toolName 为 null/未知）时的解析：只在全表唯一时放行。
describe('an unidentified caller', () => {
  test('still resolves a bare name that means one thing everywhere', () => {
    expect(resolveAgentToolAction('snapshot', null)).toEqual({ action: 'browser.snapshot' });
  });

  test('refuses a bare name that several actions share', () => {
    expect(resolveAgentToolAction('list', null).action).toBeUndefined();
  });
});

// 解析失败的错误文案契约：列出发起工具可用的动作，缺失动作点名 missing。
describe('what an unresolvable action reports', () => {
  test('names the actions the calling tool actually has', () => {
    const { error } = resolveAgentToolAction('get', 'ompchamber_memory');

    expect(error).toContain('memory.read');
    expect(error).toContain('memory.save');
    // Listing every action of every tool would bury the four that apply.
    expect(error).not.toContain('browser.open');
  });

  test('reports a missing action rather than resolving to something', () => {
    const { error, action } = resolveAgentToolAction('', 'ompchamber_memory');

    expect(action).toBeUndefined();
    expect(error).toContain('missing');
  });

  test('an unknown tool falls back to the full action list', () => {
    const { error } = resolveAgentToolAction('nonsense', 'ompchamber_future');

    expect(error).toContain('memory.read');
    expect(error).toContain('browser.open');
  });
});
