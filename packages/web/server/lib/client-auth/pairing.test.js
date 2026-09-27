/**
 * 客户端配对运行时（pairing.js）的行为测试套件。
 *
 * 覆盖配对会话的创建与一次性兑换、客户端元数据透传、过期/取消/密钥
 * 错误/种类不允许的拒绝、签发失败不消费会话，以及下次创建时清扫
 * 过期未用会话。
 */
import { describe, it, expect, vi } from 'vitest';
import fs from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import crypto from 'node:crypto';

import { createClientPairingRuntime } from './pairing.js';

/** 构造配对运行时：临时目录存储 + 可注入的远程客户端认证运行时（默认用 mock 记录每次签发的客户端）。 */
const makeRuntime = async (options = {}) => {
  const dir = await fs.mkdtemp(path.join(os.tmpdir(), 'ompchamber-pairing-test-'));
  const createdClients = [];
  const remoteClientAuthRuntime = options.remoteClientAuthRuntime || {
    // 默认的 createClient mock：返回递增的 client-N / token-N，并记录到 createdClients 供断言。
    createClient: vi.fn(async (input) => {
      const client = {
        id: `client-${createdClients.length + 1}`,
        label: input.label ?? input.fallbackLabel,
        clientKind: input.clientKind,
        authMethod: input.authMethod,
        pairingId: input.pairingId,
        deviceName: input.deviceName ?? null,
      };
      createdClients.push(client);
      return { client, token: `token-${createdClients.length}` };
    }),
  };
  const runtime = createClientPairingRuntime({
    fsPromises: fs,
    path,
    crypto,
    storePath: path.join(dir, 'pairing.json'),
    remoteClientAuthRuntime,
    ttlMs: options.ttlMs ?? 10 * 60 * 1000,
  });
  return { dir, runtime, remoteClientAuthRuntime, createdClients };
};

/** 配对会话生命周期：一次性兑换与元数据透传、各类拒绝路径、失败不消费、过期清扫。 */
describe('client auth pairing runtime', () => {
  it('redeems a pairing session once and propagates client metadata', async () => {
    const { runtime, remoteClientAuthRuntime } = await makeRuntime();
    const created = await runtime.createPairingSession({ allowedClientKinds: ['mobile'] });

    const result = await runtime.redeemPairingSession({
      pairingId: created.pairing.id,
      secret: created.pairing.secret,
      clientLabel: 'Iryna iPhone',
      clientKind: 'mobile',
      deviceName: 'Iryna iPhone',
      dedupeKey: 'device-key',
    });

    expect(result.token).toBe('token-1');
    expect(result.client).toMatchObject({
      label: 'Iryna iPhone',
      clientKind: 'mobile',
      authMethod: 'pairing',
      pairingId: created.pairing.id,
      deviceName: 'Iryna iPhone',
    });
    expect(remoteClientAuthRuntime.createClient).toHaveBeenCalledWith(expect.objectContaining({
      authMethod: 'pairing',
      pairingId: created.pairing.id,
      clientKind: 'mobile',
      dedupeKey: 'device-key',
      // No operator-typed pairing label: the app-reported name is only a
      // fallback so a re-pair keeps the existing device record's label.
      label: null,
      fallbackLabel: 'Iryna iPhone',
    }));

    await expect(runtime.redeemPairingSession({
      pairingId: created.pairing.id,
      secret: created.pairing.secret,
      clientKind: 'mobile',
    })).rejects.toThrow('Invalid or expired pairing session');
  });

  it('rejects expired, cancelled, wrong-secret, and disallowed-kind redemption', async () => {
    const { runtime: expiredRuntime } = await makeRuntime({ ttlMs: -1000 });
    const expired = await expiredRuntime.createPairingSession();
    await expect(expiredRuntime.redeemPairingSession({
      pairingId: expired.pairing.id,
      secret: expired.pairing.secret,
      clientKind: 'mobile',
    })).rejects.toThrow('Invalid or expired pairing session');

    const { runtime } = await makeRuntime();
    const cancelled = await runtime.createPairingSession();
    await runtime.cancelPairingSession(cancelled.pairing.id);
    await expect(runtime.redeemPairingSession({
      pairingId: cancelled.pairing.id,
      secret: cancelled.pairing.secret,
      clientKind: 'mobile',
    })).rejects.toThrow('Invalid or expired pairing session');

    const wrongSecret = await runtime.createPairingSession();
    await expect(runtime.redeemPairingSession({
      pairingId: wrongSecret.pairing.id,
      secret: 'wrong',
      clientKind: 'mobile',
    })).rejects.toThrow('Invalid or expired pairing session');

    const desktopOnly = await runtime.createPairingSession({ allowedClientKinds: ['desktop'] });
    await expect(runtime.redeemPairingSession({
      pairingId: desktopOnly.pairing.id,
      secret: desktopOnly.pairing.secret,
      clientKind: 'mobile',
    })).rejects.toThrow('Invalid or expired pairing session');
  });

  it('does not consume the pairing session if client issuance fails', async () => {
    const createClient = vi.fn()
      .mockRejectedValueOnce(new Error('disk failed'))
      .mockResolvedValueOnce({ client: { id: 'client-1' }, token: 'token-1' });
    const { runtime } = await makeRuntime({ remoteClientAuthRuntime: { createClient } });
    const created = await runtime.createPairingSession();

    await expect(runtime.redeemPairingSession({
      pairingId: created.pairing.id,
      secret: created.pairing.secret,
      clientKind: 'mobile',
    })).rejects.toThrow('disk failed');

    await expect(runtime.redeemPairingSession({
      pairingId: created.pairing.id,
      secret: created.pairing.secret,
      clientKind: 'mobile',
    })).resolves.toMatchObject({ token: 'token-1' });
    expect(createClient).toHaveBeenLastCalledWith(expect.objectContaining({
      dedupeKey: `pairing:${created.pairing.id}`,
    }));
  });

  it('sweeps expired never-used sessions from the store on the next create', async () => {
    const { dir, runtime } = await makeRuntime({ ttlMs: -1000 });
    // Immediately expired (negative TTL), never used or cancelled.
    const expired = await runtime.createPairingSession({ label: 'stale' });

    // The next create sweeps the store; only the fresh session should remain.
    const storePath = path.join(dir, 'pairing.json');
    await runtime.createPairingSession({ label: 'fresh' });
    const store = JSON.parse(await fs.readFile(storePath, 'utf8'));
    const ids = store.sessions.map((session) => session.id);
    expect(ids).not.toContain(expired.pairing.id);
    expect(ids).toHaveLength(1);
  });
});
