/**
 * Loader for the sherpa-onnx-node native addon.
 *
 * sherpa-onnx-node ships its native addon and shared libraries in a
 * platform-specific package (e.g. sherpa-onnx-darwin-arm64). The shared
 * libraries must be findable via the platform's dynamic-loader search path,
 * so the loader prepends the platform package directory to LD_LIBRARY_PATH /
 * DYLD_LIBRARY_PATH / PATH before requiring the addon.
 */
/**
 * sherpa-onnx-node 原生 addon 的加载与动态库路径准备。
 *
 * addon 及其共享库位于平台专属包（如 sherpa-onnx-darwin-arm64）内，
 * 动态加载器（dlopen / DYLD / LD）无法直接找到，需先把该包目录前置
 * 到对应平台的库搜索环境变量；本模块负责定位平台包目录（含 Electron
 * app.asar.unpacked 解包场景）、按需修改环境变量，并按"上游入口 →
 * 平台 addon 直载"的顺序加载，全部失败时抛出汇总诊断信息的错误。
 */

import { createRequire } from 'module';
import path from 'path';
import { existsSync } from 'fs';

/** 在 ESM 环境中构造 require，用于加载 CJS 形态的原生 addon。 */
const require = createRequire(import.meta.url);

/** 已成功加载的 sherpa-onnx-node 模块缓存（进程内只加载一次）。 */
let cached = null;

/** 拼接平台专属包名（win32 归一化为 win），如 sherpa-onnx-darwin-arm64。 */
function sherpaPlatformPackageName(platform = process.platform, arch = process.arch) {
  const normalizedPlatform = platform === 'win32' ? 'win' : platform;
  return `sherpa-onnx-${normalizedPlatform}-${arch}`;
}

/**
 * 返回当前平台动态库搜索路径的环境变量名（Linux/Darwin/Windows
 * 各不相同），无需设置的平台返回 null。
 */
function sherpaLoaderEnvKey(platform = process.platform) {
  if (platform === 'linux') {
    return 'LD_LIBRARY_PATH';
  }
  if (platform === 'darwin') {
    return 'DYLD_LIBRARY_PATH';
  }
  if (platform === 'win32') {
    return 'PATH';
  }
  return null;
}

/** 把 value 前置到分隔符连接的路径列表；已存在则保持原顺序不重复。 */
function prependEnvPath(existing, value) {
  const parts = String(existing ?? '').split(path.delimiter).filter(Boolean);
  if (parts.includes(value)) {
    return parts.join(path.delimiter);
  }
  return [value, ...parts].join(path.delimiter);
}

/**
 * Case-insensitive env key lookup: on Windows `{...process.env}` yields a
 * plain object where PATH may be stored as `Path`. Using a hardcoded 'PATH'
 * would create a duplicate key and break the child process PATH.
 */
/**
 * 在 env 对象中大小写不敏感地查找 key 的实际键名，找不到则原样返回。
 */
function findEnvKey(env, key) {
  const lower = key.toLowerCase();
  for (const k of Object.keys(env)) {
    if (k.toLowerCase() === lower) {
      return k;
    }
  }
  return key;
}

/**
 * 解析平台包的库目录：Electron 打包时 node_modules 位于 app.asar 内，
 * 而动态加载器读不了 asar 归档，故优先返回 app.asar.unpacked 的解包
 * 副本；平台包未安装时返回 null。
 */
function resolveSherpaLibDir(platform = process.platform, arch = process.arch) {
  const packageName = sherpaPlatformPackageName(platform, arch);
  try {
    const pkgJson = require.resolve(`${packageName}/package.json`);
    // Electron packages node_modules inside app.asar, but native addons and
    // their shared libraries are extracted to app.asar.unpacked. The dynamic
    // loader (dlopen/DYLD/LD) cannot read from the asar archive, so point the
    // search path at the unpacked copy.
    const dir = path.dirname(pkgJson);
    const unpacked = dir.replace(`app.asar${path.sep}`, `app.asar.unpacked${path.sep}`);
    return existsSync(unpacked) ? unpacked : dir;
  } catch {
    return null;
  }
}

/**
 * Prepend the sherpa platform package dir to the loader search path env var.
 * Mutates the provided env object.
 * @param {NodeJS.ProcessEnv} env
 */
/**
 * 把平台包目录前置到动态库搜索路径变量（就地修改传入的 env）；
 * 平台无需设置或平台包缺失时不改动并返回 { key: null, libDir: null }。
 */
export function applySherpaLoaderEnv(env) {
  const key = sherpaLoaderEnvKey();
  const libDir = resolveSherpaLibDir();
  if (!key || !libDir) {
    return { key: null, libDir: null };
  }
  const actualKey = findEnvKey(env, key);
  env[actualKey] = prependEnvPath(env[actualKey], libDir);
  return { key, libDir };
}

/**
 * Load the sherpa-onnx-node module, trying the upstream entry first and then
 * the platform addon directly.
 */
/**
 * 加载 sherpa-onnx-node 模块：优先 require 上游入口；失败后对当前
 * 进程应用加载器环境变量并直接 require 平台 addon；全部失败时抛出
 * 汇总各次尝试原因的错误（附平台、Node 版本与 ABI 等诊断信息）。
 */
export function loadSherpaOnnxNode() {
  if (cached) {
    return cached;
  }

  const attempts = [];

  try {
    cached = require('sherpa-onnx-node');
    return cached;
  } catch (error) {
    attempts.push(`sherpa-onnx-node: ${error?.message || String(error)}`);
  }

  const libDir = resolveSherpaLibDir();
  if (libDir) {
    applySherpaLoaderEnv(process.env);
    const addonPath = path.join(libDir, 'sherpa-onnx.node');
    if (existsSync(addonPath)) {
      try {
        cached = require(addonPath);
        return cached;
      } catch (error) {
        attempts.push(`${addonPath}: ${error?.message || String(error)}`);
      }
    } else {
      attempts.push(`${addonPath}: file not found`);
    }
  } else {
    attempts.push(`${sherpaPlatformPackageName()}: platform package not installed`);
  }

  throw new Error(
    [
      `Failed to load sherpa-onnx-node for ${process.platform}-${process.arch}.`,
      `Node ${process.version} (ABI ${process.versions.modules}).`,
      'Load attempts:',
      ...attempts.map((line) => `- ${line}`),
    ].join('\n'),
  );
}
