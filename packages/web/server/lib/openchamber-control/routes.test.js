/**
 * OMPChamber 控制路由（routes.js）的测试套件。
 *
 * 用 supertest 驱动真实 express app + 桩 controlService：验证路由是服
 * 务之上的薄适配（透传 action/input/contextDirectory 并带回 AbortSignal），
 * 以及错误响应保留服务的 statusCode 与 partial 部分结果详情。
 */
import express from 'express';
import request from 'supertest';
import { describe, expect, it, vi } from 'vitest';

import { OMPChamberControlError } from './error.js';
import { registerOMPChamberControlRoutes } from './routes.js';

/** 用给定的 execute 桩构造注册了控制路由的 express app。 */
const createApp = (execute) => {
  const app = express();
  registerOMPChamberControlRoutes(app, { controlService: { execute } });
  return app;
};

// 控制路由契约：成功透传服务结果，失败按 OMPChamberControlError 的
// 状态码与 partial 详情响应。
describe('OMPChamber control route', () => {
  it('is a thin adapter over the control service', async () => {
    const execute = vi.fn(async () => ({ projects: [] }));
    const response = await request(createApp(execute))
      .post('/api/ompchamber/control')
      .send({ action: 'projects.list', input: {}, contextDirectory: '/repo' })
      .expect(200);
    expect(response.body).toEqual({ projects: [] });
    expect(execute).toHaveBeenCalledWith('projects.list', {}, '/repo', expect.objectContaining({ signal: expect.any(AbortSignal) }));
  });

  it('preserves service status and partial-result details', async () => {
    const execute = vi.fn(async () => {
      throw new OMPChamberControlError('dispatch failed', 500, {
        partial: true,
        partialAction: 'fork-created',
        sessionId: 'ses_fork',
        directory: '/repo',
      });
    });
    const response = await request(createApp(execute))
      .post('/api/ompchamber/control')
      .send({ action: 'session.fork', input: {} })
      .expect(500);
    expect(response.body).toEqual({
      error: 'dispatch failed',
      partial: true,
      partialAction: 'fork-created',
      sessionId: 'ses_fork',
      directory: '/repo',
    });
  });
});
