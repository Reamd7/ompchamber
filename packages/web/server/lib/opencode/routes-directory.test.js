/**
 * OpenCode 项目目录路由（POST /api/opencode/directory）的测试。
 *
 * 覆盖：带 create: true 时先递归创建目录再校验并激活（断言 mkdir 与
 * validateDirectoryPath 的调用及响应形状）；不带 create 时复用既有
 * 激活流程、绝不创建目录。依赖全部为 stub。
 */
import { describe, expect, it, vi } from 'vitest';
import express from 'express';
import request from 'supertest';
import { registerOpenCodeRoutes } from './routes.js';

/** 组装带 stub 依赖的 Express 应用并注册被测路由，返回 app 与依赖句柄。 */
const createApp = (overrides = {}) => {
  const app = express();
  app.use(express.json());
  const dependencies = {
    fsPromises: { mkdir: vi.fn(async () => undefined) },
    validateDirectoryPath: vi.fn(async (directory) => ({ ok: true, directory })),
    readSettingsFromDisk: vi.fn(async () => ({ projects: [] })),
    sanitizeProjects: (projects) => projects,
    persistSettings: vi.fn(async (settings) => settings),
    ...overrides,
  };
  registerOpenCodeRoutes(app, dependencies);
  return { app, dependencies };
};

// 目录路由的创建/激活语义
describe('OpenCode project directory route', () => {
  it('creates and activates a requested project outside the active workspace', async () => {
    const { app, dependencies } = createApp();

    const response = await request(app)
      .post('/api/opencode/directory')
      .send({ path: '/projects/testing-one', create: true })
      .expect(200);

    expect(dependencies.fsPromises.mkdir).toHaveBeenCalledWith('/projects/testing-one', { recursive: true });
    expect(dependencies.validateDirectoryPath).toHaveBeenCalledWith('/projects/testing-one');
    expect(response.body).toMatchObject({ success: true, path: '/projects/testing-one' });
  });

  it('does not create a directory for the existing activation flow', async () => {
    const { app, dependencies } = createApp();

    await request(app)
      .post('/api/opencode/directory')
      .send({ path: '/projects/existing' })
      .expect(200);

    expect(dependencies.fsPromises.mkdir).not.toHaveBeenCalled();
  });
});
