/**
 * config-mutation-response 模块的单元测试。
 *
 * 验证两个响应构造器的输出形状：buildDeferredRestartResponse 标记
 * “重启已推迟”（restartDeferred），buildExternalManualRestartResponse
 * 标记“需外部手动重启”（requiresManualRestart）。
 */
import { describe, expect, test } from 'bun:test';

import {
  buildDeferredRestartResponse,
  buildExternalManualRestartResponse,
} from './config-mutation-response.js';

// 配置变更响应构造器的返回结构
describe('config mutation response helpers', () => {
  test('buildDeferredRestartResponse marks restart as deferred', () => {
    expect(buildDeferredRestartResponse('Saved. Restart the engine to apply.')).toEqual({
      success: true,
      requiresReload: false,
      requiresRestart: true,
      restartDeferred: true,
      message: 'Saved. Restart the engine to apply.',
    });
  });

  test('buildExternalManualRestartResponse asks for external restart', () => {
    expect(buildExternalManualRestartResponse('Restart your server.')).toEqual({
      success: true,
      requiresReload: false,
      requiresManualRestart: true,
      message: 'Restart your server.',
    });
  });
});
