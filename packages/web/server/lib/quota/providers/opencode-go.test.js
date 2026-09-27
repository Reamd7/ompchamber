/**
 * OpenCode Go 配额 provider 测试套件。
 *
 * 在独立的临时数据目录（OMPCHAMBER_DATA_DIR）中运行，验证用量载荷的
 * 窗口解析、认证失败错误不泄露密钥、请求头使用 bearer 认证，以及
 * 从 auth 文件读取 key 时会顺带删除遗留的托管凭据文件。
 */
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { afterAll, afterEach, describe, expect, it, vi } from 'vitest';

/** 记录测试前 OMPCHAMBER_DATA_DIR 的原值，afterAll 中恢复。 */
const previousDataDirectory = process.env.OMPCHAMBER_DATA_DIR;
/** 独立的临时数据目录，隔离托管凭据存储，避免污染真实数据目录。 */
const temporaryDataDirectory = fs.mkdtempSync(path.join(os.tmpdir(), 'ompchamber-opencode-go-'));
process.env.OMPCHAMBER_DATA_DIR = temporaryDataDirectory;

vi.mock('../../opencode/auth.js', () => ({
  readAuthFile: () => ({ 'opencode-go': { key: 'test-key' } }),
}));

import { fetchOpenCodeGoUsage, fetchQuota, parseOpenCodeGoUsage } from './opencode-go.js';

afterEach(() => {
  vi.unstubAllGlobals();
});

afterAll(() => {
  if (previousDataDirectory === undefined) delete process.env.OMPCHAMBER_DATA_DIR;
  else process.env.OMPCHAMBER_DATA_DIR = previousDataDirectory;
  fs.rmSync(temporaryDataDirectory, { recursive: true, force: true });
});

/** OpenCode Go provider 的载荷解析、认证与遗留凭据清理行为。 */
describe('OpenCode Go quota provider', () => {
  it('parses partial API usage windows', () => {
    const windows = parseOpenCodeGoUsage({ usage: { rolling: { percent: 25, resetsAt: '2026-08-12T12:00:00.000Z' }, weekly: { percent: 40, resetsAt: '2026-08-19T12:00:00.000Z' } } });
    expect(windows['5h'].usedPercent).toBe(25);
    expect(windows['5h'].resetAt).toBe('2026-08-12T12:00:00.000Z');
    expect(windows.weekly.usedPercent).toBe(40);
    expect(windows.monthly).toBeUndefined();
  });

  it('does not expose credentials in authentication errors', async () => {
    await expect(fetchOpenCodeGoUsage('secret', async () => new Response('', { status: 403 }))).rejects.toThrow('authentication failed');
  });

  it('uses the Go usage API with bearer authentication', async () => {
    let request;
    const usage = await fetchOpenCodeGoUsage('secret', async (url, options) => {
      request = { url, options };
      return new Response(JSON.stringify({ usage: { rolling: { percent: 25, resetsAt: '2026-08-12T12:00:00.000Z' } } }));
    });
    expect(request.url).toBe('https://opencode.ai/zen/go/v1/usage');
    expect(request.options.headers).toMatchObject({ Accept: 'application/json', Authorization: 'Bearer secret' });
    expect(request.options.headers.Cookie).toBeUndefined();
    expect(usage['5h'].usedPercent).toBe(25);
  });

  it('reads the API key from the OpenCode auth file', async () => {
    const legacyPath = path.join(temporaryDataDirectory, 'quota', 'opencode-go.json');
    fs.mkdirSync(path.dirname(legacyPath), { recursive: true });
    fs.writeFileSync(legacyPath, '{not valid json', { mode: 0o600 });
    const fetchMock = vi.fn().mockResolvedValue(new Response(JSON.stringify({ usage: { rolling: { percent: 25, resetsAt: '2026-08-12T12:00:00.000Z' } } })));
    vi.stubGlobal('fetch', fetchMock);
    const result = await fetchQuota();
    expect(result).toMatchObject({ providerId: 'opencode-go', ok: true, configured: true });
    expect(fetchMock.mock.calls[0][1].headers.Authorization).toBe('Bearer test-key');
    expect(fs.existsSync(legacyPath)).toBe(false);
  });
});
