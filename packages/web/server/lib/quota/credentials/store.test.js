/**
 * quota 凭据存储（store.js）测试套件（bun:test）。
 *
 * 通过把 OMPCHAMBER_DATA_DIR 指向临时目录，验证凭据文件的
 * owner-only 权限（目录 0o700 / 文件 0o600）、provider 白名单对
 * 路径穿越的拒绝，以及遗留 opencode-go 凭据的免解析直接删除。
 */

import { afterAll, describe, expect, it } from 'bun:test';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { deleteLegacyOpenCodeGoCredential, deleteQuotaCredential, readQuotaCredential, writeQuotaCredential } from './store.js';

/** 记住测试前的 OMPCHAMBER_DATA_DIR 取值，afterAll 中恢复现场。 */
const previousDataDir = process.env.OMPCHAMBER_DATA_DIR;
/** 每次运行新建的临时数据目录，凭据文件将写入其 quota/ 子目录。 */
const temporaryDirectory = fs.mkdtempSync(path.join(os.tmpdir(), 'ompchamber-quota-store-'));
process.env.OMPCHAMBER_DATA_DIR = temporaryDirectory;

/** 凭据写入的权限约束、读回一致性及白名单外 provider 的拒绝。 */
describe('quota credential store', () => {
  it('uses owner-only permissions and rejects arbitrary provider paths', () => {
    writeQuotaCredential('ollama-cloud', { cookie: 'secret' });
    expect(fs.statSync(path.join(temporaryDirectory, 'quota')).mode & 0o777).toBe(0o700);
    expect(fs.statSync(path.join(temporaryDirectory, 'quota', 'ollama-cloud.json')).mode & 0o777).toBe(0o600);
    expect(readQuotaCredential('ollama-cloud', (value) => value)).toEqual({ cookie: 'secret' });
    expect(() => writeQuotaCredential('../escape', {})).toThrow('Unsupported credential provider');
    deleteQuotaCredential('ollama-cloud');
  });

  it('removes the obsolete OpenCode Go credential without parsing it', () => {
    const legacyPath = path.join(temporaryDirectory, 'quota', 'opencode-go.json');
    fs.mkdirSync(path.dirname(legacyPath), { recursive: true });
    fs.writeFileSync(legacyPath, '{not valid json', { mode: 0o600 });
    deleteLegacyOpenCodeGoCredential();
    expect(fs.existsSync(legacyPath)).toBe(false);
  });
});

// 恢复环境变量并递归清理临时目录。
afterAll(() => {
  if (previousDataDir === undefined) delete process.env.OMPCHAMBER_DATA_DIR;
  else process.env.OMPCHAMBER_DATA_DIR = previousDataDir;
  fs.rmSync(temporaryDirectory, { recursive: true, force: true });
});
