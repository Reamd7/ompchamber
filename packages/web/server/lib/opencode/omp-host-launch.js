// Launch-spec resolution for the managed omp host process.
//
// The managed engine is no longer the `opencode` CLI: it is the OMPChamber
// omp host (server/lib/omp-host/host.ts) run under Bun. The host embeds
// @oh-my-pi/pi-coding-agent and serves the OpenCode-compatible wire surface,
// so everything around the child process (ports, Basic auth, readiness line,
// health checks, orphan reaping) works exactly as it did for `opencode serve`.
/**
 * （中文说明）托管 omp host 进程的启动规格解析。
 *
 * 解析优先级：显式环境变量指定的自包含二进制 → 打包资源中的捆绑二进制 →
 * 用 Bun 运行时从源码入口（host.ts）启动。指定的路径不存在或源码入口缺失
 * 时抛出 code 为 OMP_HOST_RUNTIME_INVALID 的错误，由调用方决定如何呈现。
 */

import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

// 当前模块目录（ESM 下没有 __dirname，需从 import.meta.url 推导）
const __dirname = path.dirname(fileURLToPath(import.meta.url));

/** omp host 源码入口文件（server/lib/omp-host/host.ts）的绝对路径。 */
export const OMP_HOST_ENTRY = path.join(__dirname, '..', 'omp-host', 'host.ts');

/** 判断当前进程是否运行在 Bun 之下（Electron 以 Node 模式运行时视为否）。 */
const isBunRuntime = () => Boolean(process.versions?.bun) && !process.env.ELECTRON_RUN_AS_NODE;
/** 返回当前平台下打包 omp host 二进制的文件名（Windows 带 .exe 后缀）。 */
const hostBinaryName = () => (process.platform === 'win32' ? 'omp-host.exe' : 'omp-host');

/**
 * 在候选目录中查找打包随附的 omp host 二进制。
 * 候选：OMPCHAMBER_BUNDLED_OMP_HOST_DIR 环境变量指定的目录、Electron
 * resourcesPath 下的 omp-host 目录；任一命中即返回绝对路径，否则返回 null。
 * @returns {string|null}
 */
const findBundledHostBinary = () => {
  const dirs = [
    process.env.OMPCHAMBER_BUNDLED_OMP_HOST_DIR,
    process.resourcesPath ? path.join(process.resourcesPath, 'omp-host') : null,
  ].filter(Boolean);
  for (const dir of dirs) {
    const candidate = path.join(dir, hostBinaryName());
    if (fs.existsSync(candidate)) return candidate;
  }
  return null;
};

/**
 * Resolve the runtime binary that launches the omp host from source.
 *
 * Priority: explicit OMPCHAMBER_OMP_HOST_RUNTIME env → the current process
 * when it already runs under Bun → `bun` resolved from PATH.
 */
/**
 * （中文）解析用于从源码启动 omp host 的 Bun 运行时二进制。
 * 显式指定的路径不存在时抛出 OMP_HOST_RUNTIME_INVALID；当前进程就是 Bun
 * 时复用 process.execPath；否则依赖 PATH 解析 `bun`。
 * @returns {{ binary: string, source: string }} source 标注来源
 *   （'env' | 'current-process' | 'path'）。
 */
export const resolveOmpHostRuntimeBinary = () => {
  const explicit = (process.env.OMPCHAMBER_OMP_HOST_RUNTIME || '').trim();
  if (explicit) {
    if (!fs.existsSync(explicit)) {
      const error = new Error(`OMPCHAMBER_OMP_HOST_RUNTIME does not exist: ${explicit}`);
      error.code = 'OMP_HOST_RUNTIME_INVALID';
      throw error;
    }
    return { binary: explicit, source: 'env' };
  }
  if (isBunRuntime()) {
    return { binary: process.execPath, source: 'current-process' };
  }
  return { binary: 'bun', source: 'path' };
};

/**
 * Build the managed omp host launch spec.
 *
 * Priority: a packaged, self-contained host binary (explicit env or bundled
 * resources) → source entry launched under a Bun runtime.
 * @returns {{ binary: string, args: string[], source: string }}
 */
/**
 * （中文）构建托管 omp host 的完整启动规格（binary + args + source）。
 * serve 参数（--hostname/--port）始终追加；来源优先级为 OMPCHAMBER_OMP_HOST_BINARY
 * 显式自包含二进制 → 捆绑资源二进制 → Bun 运行时 + 源码入口；显式路径
 * 不存在或入口缺失时抛出 OMP_HOST_RUNTIME_INVALID。
 */
export const resolveOmpHostLaunchSpec = ({ hostname, port }) => {
  const serveArgs = ['serve', '--hostname', hostname, '--port', String(port)];

  const explicitHost = (process.env.OMPCHAMBER_OMP_HOST_BINARY || '').trim();
  if (explicitHost) {
    if (!fs.existsSync(explicitHost)) {
      const error = new Error(`OMPCHAMBER_OMP_HOST_BINARY does not exist: ${explicitHost}`);
      error.code = 'OMP_HOST_RUNTIME_INVALID';
      throw error;
    }
    return { binary: explicitHost, args: serveArgs, source: 'env-host' };
  }

  const bundled = findBundledHostBinary();
  if (bundled) {
    return { binary: bundled, args: serveArgs, source: 'bundled' };
  }

  if (!fs.existsSync(OMP_HOST_ENTRY)) {
    const error = new Error(`omp host entry missing: ${OMP_HOST_ENTRY}`);
    error.code = 'OMP_HOST_RUNTIME_INVALID';
    throw error;
  }
  const runtime = resolveOmpHostRuntimeBinary();
  return {
    binary: runtime.binary,
    args: [OMP_HOST_ENTRY, ...serveArgs],
    source: runtime.source,
  };
};
