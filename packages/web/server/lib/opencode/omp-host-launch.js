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
 * 解析优先级：显式环境变量指定的自包含可执行文件 → 打包资源中的捆绑
 * host.js（由 Bun 运行时执行）→ 用 Bun 运行时从源码入口（host.ts）启动。
 * 指定的路径不存在或源码入口缺失时抛出 code 为 OMP_HOST_RUNTIME_INVALID
 * 的错误，由调用方决定如何呈现。
 */

import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

import { resolveManagedBunRuntimeBinary } from './managed-bun-runtime.js';

// 当前模块目录（ESM 下没有 __dirname，需从 import.meta.url 推导）
const __dirname = path.dirname(fileURLToPath(import.meta.url));

/** omp host 源码入口文件（server/lib/omp-host/host.ts）的绝对路径。 */
export const OMP_HOST_ENTRY = path.join(__dirname, '..', 'omp-host', 'host.ts');

/** 捆绑 omp host 资源包（dist/ host.js + 资产 + sibling native/）内的 bundle 入口。 */
const HOST_BUNDLE_ENTRY = path.join('dist', 'host.js');

/** 判断当前进程是否运行在 Bun 之下（Electron 以 Node 模式运行时视为否）。 */
const isBunRuntime = () => Boolean(process.versions?.bun) && !process.env.ELECTRON_RUN_AS_NODE;

/**
 * 在候选目录中查找打包随附的 omp host bundle 入口（dist/host.js）。
 * 候选：OMPCHAMBER_BUNDLED_OMP_HOST_DIR 环境变量指定的目录、Electron
 * resourcesPath 下的 omp-host 目录；任一命中即返回该绝对路径，否则返回 null。
 * @returns {string|null}
 */
const findBundledHostJs = () => {
  const dirs = [
    process.env.OMPCHAMBER_BUNDLED_OMP_HOST_DIR,
    process.resourcesPath ? path.join(process.resourcesPath, 'omp-host') : null,
  ].filter(Boolean);
  for (const dir of dirs) {
    const candidate = path.join(dir, HOST_BUNDLE_ENTRY);
    if (fs.existsSync(candidate)) return candidate;
  }
  return null;
};

/**
 * Resolve the runtime binary that launches the omp host (bundled host.js or
 * source entry).
 *
 * Priority: explicit OMPCHAMBER_OMP_HOST_RUNTIME env → a managed runtime
 * installed under the app data dir → the current process when it already
 * runs under Bun → `bun` resolved from PATH.
 */
/**
 * （中文）解析用于启动 omp host（捆绑 host.js 或源码入口）的 Bun 运行时二进制。
 * 显式指定的路径不存在时抛出 OMP_HOST_RUNTIME_INVALID；其后依次尝试应用
 * 托管运行时（OMPCHAMBER_MANAGED_BUN_DIR → ~/.ompchamber/runtimes）、当前
 * 进程（Bun 下复用 process.execPath）、PATH 上的 `bun`。
 * @returns {{ binary: string, source: string }} source 标注来源
 *   （'env' | 'managed' | 'current-process' | 'path'）。
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
  const managed = resolveManagedBunRuntimeBinary();
  if (managed) {
    return { binary: managed.binary, source: 'managed' };
  }
  if (isBunRuntime()) {
    return { binary: process.execPath, source: 'current-process' };
  }
  return { binary: 'bun', source: 'path' };
};

/**
 * Build the managed omp host launch spec.
 *
 * Priority: a packaged self-contained executable (explicit env) → the bundled
 * host.js bundle launched under a Bun runtime → the source entry launched
 * under a Bun runtime.
 * @returns {{ binary: string, args: string[], source: string }}
 */
/**
 * （中文）构建托管 omp host 的完整启动规格（binary + args + source）。
 * serve 参数（--hostname/--port）始终追加；来源优先级为 OMPCHAMBER_OMP_HOST_BINARY
 * 显式自包含可执行文件 → 捆绑 dist/host.js（Bun 运行时执行）→ Bun 运行时 +
 * 源码入口；显式路径不存在或入口缺失时抛出 OMP_HOST_RUNTIME_INVALID。
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

  const bundledHostJs = findBundledHostJs();
  if (bundledHostJs) {
    // The bundle is JS, not a self-contained executable: it always needs a
    // Bun runtime (explicit env → managed → current process → PATH). A bad
    // explicit runtime surfaces through the same OMP_HOST_RUNTIME_INVALID
    // code the env override already uses.
    const runtime = resolveOmpHostRuntimeBinary();
    return {
      binary: runtime.binary,
      args: [bundledHostJs, ...serveArgs],
      source: 'bundled',
    };
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
