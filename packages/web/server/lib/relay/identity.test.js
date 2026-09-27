/**
 * 【测试套件】relay 身份运行时（identity.js）：serverId 从签名公钥稳定派生、
 * 签名/加密两把密钥对的持久化、已有签名密钥的复用（serverId 跨安装稳定），
 * 以及 signRelayAuth 产出的 ECDSA 签名可被公钥验证。
 */
import { describe, expect, it } from 'bun:test';
import crypto from 'node:crypto';

import { createRelayIdentityRuntime } from './identity.js';
import { canonicalPublicJwkString } from './signing-key.js';

// In-memory settings store standing in for the on-disk settings file.
// 内存版 settings 存储器，替代磁盘 settings 文件（含 peek 供断言读取当前值）。
const makeSettingsStore = (initial = {}) => {
  let settings = { ...initial };
  return {
    readSettingsFromDiskMigrated: async () => ({ ...settings }),
    writeSettingsToDisk: async (next) => {
      settings = { ...next };
    },
    peek: () => settings,
  };
};

// 主 describe：身份派生、密钥持久化与复用、鉴权签名可验证性。
describe('relay identity', () => {
  it('derives a stable serverId from the signing key and persists both keypairs', async () => {
    const store = makeSettingsStore();
    const runtime = createRelayIdentityRuntime({ crypto, ...store });
    const identity = await runtime.getRelayIdentity();

    const stored = store.peek();
    expect(stored.relaySigningKey).toBeDefined();
    expect(stored.relayEncryptionKey).toBeDefined();

    const expectedServerId = crypto
      .createHash('sha256')
      .update(canonicalPublicJwkString(stored.relaySigningKey.publicJwk))
      .digest('base64url');
    expect(identity.serverId).toBe(expectedServerId);
    expect(identity.hostEncPubJwk.crv).toBe('P-256');
  });

  it('reuses an existing signing key (serverId stays stable across installs)', async () => {
    const { privateKey, publicKey } = crypto.generateKeyPairSync('ec', { namedCurve: 'P-256' });
    void privateKey;
    const publicJwk = publicKey.export({ format: 'jwk' });
    const store = makeSettingsStore({
      relaySigningKey: {
        privateJwk: crypto.generateKeyPairSync('ec', { namedCurve: 'P-256' }).privateKey.export({ format: 'jwk' }),
        publicJwk,
      },
    });
    // Match private to public so importing works.
    const pair = crypto.generateKeyPairSync('ec', { namedCurve: 'P-256' });
    store.peek().relaySigningKey.privateJwk = pair.privateKey.export({ format: 'jwk' });
    store.peek().relaySigningKey.publicJwk = pair.publicKey.export({ format: 'jwk' });

    const runtime = createRelayIdentityRuntime({ crypto, ...store });
    const identity = await runtime.getRelayIdentity();
    const expected = crypto
      .createHash('sha256')
      .update(canonicalPublicJwkString(pair.publicKey.export({ format: 'jwk' })))
      .digest('base64url');
    expect(identity.serverId).toBe(expected);
  });

  it('produces a verifiable relay auth signature', async () => {
    const store = makeSettingsStore();
    const runtime = createRelayIdentityRuntime({ crypto, ...store });
    const identity = await runtime.getRelayIdentity();
    const { ts, sig, pk } = identity.signRelayAuth('host-control', null);

    const canonical = Buffer.from(pk, 'base64url').toString('utf8');
    const publicJwk = JSON.parse(canonical);
    const key = crypto.createPublicKey({ key: publicJwk, format: 'jwk' });
    const ok = crypto.verify(
      'SHA256',
      Buffer.from(`${ts}.${identity.serverId}.host-control.`),
      { key, dsaEncoding: 'ieee-p1363' },
      Buffer.from(sig, 'base64url'),
    );
    expect(ok).toBe(true);
  });
});
