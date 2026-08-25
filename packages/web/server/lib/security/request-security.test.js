import { describe, expect, test } from 'bun:test';
import { createRequestSecurityRuntime } from './request-security.js';

const createRuntime = () => createRequestSecurityRuntime({
  readSettingsFromDiskMigrated: async () => ({}),
});

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
});
