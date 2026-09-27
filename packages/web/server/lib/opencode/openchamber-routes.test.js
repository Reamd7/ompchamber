/**
 * OMPChamber 自更新路由测试：聚焦前台（foreground）运行模式下的
 * /api/ompchamber/update-install——非 systemd 托管与非法 unit 名直接
 * 409 拒绝；systemd 环境下通过 systemd-run 的瞬时 unit 排队安装并返回
 * job 标识与日志查看方式。child_process 与 package-manager 均被打桩。
 */
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import express from 'express';
import path from 'node:path';
import request from 'supertest';

// 打桩 child_process：捕获 spawnSync 调用参数并阻止真实执行。
vi.mock('child_process', () => ({
  spawn: vi.fn(),
  spawnSync: vi.fn(),
}));

// 打桩包管理器的更新检查、命令构造与探测。
vi.mock('../package-manager.js', () => ({
  checkForUpdates: vi.fn(),
  getUpdateCommand: vi.fn(),
  detectPackageManagerDetails: vi.fn(),
}));

/** 被打桩的 child_process 模块句柄（断言 spawnSync 参数用）。 */
const childProcess = await import('child_process');
/** 被打桩的 package-manager 模块句柄。 */
const packageManager = await import('../package-manager.js');
/** 被测的 OMPChamber 路由注册器。 */
const { registerOMPChamberRoutes } = await import('./openchamber-routes.js');

/** 构造挂好被测路由的 express app；environment 注入 process.env，storedOptions 覆盖磁盘启动选项。 */
const createApp = ({ environment = {}, storedOptions = {} } = {}) => {
  const app = express();
  const dependencies = {
    fs: {
      existsSync: vi.fn(() => false),
      promises: {
        readFile: vi.fn(async () => JSON.stringify({
          launchMode: 'foreground',
          port: 7897,
          ...storedOptions,
        })),
      },
    },
    path,
    process: {
      env: environment,
      platform: 'linux',
      execPath: '/usr/bin/node',
    },
    server: {
      address: () => ({ port: 7897 }),
    },
    __dirname: '/opt/ompchamber/server',
    ompchamberDataDir: '/tmp/ompchamber',
    modelsDevApiUrl: 'https://models.example.test',
    modelsMetadataCacheTtl: 0,
    readSettingsFromDiskMigrated: vi.fn(),
    fetchFreeZenModels: vi.fn(),
    getCachedZenModels: vi.fn(),
  };

  registerOMPChamberRoutes(app, dependencies);
  return { app, dependencies };
};

// 每个用例前设定更新检查与包管理器桩的默认返回。
beforeEach(() => {
  packageManager.checkForUpdates.mockResolvedValue({
    available: true,
    version: '1.17.1',
  });
  packageManager.detectPackageManagerDetails.mockReturnValue({
    packageManager: 'npm',
  });
  packageManager.getUpdateCommand.mockReturnValue('npm install -g https://github.com/Reamd7/ompchamber/releases/latest/download/ompchamber-latest.tgz');
});

// 每个用例后还原并清空全部 mock。
afterEach(() => {
  vi.restoreAllMocks();
  vi.clearAllMocks();
});

// 前台更新路由套件：拒绝路径与 systemd 排队路径。
describe('OMPChamber foreground update route', () => {
  it('rejects a foreground update when the server is not owned by systemd', async () => {
    const { app } = createApp();

    await request(app)
      .post('/api/ompchamber/update-install')
      .expect(409, {
        error: 'Foreground servers must be updated by their service manager. Set OMPCHAMBER_SYSTEMD_UNIT when running under systemd, or run ompchamber update and restart the service.',
      });

    expect(childProcess.spawnSync).not.toHaveBeenCalled();
  });

  it('rejects an unsafe systemd unit override before starting an update job', async () => {
    const { app } = createApp({
      environment: {
        INVOCATION_ID: 'systemd-invocation',
        OMPCHAMBER_SYSTEMD_UNIT: 'ompchamber.service; rm -rf /',
      },
    });

    await request(app)
      .post('/api/ompchamber/update-install')
      .expect(409, {
        error: 'Foreground servers must be updated by their service manager. Set OMPCHAMBER_SYSTEMD_UNIT when running under systemd, or run ompchamber update and restart the service.',
      });

    expect(childProcess.spawnSync).not.toHaveBeenCalled();
  });

  it('queues the install in a transient systemd unit and returns its job identifier', async () => {
    vi.spyOn(Date, 'now').mockReturnValue(1_700_000_000_000);
    childProcess.spawnSync.mockReturnValue({ status: 0, stdout: '', stderr: '' });
    const { app } = createApp({
      environment: {
        INVOCATION_ID: 'systemd-invocation',
        OMPCHAMBER_SYSTEMD_UNIT: 'ompchamber@wsl.service',
        PATH: '/home/syu/.npm-global/bin:/usr/bin:/bin',
      },
    });

    await request(app)
      .post('/api/ompchamber/update-install')
      .expect(200, {
        success: true,
        message: 'Update queued; OMPChamber will restart after installation completes',
        version: '1.17.1',
        packageManager: 'npm',
        autoRestart: true,
        restartManager: 'systemd',
        jobId: 'ompchamber-update-1700000000000',
        logPath: 'journalctl --user-unit ompchamber-update-1700000000000.service',
      });

    expect(childProcess.spawnSync).toHaveBeenCalledWith('systemd-run', [
      '--user',
      '--unit=ompchamber-update-1700000000000',
      '--collect',
      '--service-type=exec',
      '--setenv=PATH=/home/syu/.npm-global/bin:/usr/bin:/bin',
      '/bin/sh',
      '-c',
      "set -eu\nnpm install -g https://github.com/Reamd7/ompchamber/releases/latest/download/ompchamber-latest.tgz\nsystemctl --user restart 'ompchamber@wsl.service'",
    ], {
      encoding: 'utf8',
      stdio: ['ignore', 'pipe', 'pipe'],
      timeout: 5000,
    });
  });
});
