import { afterEach, describe, expect, it, vi } from 'vitest';
import express from 'express';
import request from 'supertest';
import { registerOpenCodeRoutes } from './routes.js';

const originalFetch = globalThis.fetch;

afterEach(() => {
  globalThis.fetch = originalFetch;
});

const createApp = (overrides = {}) => {
  const app = express();
  app.use(express.json());
  const dependencies = {
    getOpenCodeUpgradeCapability: () => ({
      supported: false,
      manager: 'openchamber',
      reason: 'bundled',
    }),
    buildOpenCodeUrl: (pathname) => `http://127.0.0.1:4096${pathname}`,
    getOpenCodeAuthHeaders: () => ({}),
    refreshOpenCodeAfterConfigChange: vi.fn(async () => {}),
    ...overrides,
  };
  registerOpenCodeRoutes(app, dependencies);
  return { app, dependencies };
};

describe('OpenCode upgrade routes', () => {
  it('fails closed: the omp engine upgrades with OpenChamber itself', async () => {
    globalThis.fetch = vi.fn();
    const { app } = createApp();

    await request(app)
      .post('/api/opencode/upgrade')
      .send({})
      .expect(409, {
        upgraded: false,
        upgrade: { supported: false, manager: 'openchamber', reason: 'bundled' },
        error: 'The omp engine ships with OpenChamber and is upgraded by updating OpenChamber.',
      });

    expect(globalThis.fetch).not.toHaveBeenCalled();
  });

  it('reports bundled update ownership through the capability contract', async () => {
    globalThis.fetch = vi.fn(async () => new Response(JSON.stringify({ healthy: true, version: '1.18.8' }), {
      status: 200,
      headers: { 'Content-Type': 'application/json' },
    }));
    const { app } = createApp();

    const response = await request(app)
      .get('/api/opencode/upgrade-status')
      .expect(200);

    expect(response.body).toEqual({
      available: false,
      currentVersion: '1.18.8',
      latestVersion: null,
      upgrade: {
        supported: false,
        manager: 'openchamber',
        reason: 'bundled',
      },
    });
  });

  it('never contacts an upstream upgrade endpoint for the omp engine', async () => {
    globalThis.fetch = vi.fn();
    const { app } = createApp({
      getOpenCodeUpgradeCapability: () => ({
        supported: true,
        manager: 'opencode',
        reason: null,
      }),
    });

    await request(app)
      .post('/api/opencode/upgrade')
      .send({})
      .expect(409);

    expect(globalThis.fetch).not.toHaveBeenCalled();
  });
});
