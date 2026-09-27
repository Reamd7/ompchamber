/**
 * 功能开关 feature-flag.js 的测试套件。
 *
 * 覆盖：变量未设置时关闭、常见 truthy 拼写（含大小写与首尾空白）开
 * 启、其余拼写（包括 'false'）保持关闭，以及开关逐次调用即时读取而非
 * import 时固化。每个用例后恢复环境变量，避免污染同进程的其它套件。
 */
import { afterEach, describe, expect, test } from 'bun:test';

import { isAgentMemoryFeatureAvailable } from './feature-flag.js';

/** 套件开始前的环境变量原值；afterEach 据此决定恢复还是删除。 */
const original = process.env.OMPCHAMBER_MEMORY_ENABLE;

// 每个用例后把 OMPCHAMBER_MEMORY_ENABLE 还原到套件开始前的状态。
afterEach(() => {
  if (original === undefined) delete process.env.OMPCHAMBER_MEMORY_ENABLE;
  else process.env.OMPCHAMBER_MEMORY_ENABLE = original;
});

// 未发布功能的发布闸门：默认关闭，只有显式的 truthy 拼写才打开。
describe('the unreleased feature gate', () => {
  test('is closed when the variable is unset', () => {
    delete process.env.OMPCHAMBER_MEMORY_ENABLE;
    expect(isAgentMemoryFeatureAvailable()).toBe(false);
  });

  test('opens for the usual truthy spellings', () => {
    for (const value of ['1', 'true', 'TRUE', 'yes', 'on', ' true ']) {
      process.env.OMPCHAMBER_MEMORY_ENABLE = value;
      expect(isAgentMemoryFeatureAvailable()).toBe(true);
    }
  });

  test('stays closed for anything else, including "false"', () => {
    for (const value of ['', '0', 'false', 'no', 'off', 'maybe']) {
      process.env.OMPCHAMBER_MEMORY_ENABLE = value;
      expect(isAgentMemoryFeatureAvailable()).toBe(false);
    }
  });

  test('is read per call, so a process started with it set is what decides', () => {
    delete process.env.OMPCHAMBER_MEMORY_ENABLE;
    expect(isAgentMemoryFeatureAvailable()).toBe(false);
    process.env.OMPCHAMBER_MEMORY_ENABLE = '1';
    expect(isAgentMemoryFeatureAvailable()).toBe(true);
  });
});
