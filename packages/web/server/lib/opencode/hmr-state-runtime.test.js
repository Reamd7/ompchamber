/**
 * hmr-state-runtime 模块的单元测试。
 *
 * 验证 HMR 状态的初始 OpenCode 工作目录取值规则：配置了
 * OMPCHAMBER_OPENCODE_CWD 时使用该值；未配置时回退到用户主目录。
 */
import { describe, expect, it } from 'vitest';

import { createHmrStateRuntime } from './hmr-state-runtime.js';

/** 以给定 env 构造被测 HMR 状态运行时（隔离的 globalThis 与 stateKey）。 */
const createRuntime = (env = {}) => createHmrStateRuntime({
  globalThisLike: {},
  os: { homedir: () => '/Users/example' },
  processLike: { env },
  stateKey: '__testHmrState',
});

// HMR 状态初始工作目录的取值规则
describe('hmr state runtime', () => {
  it('uses configured OpenCode cwd when provided', () => {
    const runtime = createRuntime({ OMPCHAMBER_OPENCODE_CWD: '/tmp/ompchamber-data' });

    expect(runtime.getOrCreateHmrState().openCodeWorkingDirectory).toBe('/tmp/ompchamber-data');
  });

  it('falls back to home directory without configured OpenCode cwd', () => {
    const runtime = createRuntime();

    expect(runtime.getOrCreateHmrState().openCodeWorkingDirectory).toBe('/Users/example');
  });
});
