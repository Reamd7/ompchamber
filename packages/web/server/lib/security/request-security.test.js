/**
 * 请求安全运行时测试套件（bun:test）：验证打包客户端 Origin 放行、未知
 * Origin 拒绝、WebSocket 升级拒绝时先完整写出 HTTP 错误响应再销毁 socket
 * （以及对已销毁 socket 的无操作），以及反向代理场景下对外部 host 的
 * 信任边界（x-forwarded-host / x-forwarded-proto）。
 */
import { describe, expect, test } from 'bun:test';
import { createRequestSecurityRuntime } from './request-security.js';

/** 构造带空设置读取器（无 publicOrigin）的请求安全运行时。 */
const createRuntime = () => createRequestSecurityRuntime({
  readSettingsFromDiskMigrated: async () => ({}),
});

/** 同源放行判定与 WebSocket 升级拒绝行为验证。 */
describe('request security runtime', () => {
  test('allows packaged client origins for remote client transports', async () => {
    const runtime = createRuntime();

    await expect(runtime.isRequestOriginAllowed({
      headers: {
        origin: 'ompchamber-ui://app',
        host: '192.168.1.130:1202',
      },
      socket: {},
    })).resolves.toBe(true);

    await expect(runtime.isRequestOriginAllowed({
      headers: {
        origin: 'capacitor://localhost',
        host: '192.168.1.130:1202',
      },
      socket: {},
    })).resolves.toBe(true);

    // Android Capacitor WebView (androidScheme 'https') reports this origin.
    await expect(runtime.isRequestOriginAllowed({
      headers: {
        origin: 'https://localhost',
        host: '192.168.1.130:1202',
      },
      socket: {},
    })).resolves.toBe(true);
  });

  test('rejects unknown origins', async () => {
    const runtime = createRuntime();

    await expect(runtime.isRequestOriginAllowed({
      headers: {
        origin: 'https://evil.example.com',
        host: '192.168.1.130:1202',
      },
      socket: {},
    })).resolves.toBe(false);
  });

  test('rejectWebSocketUpgrade delivers the HTTP error before destroying the socket', () => {
    const runtime = createRuntime();
    const writes = [];
    let destroyed = false;
    let flushed = false;
    const socket = {
      get destroyed() {
        return destroyed;
      },
      once(event, handler) {
        expect(event).toBe('error');
        return this;
      },
      end(data, callback) {
        writes.push(data);
        flushed = true;
        callback?.();
        return this;
      },
      destroy() {
        destroyed = true;
      },
    };

    runtime.rejectWebSocketUpgrade(socket, 401, 'UI authentication required');

    expect(flushed).toBe(true);
    expect(destroyed).toBe(true);
    expect(writes).toHaveLength(1);
    expect(writes[0].startsWith('HTTP/1.1 401 Unauthorized\r\n')).toBe(true);
    expect(writes[0].endsWith('UI authentication required')).toBe(true);
    expect(writes[0]).toContain('Content-Length: 26\r\n');
  });

  test('rejectWebSocketUpgrade is a no-op for an already destroyed socket', () => {
    const runtime = createRuntime();
    const socket = {
      destroyed: true,
      once() {
        throw new Error('must not attach listeners');
      },
      end() {
        throw new Error('must not write');
      },
      destroy() {
        throw new Error('must not destroy');
      },
    };

    runtime.rejectWebSocketUpgrade(socket, 403, 'Invalid origin');
  });
  test('allows the external host when TLS terminates before an HTTP proxy hop', async () => {
    const runtime = createRuntime();

    await expect(runtime.isRequestOriginAllowed({
      headers: {
        origin: 'https://devchamber.example.com',
        host: 'devchamber.example.com',
        'x-forwarded-proto': 'http',
      },
      socket: {},
    })).resolves.toBe(true);
  });

  test('uses the forwarded external host without trusting a different origin', async () => {
    const runtime = createRuntime();
    const request = {
      headers: {
        host: '127.0.0.1:3000',
        'x-forwarded-host': 'devchamber.example.com',
        'x-forwarded-proto': 'http',
      },
      socket: {},
    };

    await expect(runtime.isRequestOriginAllowed({
      ...request,
      headers: { ...request.headers, origin: 'https://devchamber.example.com' },
    })).resolves.toBe(true);
    await expect(runtime.isRequestOriginAllowed({
      ...request,
      headers: { ...request.headers, origin: 'https://evil.example.com' },
    })).resolves.toBe(false);
  });
});
