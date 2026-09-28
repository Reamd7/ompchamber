/**
 * createGlobalMessageStreamHub 的测试套件：验证同步/异步订阅者抛错
 * 不中断 fanout，以及 stop()/start() 依托保留 replay 尾部 + epoch
 * 实现的同 boot 续连（不重复摄入环、不向订阅者重复扇出）。
 */

import { describe, expect, it, vi } from 'vitest';

import { createGlobalMessageStreamHub } from './global-hub.js';

/**
 * 构造带自定义响应头（如 x-omp-epoch）与一次性 SSE 块的伪 fetch
 * 响应；块耗尽后立即 done，配合读取器的自动重连驱动测试节奏。
 */
function createSseResponse({ blocks = [], headers = {} } = {}) {
  const encoder = new TextEncoder();
  let index = 0;

  return {
    ok: true,
    headers: {
      get(name) {
        return headers[name.toLowerCase()] ?? null;
      },
    },
    body: {
      getReader() {
        return {
          async read() {
            if (index < blocks.length) {
              return { value: encoder.encode(blocks[index++]), done: false };
            }
            return { value: undefined, done: true };
          },
        };
      },
    },
  };
}

/**
 * 轮询断言直至通过或 1 秒超时：hub 的连接与扇出发生在后台异步
 * 循环中，测试先等待副作用到达再做最终断言（超时抛出最后一次错误）。
 */
async function waitForAssertion(assertion) {
  const deadline = Date.now() + 1000;
  let lastError;

  while (Date.now() < deadline) {
    try {
      assertion();
      return;
    } catch (error) {
      lastError = error;
      await new Promise((resolve) => setTimeout(resolve, 10));
    }
  }

  throw lastError;
}

// 覆盖全局 hub 的 fanout 容错与 stop/start 续连语义。
describe('createGlobalMessageStreamHub', () => {
  it('continues fanout when an event subscriber throws', async () => {
    const warnSpy = vi.spyOn(console, 'warn').mockImplementation(() => {});
    const received = [];
    const hub = createGlobalMessageStreamHub({
      buildOpenCodeUrl: (pathname) => `http://127.0.0.1:4096${pathname}`,
      getOpenCodeAuthHeaders: () => ({}),
      upstreamReconnectDelayMs: 100,
      fetchImpl: async () => createSseResponse({
        blocks: [
          'id: evt-1\ndata: {"type":"session.updated","properties":{}}\n\n',
        ],
      }),
    });

    hub.subscribeEvent(() => {
      throw new Error('subscriber failed');
    });
    hub.subscribeEvent((event) => {
      received.push(event.eventId);
    });

    try {
      hub.start();
      await waitForAssertion(() => {
        expect(received).toEqual(['evt-1']);
      });
      expect(warnSpy).toHaveBeenCalled();
    } finally {
      hub.stop();
      warnSpy.mockRestore();
    }
  });

  it('continues status fanout when a status subscriber throws', async () => {
    const warnSpy = vi.spyOn(console, 'warn').mockImplementation(() => {});
    const received = [];
    const hub = createGlobalMessageStreamHub({
      buildOpenCodeUrl: (pathname) => `http://127.0.0.1:4096${pathname}`,
      getOpenCodeAuthHeaders: () => ({}),
      upstreamReconnectDelayMs: 100,
      fetchImpl: async () => createSseResponse(),
    });

    hub.subscribeStatus(() => {
      throw new Error('status subscriber failed');
    });
    hub.subscribeStatus((status) => {
      received.push(status.type);
    });

    try {
      hub.start();
      await waitForAssertion(() => {
        expect(received).toContain('connect');
      });
      expect(warnSpy).toHaveBeenCalled();
    } finally {
      hub.stop();
      warnSpy.mockRestore();
    }
  });

  it('stop()/start() resumes upstream from the retained tail instead of re-ingesting the ring', async () => {
    const requestHeaders = [];
    let connectCount = 0;
    const hub = createGlobalMessageStreamHub({
      buildOpenCodeUrl: (pathname) => `http://127.0.0.1:4096${pathname}`,
      getOpenCodeAuthHeaders: () => ({}),
      upstreamReconnectDelayMs: 100,
      fetchImpl: async (_url, init) => {
        requestHeaders.push({ ...(init?.headers ?? {}) });
        connectCount += 1;
        return createSseResponse({
          blocks:
            connectCount === 1
              ? [
                  'id: evt-1\ndata: {"type":"session.updated","properties":{}}\n\n',
                  'id: evt-2\ndata: {"type":"session.updated","properties":{}}\n\n',
                ]
              : [],
          headers: { 'x-omp-epoch': 'boot-1' },
        });
      },
    });

    const received = [];
    hub.subscribeEvent((event) => {
      received.push(event.eventId);
    });

    try {
      hub.start();
      await waitForAssertion(() => {
        expect(hub.tailEventId()).toBe('evt-2');
      });
      hub.stop();
      hub.start();
      await waitForAssertion(() => {
        expect(connectCount).toBeGreaterThanOrEqual(2);
      });
      // The second connect proves a same-boot suffix resume: retained events
      // stay exactly once, and no duplicate burst reaches live subscribers.
      expect(requestHeaders[1]['Last-Event-ID']).toBe('evt-2');
      expect(requestHeaders[1]['x-omp-epoch']).toBe('boot-1');
      expect(hub.getStats().retainedEntries).toBe(2);
      expect(received).toEqual(['evt-1', 'evt-2']);
    } finally {
      hub.stop();
    }
  });

  it('continues fanout when an async event subscriber rejects', async () => {
    const warnSpy = vi.spyOn(console, 'warn').mockImplementation(() => {});
    const received = [];
    const hub = createGlobalMessageStreamHub({
      buildOpenCodeUrl: (pathname) => `http://127.0.0.1:4096${pathname}`,
      getOpenCodeAuthHeaders: () => ({}),
      upstreamReconnectDelayMs: 100,
      fetchImpl: async () => createSseResponse({
        blocks: [
          'id: evt-1\ndata: {"type":"session.updated","properties":{}}\n\n',
        ],
      }),
    });

    hub.subscribeEvent(async () => {
      throw new Error('async subscriber failed');
    });
    hub.subscribeEvent((event) => {
      received.push(event.eventId);
    });

    try {
      hub.start();
      await waitForAssertion(() => {
        expect(received).toEqual(['evt-1']);
      });
      await waitForAssertion(() => {
        expect(warnSpy).toHaveBeenCalled();
      });
    } finally {
      hub.stop();
      warnSpy.mockRestore();
    }
  });
});
