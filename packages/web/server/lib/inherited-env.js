/**
 * Sanitize environment objects inherited by user-facing child processes.
 *
 * Linux AppImage runtimes export `ARGV0` as the AppImage path before launching
 * the packaged app. zsh treats an exported `ARGV0` as the argv[0] for every
 * external command it spawns, which corrupts Python venv detection and any
 * other program that reads argv[0]/$0 while leaving `/proc/self/exe` correct.
 *
 * See openchamber/openchamber#2588 and pingdotgg/t3code#2509.
 */
/**
 * 本模块处理 Linux AppImage 泄漏的 ARGV0 环境变量——它会被 zsh
 * 当作外部命令的 argv[0]，破坏 Python venv 等依赖 $0 的探测。提供三层处理：
 * 从子进程 env 对象剔除、从当前进程环境（含 Bun 的原生 environ）清除、
 * 以及用 `env -u ARGV0` 包裹 PTY 启动命令。
 */

import { createRequire } from 'node:module';
import { existsSync } from 'node:fs';

/** Linux 上可用的 env 二进制候选，用于包裹 PTY 启动以在 exec 前 unset ARGV0。 */
const LINUX_ENV_BINARIES = ['/usr/bin/env', '/bin/env'];

/**
 * Remove AppImage `ARGV0` from a mutable env object (or `process.env`).
 * @param {NodeJS.ProcessEnv | Record<string, string | undefined> | null | undefined} env
 * @returns {typeof env}
 */
/** 原地删除 env 对象（或 process.env）上的 ARGV0 属性并原样返回该对象，便于链式使用。 */
export function stripAppImageArgv0Leak(env) {
  if (!env || typeof env !== 'object') return env;
  if (Object.prototype.hasOwnProperty.call(env, 'ARGV0')) {
    delete env.ARGV0;
  }
  return env;
}

/**
 * Clear AppImage `ARGV0` from this process.
 *
 * Bun keeps a native environ that `bun-pty` inherits even after
 * `delete process.env.ARGV0`. On Linux under Bun we also call libc `unsetenv`.
 */
/**
 * 清除当前进程环境中的 ARGV0：先 delete process.env；Linux 上的 Bun 因 bun-pty
 * 会继承原生 environ，还需经 bun:ffi 调 libc unsetenv 才真正移除。非 Linux 或
 * 无 bun:ffi 时静默跳过（依赖调用方显式构造子进程 env 兜底）。
 */
export function clearAppImageArgv0FromProcessEnv() {
  delete process.env.ARGV0;
  if (process.platform !== 'linux' || typeof Bun === 'undefined') return;
  try {
    const require = createRequire(import.meta.url);
    const { dlopen } = require('bun:ffi');
    const libc = dlopen('libc.so.6', {
      unsetenv: { args: ['cstring'], returns: 'i32' },
    });
    libc.symbols.unsetenv(Buffer.from('ARGV0\0'));
  } catch {
    // Node/Electron and environments without bun:ffi rely on explicit child envs.
  }
}

/**
 * Resolve a Linux PTY launch that drops native `ARGV0` before the shell starts.
 *
 * `bun-pty` merges the OS environ into the child, so deleting `ARGV0` from the
 * JS env object alone is not enough. Wrapping with `env -u ARGV0` unsets it
 * before execing the real shell. No-op on non-Linux platforms.
 *
 * @param {string} executable
 * @param {string[]} args
 * @returns {{ executable: string, args: string[] }}
 */
/**
 * Linux 上把 PTY 启动命令改写为 `<env> -u ARGV0 <executable> <args...>`，在
 * 真实 shell 启动前 unset 掉原生 ARGV0；非 Linux 平台或找不到可用 env 二进制时
 * 原样返回。
 */
export function resolveLinuxPtyLaunch(executable, args = []) {
  if (process.platform !== 'linux') {
    return { executable, args };
  }
  const envBinary = LINUX_ENV_BINARIES.find((candidate) => existsSync(candidate));
  if (!envBinary) {
    return { executable, args };
  }
  return {
    executable: envBinary,
    args: ['-u', 'ARGV0', executable, ...args],
  };
}
