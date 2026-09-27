/**
 * notification emitter runtime 测试套件。
 *
 * 通过注入 mock 依赖构造运行时，验证通知的三条投递路径：进程内原生回调
 * （onDesktopNotification，Electron 等进程内 shell 使用）、legacy shell 的
 * stdout 单行 JSON 协议（[desktop-notify] 前缀），以及 UI 广播事件中对
 * "已原生投递"标记（desktopNotificationDelivered / desktopStdoutActive）的携带。
 */
import { describe, expect, it, vi } from 'vitest';

import { createNotificationEmitterRuntime } from './emitter-runtime.js';

/**
 * 用默认 mock 依赖构造 emitter 运行时；overrides 可按用例替换任意依赖
 * （如注入 onDesktopNotification 回调或自定义 stdout.write）。
 */
const createRuntime = (overrides = {}) => createNotificationEmitterRuntime({
  process: { stdout: { write: vi.fn() } },
  getDesktopNotifyEnabled: () => true,
  desktopNotifyPrefix: '[desktop-notify]',
  getUiNotificationClients: () => new Set(),
  getBroadcastGlobalUiEvent: () => null,
  ...overrides,
});

/** 验证桌面通知投递与 UI 广播事件的载荷组装。 */
describe('notification emitter runtime', () => {
  it('reports desktop delivery through the injected native callback', () => {
    const onDesktopNotification = vi.fn();
    const runtime = createRuntime({ onDesktopNotification });
    const payload = { title: 'Ready', body: 'Done' };

    expect(runtime.emitDesktopNotification(payload)).toBe(true);
    expect(onDesktopNotification).toHaveBeenCalledWith(payload);
  });

  it('reports stdout desktop delivery for legacy shells', () => {
    const write = vi.fn();
    const runtime = createRuntime({ process: { stdout: { write } } });

    expect(runtime.emitDesktopNotification({ title: 'Ready' })).toBe(true);
    expect(write).toHaveBeenCalledWith('[desktop-notify]{"title":"Ready"}\n');
  });

  it('marks UI broadcasts that were already delivered natively', () => {
    const broadcastGlobalUiEvent = vi.fn();
    const runtime = createRuntime({ getBroadcastGlobalUiEvent: () => broadcastGlobalUiEvent });

    runtime.broadcastUiNotification({ title: 'Ready' }, { desktopNotificationDelivered: true });

    expect(broadcastGlobalUiEvent).toHaveBeenCalledWith({
      type: 'ompchamber:notification',
      properties: {
        title: 'Ready',
        desktopNotificationDelivered: true,
        desktopStdoutActive: true,
      },
    });
  });
});
