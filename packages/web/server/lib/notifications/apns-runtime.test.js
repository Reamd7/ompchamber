/**
 * APNs 运行时（apns-runtime.js）测试套件。
 *
 * 分两组：relay 模式（默认）——签名注册 token、签名发送通用文案、
 * 死 token 清理、密钥对复用与按注册环境路由；直连模式（禁用 relay）——
 * ES256 JWT 签名与 HTTP/2 分环境发送。签名均以与 relay 一致的
 * crypto.subtle 验签逻辑复核，证明服务端产出的签名真实有效。
 */
import crypto from 'node:crypto';
import { afterEach, describe, expect, it, vi } from 'vitest';

import { createApnsRuntime } from './apns-runtime.js';

// A real P-256 key so the ES256 signing path (direct mode) runs for real.
/** 生成真实 P-256 密钥并导出 PKCS8 PEM，让直连模式的 ES256 签名路径真实执行。 */
const { privateKey } = crypto.generateKeyPairSync('ec', { namedCurve: 'P-256' });
/** 导出的 PKCS8 PEM 私钥，作为直连签名的 p8 材料。 */
const P8 = privateKey.export({ type: 'pkcs8', format: 'pem' }).toString();
/** 直连模式的完整 APNs 配置（沙盒环境），供建桩与断言使用。 */
const APNS_CONFIG = { keyId: 'KEY123', teamId: 'TEAM123', p8: P8, bundleId: 'com.openchamber.app', environment: 'sandbox' };

// In-memory fs so add-then-read reflects within a test.
/** 创建内存版 fsPromises 桩：writeFile 保存内容、readFile 读回，未写过则抛 ENOENT。 */
const createMemoryFs = () => {
  let content = null;
  return {
    mkdir: vi.fn(async () => {}),
    readFile: vi.fn(async () => {
      if (content == null) {
        const err = new Error('ENOENT');
        err.code = 'ENOENT';
        throw err;
      }
      return content;
    }),
    writeFile: vi.fn(async (_path, data) => {
      content = data;
    }),
  };
};

/**
 * 构造 createApnsRuntime 的依赖集合：内存 fs、真实 crypto、
 * relay 模式下不允许触达的 http2 桩（connect 即抛错），
 * 以及有状态的设置读写（自动生成的 relay 密钥对得以持久化并读回）。
 * overrides 可按用例覆盖任意依赖。
 */
const makeDeps = (overrides = {}) => {
  // Stateful settings so the auto-generated relay signing keypair persists + reads back.
  let settings = {};
  return {
    fsPromises: createMemoryFs(),
    path: { dirname: () => '/tmp' },
    crypto,
    http2: { connect: vi.fn(() => { throw new Error('http2 must not be used in relay mode'); }) },
    APNS_TOKENS_FILE_PATH: '/tmp/apns-tokens.json',
    readSettingsFromDiskMigrated: vi.fn(async () => settings),
    writeSettingsToDisk: vi.fn(async (next) => { settings = next; }),
    ...overrides,
  };
};

/** 构造 JSON Response（默认 200），模拟 relay 的 HTTP 应答。 */
const jsonResponse = (data, status = 200) =>
  new Response(JSON.stringify(data), { status, headers: { 'content-type': 'application/json' } });

// Mirror of the relay's verifier (crypto.subtle), to prove the server's signatures are valid.
/** 复刻 relay 端的验签实现（crypto.subtle ECDSA P-256），验证服务端签名有效。 */
const verifyRelaySignature = async (publicKeyJwk, message, sigB64Url) => {
  const key = await crypto.subtle.importKey(
    'jwk',
    { kty: publicKeyJwk.kty, crv: publicKeyJwk.crv, x: publicKeyJwk.x, y: publicKeyJwk.y },
    { name: 'ECDSA', namedCurve: 'P-256' },
    false,
    ['verify'],
  );
  return crypto.subtle.verify(
    { name: 'ECDSA', hash: 'SHA-256' },
    key,
    new Uint8Array(Buffer.from(sigB64Url, 'base64url')),
    new TextEncoder().encode(message),
  );
};

/** 判断某次 fetch 调用是否为 relay 的 register-token 请求。 */
const isRegister = ([url]) => String(url).endsWith('/register-token');
/** 判断某次 fetch 调用是否为 relay 的 send 请求（固定测试 relay 地址）。 */
const isSend = ([url]) => String(url) === 'https://relay.test/v1/push/send';

/** 恢复 stub 与相关环境变量，避免用例间状态泄漏。 */
afterEach(() => {
  vi.unstubAllGlobals();
  delete process.env.OMPCHAMBER_PUSH_RELAY_URL;
  delete process.env.OMPCHAMBER_PUSH_RELAY_DISABLED;
  delete process.env.OMPCHAMBER_APNS_ENVIRONMENT;
});

/** relay 模式（默认投递路径）的注册、发送、死 token 清理与环境路由行为。 */
describe('apns runtime relay mode (default)', () => {
  it('registers tokens (signed) and posts signed generic text, dropping dead tokens', async () => {
    const fetchMock = vi.fn(async (url) =>
      isRegister([url])
        ? jsonResponse({ ok: true })
        : jsonResponse({
            results: [
              { token: 'tokenA', ok: true, drop: false },
              { token: 'tokenDead', ok: false, drop: true },
            ],
          }),
    );
    vi.stubGlobal('fetch', fetchMock);
    process.env.OMPCHAMBER_PUSH_RELAY_URL = 'https://relay.test/v1/push/send';

    const runtime = createApnsRuntime(makeDeps());
    await runtime.addOrUpdateApnsToken('s1', 'tokenA');
    await runtime.addOrUpdateApnsToken('s2', 'tokenDead');

    // Each new token is bound on the relay with a signed register-token call.
    const registerCalls = fetchMock.mock.calls.filter(isRegister);
    expect(registerCalls).toHaveLength(2);
    for (const [url, init] of registerCalls) {
      expect(url).toBe('https://relay.test/v1/push/register-token');
      const body = JSON.parse(init.body);
      expect(body.publicKeyJwk).toMatchObject({ kty: 'EC', crv: 'P-256' });
      expect(typeof body.ts).toBe('number');
      expect(body.platform).toBe('ios');
      expect(await verifyRelaySignature(body.publicKeyJwk, `${body.ts}.${body.token}.${body.platform}`, body.sig)).toBe(true);
    }

    fetchMock.mockClear();
    await runtime.sendApnsToAllUiSessions(
      { title: 'Agent response is ready', body: 'My session', badge: 3, tag: 'ready-x', data: { sessionId: 'sess1' } },
      {},
    );

    const sendCall = fetchMock.mock.calls.find(isSend);
    expect(sendCall).toBeTruthy();
    const sent = JSON.parse(sendCall[1].body);
    expect(sendCall[1].headers.authorization).toBeUndefined();
    expect(new Set(sent.tokens)).toEqual(new Set(['tokenA', 'tokenDead']));
    expect(sent.title).toBe('Agent response is ready');
    expect(sent.body).toBe('My session');
    expect(sent.badge).toBe(3);
    expect(sent.env).toBe('production');
    expect(sent.data).toEqual({ sessionId: 'sess1' });
    expect(sent.publicKeyJwk).toMatchObject({ kty: 'EC', crv: 'P-256' });
    const sendMessage = `${sent.ts}.${[...sent.tokens].sort().join(',')}.${sent.title}`;
    expect(await verifyRelaySignature(sent.publicKeyJwk, sendMessage, sent.sig)).toBe(true);

    // tokenDead should have been dropped → next send targets only tokenA.
    fetchMock.mockClear();
    await runtime.sendApnsToAllUiSessions({ title: 'x', body: 'y', tag: 't' }, {});
    expect(JSON.parse(fetchMock.mock.calls.find(isSend)[1].body).tokens).toEqual(['tokenA']);
  });

  it('reuses one persisted keypair (same serverId) across register + send', async () => {
    const fetchMock = vi.fn(async () => jsonResponse({ ok: true, results: [] }));
    vi.stubGlobal('fetch', fetchMock);
    process.env.OMPCHAMBER_PUSH_RELAY_URL = 'https://relay.test/v1/push/send';

    const deps = makeDeps();
    const runtime = createApnsRuntime(deps);
    await runtime.addOrUpdateApnsToken('s1', 'tokenA');
    await runtime.sendApnsToAllUiSessions({ title: 't', body: 'b', tag: 'x' }, {});

    const keys = fetchMock.mock.calls.map(([, init]) => JSON.parse(init.body).publicKeyJwk);
    expect(keys.length).toBeGreaterThanOrEqual(2);
    expect(keys.every((k) => k.x === keys[0].x && k.y === keys[0].y)).toBe(true);
    // Keypair was generated + persisted exactly once.
    expect(deps.writeSettingsToDisk).toHaveBeenCalledTimes(1);
  });

  it('honors an explicit sandbox environment override for every token', async () => {
    const fetchMock = vi.fn(async () => jsonResponse({ ok: true, results: [] }));
    vi.stubGlobal('fetch', fetchMock);
    process.env.OMPCHAMBER_PUSH_RELAY_URL = 'https://relay.test/v1/push/send';
    process.env.OMPCHAMBER_APNS_ENVIRONMENT = 'sandbox';

    const runtime = createApnsRuntime(makeDeps());
    await runtime.addOrUpdateApnsToken('s1', 'tokenA', undefined, 'ios', 'production');
    await runtime.sendApnsToAllUiSessions({ title: 't', body: 'b' });

    const sent = JSON.parse(fetchMock.mock.calls.find(isSend)[1].body);
    expect(sent.env).toBe('sandbox');
  });

  it('routes each token to its registered environment (dev build sandbox, release production)', async () => {
    const fetchMock = vi.fn(async () => jsonResponse({ ok: true, results: [] }));
    vi.stubGlobal('fetch', fetchMock);
    process.env.OMPCHAMBER_PUSH_RELAY_URL = 'https://relay.test/v1/push/send';

    const runtime = createApnsRuntime(makeDeps());
    await runtime.addOrUpdateApnsToken('s1', 'tokenXcode', undefined, 'ios', 'sandbox');
    await runtime.addOrUpdateApnsToken('s2', 'tokenStore', undefined, 'ios', 'production');
    await runtime.addOrUpdateApnsToken('s3', 'tokenLegacy'); // no environment → production

    fetchMock.mockClear();
    await runtime.sendApnsToAllUiSessions({ title: 't', body: 'b' });

    const sends = fetchMock.mock.calls.filter(isSend).map(([, init]) => JSON.parse(init.body));
    expect(sends).toHaveLength(2);
    const byEnv = Object.fromEntries(sends.map((s) => [s.env, new Set(s.tokens)]));
    expect(byEnv.sandbox).toEqual(new Set(['tokenXcode']));
    expect(byEnv.production).toEqual(new Set(['tokenStore', 'tokenLegacy']));
  });

  it('no-ops (no relay call) when no tokens are registered', async () => {
    const fetchMock = vi.fn();
    vi.stubGlobal('fetch', fetchMock);
    const runtime = createApnsRuntime(makeDeps());
    await runtime.sendApnsToAllUiSessions({ title: 't', body: 'b' });
    expect(fetchMock).not.toHaveBeenCalled();
  });
});

/** 直连模式（OMPCHAMBER_PUSH_RELAY_DISABLED=true 时的兜底路径）的 JWT 签名与 HTTP/2 发送行为。 */
describe('apns runtime direct fallback (relay disabled)', () => {
  it('leaves direct APNs environment unset without an explicit override (per-token routing)', async () => {
    const { environment: _environment, ...configWithoutEnvironment } = APNS_CONFIG;
    const runtime = createApnsRuntime(
      makeDeps({ readSettingsFromDiskMigrated: vi.fn(async () => ({ apnsConfig: configWithoutEnvironment })) }),
    );

    await expect(runtime.resolveApnsConfig()).resolves.toMatchObject({ environment: null });
  });

  it('sends each token to the APNs host of its registered environment', async () => {
    process.env.OMPCHAMBER_PUSH_RELAY_DISABLED = 'true';
    const { environment: _environment, ...configWithoutEnvironment } = APNS_CONFIG;
    const hosts = [];
    const http2 = {
      connect: (host) => {
        const targeted = [];
        hosts.push({ host, targeted });
        return {
          on: () => {},
          close: () => {},
          request: (headers) => {
            targeted.push(String(headers[':path']).replace('/3/device/', ''));
            const listeners = {};
            const req = {
              on: (event, cb) => { listeners[event] = cb; return req; },
              setEncoding: () => req,
              end: () => {
                queueMicrotask(() => {
                  listeners.response?.({ ':status': '200' });
                  listeners.end?.();
                });
              },
            };
            return req;
          },
        };
      },
    };
    const runtime = createApnsRuntime(
      makeDeps({ http2, readSettingsFromDiskMigrated: vi.fn(async () => ({ apnsConfig: configWithoutEnvironment })) }),
    );
    await runtime.addOrUpdateApnsToken('s1', 'tokenXcode', undefined, 'ios', 'sandbox');
    await runtime.addOrUpdateApnsToken('s2', 'tokenStore', undefined, 'ios', 'production');
    await runtime.sendApnsToAllUiSessions({ title: 't', body: 'b', tag: 'ready-x' });

    const byHost = Object.fromEntries(hosts.map(({ host, targeted }) => [host, targeted]));
    expect(byHost['https://api.sandbox.push.apple.com']).toEqual(['tokenXcode']);
    expect(byHost['https://api.push.apple.com']).toEqual(['tokenStore']);
  });

  it('signs an ES256 JWT and sends over http2 when relay is disabled', async () => {
    process.env.OMPCHAMBER_PUSH_RELAY_DISABLED = 'true';
    const targeted = [];
    const http2 = {
      connect: () => ({
        on: () => {},
        close: () => {},
        request: (headers) => {
          targeted.push(String(headers[':path']).replace('/3/device/', ''));
          const listeners = {};
          const req = {
            on: (event, cb) => { listeners[event] = cb; return req; },
            setEncoding: () => req,
            end: () => {
              queueMicrotask(() => {
                listeners.response?.({ ':status': '200' });
                listeners.end?.();
              });
            },
          };
          return req;
        },
      }),
    };
    const runtime = createApnsRuntime(
      makeDeps({ http2, readSettingsFromDiskMigrated: vi.fn(async () => ({ apnsConfig: APNS_CONFIG })) }),
    );
    await runtime.addOrUpdateApnsToken('s', 'tokenDirect');
    await runtime.sendApnsToAllUiSessions({ title: 't', body: 'b', tag: 'ready-x' });
    expect(targeted).toEqual(['tokenDirect']);
  });

  it('signApnsJwt produces a 3-part ES256 token with the expected header/claims', () => {
    const runtime = createApnsRuntime(makeDeps());
    const parts = runtime.signApnsJwt(APNS_CONFIG).split('.');
    expect(parts).toHaveLength(3);
    expect(JSON.parse(Buffer.from(parts[0], 'base64url').toString())).toEqual({ alg: 'ES256', kid: 'KEY123' });
    expect(JSON.parse(Buffer.from(parts[1], 'base64url').toString()).iss).toBe('TEAM123');
  });
});
