import { describe, expect, it } from 'vitest';
import {
  isLoopbackBindHost,
  isNetworkExposedBindHost,
  isDevelopmentServer,
} from './bind-host.js';

describe('bind host exposure classification', () => {
  it('allows only proven loopback bind hosts without authentication', () => {
    for (const host of ['localhost', '127.0.0.1', '127.25.1.2', '::1', '[::1]', '::ffff:127.0.0.1']) {
      expect(isLoopbackBindHost(host), host).toBe(true);
      expect(isNetworkExposedBindHost(host), host).toBe(false);
    }
  });

  it('treats wildcard, LAN, IPv6 local, and unknown hosts as exposed', () => {
    for (const host of [
      '0.0.0.0',
      '0',
      '0x0',
      '::',
      '[::]',
      '192.168.1.10',
      '10.0.0.5',
      '172.16.0.2',
      '::ffff:192.168.1.10',
      'fe80::1',
      'fc00::1',
      'openchamber.local',
      'example.com',
      '',
    ]) {
      expect(isLoopbackBindHost(host), host).toBe(false);
      expect(isNetworkExposedBindHost(host), host).toBe(true);
    }
  });

  it('recognizes only the explicit development server marker', () => {
    expect(isDevelopmentServer({ OPENCHAMBER_DEV_SERVER: 'true' })).toBe(true);
    expect(isDevelopmentServer({ OPENCHAMBER_DEV_SERVER: 'false' })).toBe(false);
    expect(isDevelopmentServer({ NODE_ENV: 'development' })).toBe(false);
    expect(isDevelopmentServer({})).toBe(false);
  });
});
