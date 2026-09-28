/**
 * behavior（AGENTS.md）端点测试套件：mock 掉 node:fs 与全局 fetch，验证
 * /api/behavior/agents-md 的解析链（omp-host 的 agent-dir 端点 → host
 * 不可达时的 ~/.omp/agent 静态回退、跨请求的 profile 切换、短暂失联不
 * 固化回退）、内容读取、遗留文件的只读上报与 PUT 写入路径。
 */
import { describe, expect, it, vi, afterEach } from 'vitest';
import express from 'express';
import request from 'supertest';
import os from 'node:os';
import path from 'node:path';

// 中文补充：behavior / AGENTS.md 端点（spec 07 §5.13 修订版）——编辑目标
// 是 omp-host 解析出的 omp-native 用户级文件（本 Node 服务无法 import
// SDK），host 不可达时回退静态 ~/.omp/agent。以下测试锁定解析链、遗留
// 文件只读上报与写入路径；routes.js 直接经 node fs 读取，故两者都在此 mock。
// Behavior / AGENTS.md endpoints (spec 07 §5.13 REVISED): the edit target is
// the omp-native user-level file resolved by the omp-host (this Node server
// cannot import the SDK), with a static ~/.omp/agent fallback when the host
// is unreachable. These tests pin the resolution chain, the read-only legacy
// reporting, and the write path. routes.js reads through the node `fs`
// module directly, so both are mocked here.

/** fs.promises.readFile 桩：默认抛 ENOENT（模拟文件不存在）。 */
const readFile = vi.fn(async () => { throw Object.assign(new Error('ENOENT'), { code: 'ENOENT' }); });
/** fs.promises.writeFile 桩。 */
const writeFile = vi.fn(async () => undefined);
/** fs.promises.access 桩。 */
const access = vi.fn(async () => undefined);
/** fs.promises.mkdir 桩。 */
const mkdir = vi.fn(async () => undefined);

// 将整个 fs 模块替换为上面的桩集合。
vi.mock('fs', () => ({
  default: { promises: { readFile, writeFile, access, mkdir } },
}));

/** 被测路由注册器（在 fs mock 生效后加载）。 */
const { registerOpenCodeRoutes } = await import('./routes.js');

/** 构造挂好被测路由的 express app；overrides 可替换任一依赖。 */
const createApp = (overrides = {}) => {
  const app = express();
  app.use(express.json());
  const dependencies = {
    validateDirectoryPath: vi.fn(async (directory) => ({ ok: true, directory })),
    buildOpenCodeUrl: vi.fn(() => 'http://127.0.0.1:3902'),
    getOpenCodeAuthHeaders: vi.fn(() => ({ Authorization: 'Basic dGVzdA==' })),
    ...overrides,
  };
  registerOpenCodeRoutes(app, dependencies);
  return { app, dependencies };
};

/** 构造返回指定 agentDir 的 fetch 桩（模拟 omp-host 的 agent-dir 端点）。 */
const agentDirFetch = (agentDir) => async () => ({
  ok: true,
  json: async () => ({ agentDir }),
});

// 用例收尾：解除全局 fetch 桩并重置 fs 桩的默认行为。
afterEach(() => {
  vi.unstubAllGlobals();
  readFile.mockReset();
  writeFile.mockReset();
  readFile.mockImplementation(async () => { throw Object.assign(new Error('ENOENT'), { code: 'ENOENT' }); });
});

// AGENTS.md 端点主套件：omp-native 目标解析、回退与读写路径。
describe('behavior AGENTS.md endpoints (omp-native target, 07 §5.13)', () => {
  it('resolves the native path from the omp-host agent-dir endpoint', async () => {
    const nativeDir = path.join(os.homedir(), '.omp', 'profiles', 'night', 'agent');
    vi.stubGlobal('fetch', vi.fn(agentDirFetch(nativeDir)));
    const { app } = createApp();

    const response = await request(app).get('/api/behavior/agents-md').expect(200);

    expect(response.body.path).toBe(path.join(nativeDir, 'AGENTS.md'));
    expect(response.body.exists).toBe(false);
    expect(response.body.legacy.path).toBe(path.join(os.homedir(), '.config', 'opencode', 'AGENTS.md'));
  });

  it('falls back to ~/.omp/agent when the omp-host is unreachable', async () => {
    vi.stubGlobal('fetch', vi.fn(async () => { throw new Error('ECONNREFUSED'); }));
    const { app } = createApp();

    const response = await request(app).get('/api/behavior/agents-md').expect(200);

    expect(response.body.path).toBe(path.join(os.homedir(), '.omp', 'agent', 'AGENTS.md'));
  });

  it('honors agent-dir changes across requests (profile switch)', async () => {
    const dayDir = path.join(os.homedir(), '.omp', 'profiles', 'day', 'agent');
    const nightDir = path.join(os.homedir(), '.omp', 'profiles', 'night', 'agent');
    const dirs = [dayDir, nightDir];
    let call = 0;
    vi.stubGlobal('fetch', vi.fn(async () => ({
      ok: true,
      json: async () => ({ agentDir: dirs[Math.min(call++, 1)] }),
    })));
    const { app } = createApp();

    const first = await request(app).get('/api/behavior/agents-md').expect(200);
    const second = await request(app).get('/api/behavior/agents-md').expect(200);

    expect(first.body.path).toBe(path.join(dayDir, 'AGENTS.md'));
    expect(second.body.path).toBe(path.join(nightDir, 'AGENTS.md'));
  });

  it('does not pin the fallback when the omp-host is briefly unreachable', async () => {
    const nativeDir = path.join(os.homedir(), '.omp', 'profiles', 'day', 'agent');
    let fail = true;
    vi.stubGlobal('fetch', vi.fn(async () => {
      if (fail) {
        fail = false;
        throw new Error('ECONNREFUSED');
      }
      return { ok: true, json: async () => ({ agentDir: nativeDir }) };
    }));
    const { app } = createApp();

    const fallback = await request(app).get('/api/behavior/agents-md').expect(200);
    const recovered = await request(app).get('/api/behavior/agents-md').expect(200);

    expect(fallback.body.path).toBe(path.join(os.homedir(), '.omp', 'agent', 'AGENTS.md'));
    expect(recovered.body.path).toBe(path.join(nativeDir, 'AGENTS.md'));
  });

  it('serves existing native content, reports legacy read-only, and writes to the native file', async () => {
    const nativeDir = path.join(os.homedir(), '.omp', 'agent');
    vi.stubGlobal('fetch', vi.fn(agentDirFetch(nativeDir)));
    readFile.mockImplementation(async (file) => (String(file).includes('.config') ? 'legacy body' : 'native body'));
    const { app } = createApp();

    const response = await request(app).get('/api/behavior/agents-md').expect(200);

    expect(response.body.content).toBe('native body');
    expect(response.body.exists).toBe(true);
    expect(response.body.legacy.hasContent).toBe(true);

    const put = await request(app).put('/api/behavior/agents-md').send({ content: 'updated' }).expect(200);
    expect(put.body).toHaveProperty('success');
    expect(writeFile).toHaveBeenCalledWith(
      path.join(nativeDir, 'AGENTS.md'),
      'updated',
      'utf8',
    );
  });
});
