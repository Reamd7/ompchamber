/**
 * omp-host-launch.js（托管 omp host 启动规格解析）的单元测试。
 *
 * 覆盖三级解析的捆绑分支契约：捆绑产物已是 JS bundle（dist/host.js），
 * 启动规格必须由 Bun 运行时执行该入口，而不是把 host.js 当可执行文件直接
 * spawn；显式二进制与源码分支行为保持不变；捆绑分支下无效的显式运行时
 * 以 OMP_HOST_RUNTIME_INVALID 暴露（与 env 覆盖同码，调用方只渲染一种
 * 失效路径）。
 */
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { afterEach, describe, expect, it } from 'vitest';

import { resolveOmpHostLaunchSpec, resolveOmpHostRuntimeBinary } from './omp-host-launch.js';

/** 逐项恢复测试触碰过的环境变量。 */
const savedEnv = {};
const ENV_KEYS = ['OMPCHAMBER_OMP_HOST_BINARY', 'OMPCHAMBER_BUNDLED_OMP_HOST_DIR', 'OMPCHAMBER_OMP_HOST_RUNTIME', 'OMPCHAMBER_MANAGED_BUN_DIR'];
for (const key of ENV_KEYS) savedEnv[key] = process.env[key];
afterEach(() => {
  for (const key of ENV_KEYS) {
    if (savedEnv[key] === undefined) delete process.env[key];
    else process.env[key] = savedEnv[key];
  }
});

/** 构造带 dist/host.js 的临时捆绑目录。 */
const makeBundledDir = () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'ompchamber-bundled-'));
  fs.mkdirSync(path.join(dir, 'dist'), { recursive: true });
  fs.writeFileSync(path.join(dir, 'dist', 'host.js'), '// bundle');
  return dir;
};

describe('resolveOmpHostLaunchSpec bundled branch', () => {
  it('launches the bundled host.js through a Bun runtime, not as an executable', () => {
    const bundledDir = makeBundledDir();
    process.env.OMPCHAMBER_BUNDLED_OMP_HOST_DIR = bundledDir;
    const spec = resolveOmpHostLaunchSpec({ hostname: '127.0.0.1', port: 3902 });
    expect(spec.source).toBe('bundled');
    expect(spec.args[0]).toBe(path.join(bundledDir, 'dist', 'host.js'));
    expect(spec.args.slice(1)).toEqual(['serve', '--hostname', '127.0.0.1', '--port', '3902']);
    // Runtime contract: bun executes the bundle. Under vitest the current
    // process is not Bun, so resolution falls to PATH (`bun`).
    expect(spec.binary).toBe(process.env.OMPCHAMBER_OMP_HOST_RUNTIME ?? 'bun');
  });

  it('honors an explicit runtime for the bundled branch and fails with the shared invalid-runtime code when missing', () => {
    const bundledDir = makeBundledDir();
    process.env.OMPCHAMBER_BUNDLED_OMP_HOST_DIR = bundledDir;
    const runtimePath = path.join(bundledDir, 'dist', 'host.js');
    process.env.OMPCHAMBER_OMP_HOST_RUNTIME = runtimePath;
    expect(resolveOmpHostLaunchSpec({ hostname: '127.0.0.1', port: 3902 }).binary).toBe(runtimePath);

    process.env.OMPCHAMBER_OMP_HOST_RUNTIME = path.join(bundledDir, 'does-not-exist');
    let thrown;
    try {
      resolveOmpHostLaunchSpec({ hostname: '127.0.0.1', port: 3902 });
    } catch (error) {
      thrown = error;
    }
    expect(thrown?.code).toBe('OMP_HOST_RUNTIME_INVALID');
  });

  it('falls back to the source entry when no bundled host.js exists', () => {
    const emptyDir = fs.mkdtempSync(path.join(os.tmpdir(), 'ompchamber-empty-'));
    process.env.OMPCHAMBER_BUNDLED_OMP_HOST_DIR = emptyDir;
    const spec = resolveOmpHostLaunchSpec({ hostname: '127.0.0.1', port: 3902 });
    expect(spec.source).not.toBe('bundled');
    expect(spec.args[0].endsWith('omp-host/host.ts')).toBe(true);
  });

  it('keeps the explicit self-contained executable branch ahead of the bundle', () => {
    const bundledDir = makeBundledDir();
    process.env.OMPCHAMBER_BUNDLED_OMP_HOST_DIR = bundledDir;
    const explicit = path.join(bundledDir, 'dist', 'host.js');
    process.env.OMPCHAMBER_OMP_HOST_BINARY = explicit;
    const spec = resolveOmpHostLaunchSpec({ hostname: '127.0.0.1', port: 3902 });
    expect(spec).toEqual({ binary: explicit, args: ['serve', '--hostname', '127.0.0.1', '--port', '3902'], source: 'env-host' });
  });
});

describe('resolveOmpHostRuntimeBinary managed layer', () => {
  it('prefers a managed runtime between env override and PATH lookup', () => {
    const runtimesDir = fs.mkdtempSync(path.join(os.tmpdir(), 'ompchamber-runtimes-'));
    const bunDir = path.join(runtimesDir, 'bun-9.9.9');
    fs.mkdirSync(bunDir, { recursive: true });
    const bunPath = path.join(bunDir, process.platform === 'win32' ? 'bun.exe' : 'bun');
    fs.writeFileSync(bunPath, '#!/bin/sh\n', { mode: 0o755 });
    process.env.OMPCHAMBER_MANAGED_BUN_DIR = runtimesDir;

    const resolved = resolveOmpHostRuntimeBinary();
    expect(resolved).toEqual({ binary: bunPath, source: 'managed' });
  });

  it('still honors an explicit env runtime ahead of a managed install', () => {
    const runtimesDir = fs.mkdtempSync(path.join(os.tmpdir(), 'ompchamber-runtimes-'));
    const bunDir = path.join(runtimesDir, 'bun-9.9.9');
    fs.mkdirSync(bunDir, { recursive: true });
    fs.writeFileSync(path.join(bunDir, 'bun'), '#!/bin/sh\n', { mode: 0o755 });
    process.env.OMPCHAMBER_MANAGED_BUN_DIR = runtimesDir;
    const explicit = path.join(runtimesDir, 'explicit-bun');
    fs.writeFileSync(explicit, '#!/bin/sh\n', { mode: 0o755 });
    process.env.OMPCHAMBER_OMP_HOST_RUNTIME = explicit;

    expect(resolveOmpHostRuntimeBinary()).toEqual({ binary: explicit, source: 'env' });
  });
});
