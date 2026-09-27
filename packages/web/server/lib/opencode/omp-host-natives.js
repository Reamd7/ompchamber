// Runtime staging of the pi_natives native addon for the omp host engine.
//
// The engine dlopens pi_natives at boot and resolves it from the per-user
// cache ~/.omp/natives/<version>/ (or, for compiled binaries, from the
// executable's directory — the desktop packaging stages it there). An npm
// tarball install has neither on a clean machine: the loader does not look
// into node_modules, so the addon would be missing even though npm installs
// the matching platform package as an optional dependency of
// @oh-my-pi/pi-coding-agent. Before launching the host from source we copy
// the installed platform addon into the per-user cache the loader reads.

/**
 * 中文说明：为 omp host 引擎预置（staging）pi_natives 原生扩展。引擎
 * 启动时 dlopen pi_natives，并从按用户缓存 ~/.omp/natives/<version>/
 * 解析（编译版二进制则从可执行文件所在目录解析——桌面打包会把扩展预置
 * 在那里）。干净的 npm 安装上两处都没有：加载器不查 node_modules，因此
 * 即使 npm 已把匹配平台的平台包作为 @oh-my-pi/pi-coding-agent 的可选
 * 依赖装好，扩展仍会缺失。从源码启动 host 之前，本模块把已安装的平台
 * 扩展拷贝进加载器读取的按用户缓存。
 */

import { spawnSync } from 'node:child_process';
import { createRequire } from 'node:module';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';

/** 基于 ESM 入口构造 CommonJS require，用于解析 npm 包的安装路径。 */
const require = createRequire(import.meta.url);

/** process.platform 到平台包名后缀的映射；之外的平台直接跳过预置。 */
const NATIVES_PLATFORM_TAG = { win32: 'win32', darwin: 'darwin', linux: 'linux' };

/**
 * 列出目录中所有 pi_natives 原生扩展文件（形如 pi_natives.<...>.node）。
 * 目录不存在或不可读时返回空数组而不是抛错。
 */
const listAddonFiles = (dir) => {
  try {
    return fs.readdirSync(dir).filter((entry) => /^pi_natives\..+\.node$/.test(entry));
  } catch {
    return [];
  }
};

/**
 * 解析某个 npm 包在磁盘上的安装目录，兼容 npm 与 bun 的布局：先走标准
 * require.resolve；失败后逐个 node_modules 根尝试直接软链、bun 的提升根
 * （.bun/node_modules）以及 bun 的工作区 store 条目
 * （.bun/@scope+pkg@ver/node_modules/pkg）。找不到返回 null。
 */
const resolvePackageDir = (name) => {
  try {
    return path.dirname(require.resolve(`${name}/package.json`));
  } catch {
    // Exports-restricted or absent from the plain lookup paths.
  }
  // npm/bun layouts the resolver misses: direct links at each node_modules
  // root, bun's hoist root (.bun/node_modules), and bun's workspace store
  // entries (.bun/@scope+pkg@ver/node_modules/pkg).
  const storePrefix = name.replace('/', '+') + '@';
  for (const root of require.resolve.paths(name) ?? []) {
    for (const candidate of [path.join(root, name), path.join(root, '.bun', 'node_modules', name)]) {
      if (fs.existsSync(path.join(candidate, 'package.json'))) return candidate;
    }
    try {
      for (const entry of fs.readdirSync(path.join(root, '.bun'))) {
        if (!entry.startsWith(storePrefix)) continue;
        const candidate = path.join(root, '.bun', entry, 'node_modules', name);
        if (fs.existsSync(path.join(candidate, 'package.json'))) return candidate;
      }
    } catch {
      // Not a bun store root.
    }
  }
  return null;
};

/**
 * 确定 npm registry 地址：优先 NPM_CONFIG_REGISTRY 环境变量，其次项目
 * .npmrc 与用户 ~/.npmrc 中的 registry= 配置，最后回退官方地址；
 * 统一去掉末尾斜杠。
 */
const readNpmRegistry = () => {
  const explicit = process.env.NPM_CONFIG_REGISTRY?.trim();
  if (explicit) return explicit.replace(/\/+$/, '');
  for (const configFile of [path.join(process.cwd(), '.npmrc'), path.join(os.homedir(), '.npmrc')]) {
    try {
      const line = fs.readFileSync(configFile, 'utf8')
        .split(/\r?\n/)
        .find((entry) => /^\s*registry\s*=/.test(entry));
      if (line) return line.split('=').slice(1).join('=').trim().replace(/\/+$/, '');
    } catch {
      // No .npmrc at this location.
    }
  }
  return 'https://registry.npmjs.org';
};

/**
 * 平台包未随安装提供时，从 registry 下载对应 tarball，解压出其中的
 * pi_natives 扩展文件并拷贝到目标目录。下载超时 5 分钟，解压依赖系统
 * tar；任何一步失败都会抛错（由调用方决定是否仅告警）；临时目录在
 * finally 中无条件清理。
 */
const downloadPlatformPackage = async (packageName, version, destinationDir) => {
  const registry = readNpmRegistry();
  const unscoped = packageName.split('/').pop();
  const tarballUrl = `${registry}/${packageName}/-/${unscoped}-${version}.tgz`;
  console.log(`[omp-host] ${packageName} not installed; downloading ${tarballUrl}`);
  const response = await fetch(tarballUrl, { redirect: 'follow', signal: AbortSignal.timeout(300_000) });
  if (!response.ok) throw new Error(`Failed to download omp natives (${response.status}): ${tarballUrl}`);
  const tempDir = fs.mkdtempSync(path.join(os.tmpdir(), 'omp-natives-'));
  try {
    const tarballPath = path.join(tempDir, 'natives.tgz');
    fs.writeFileSync(tarballPath, Buffer.from(await response.arrayBuffer()));
    const extract = spawnSync('tar', ['-xzf', tarballPath, '-C', tempDir], { windowsHide: true });
    if (extract.status !== 0) throw new Error(`tar extract failed (${extract.status}) for ${tarballUrl}`);
    const files = listAddonFiles(path.join(tempDir, 'package'));
    if (files.length === 0) throw new Error(`npm tarball ${packageName}@${version} contains no pi_natives addon`);
    fs.mkdirSync(destinationDir, { recursive: true });
    for (const file of files) {
      fs.copyFileSync(path.join(tempDir, 'package', file), path.join(destinationDir, file));
    }
    console.log(`[omp-host] downloaded omp natives ${packageName}@${version}: ${files.join(', ')}`);
  } finally {
    fs.rmSync(tempDir, { recursive: true, force: true });
  }
};

/**
 * 中文补充：确保按用户缓存中存在当前平台的 natives 扩展；缓存里已有
 * 任意该版本扩展时为 no-op。失败只告警，把可操作的报错留给引擎自身——
 * 缺少扩展不应阻塞服务器其它工作。
 */
/**
 * Ensure the per-user natives cache holds the addon for this platform.
 * No-op when the cache already has any addon for the engine's natives
 * version. Failures log a warning and leave the engine to surface its own
 * actionable error — a missing addon must not block unrelated server work.
 */
export const ensureOmpHostNatives = async () => {
  const tag = NATIVES_PLATFORM_TAG[process.platform];
  if (!tag) return;

  const metaDir = resolvePackageDir('@oh-my-pi/pi-natives');
  if (!metaDir) {
    // No installed meta package to derive the addon version from; the
    // engine surfaces its own actionable error if the addon is missing.
    return;
  }
  let version;
  try {
    version = JSON.parse(fs.readFileSync(path.join(metaDir, 'package.json'), 'utf8')).version;
  } catch {
    return;
  }

  const cacheDir = path.join(os.homedir(), '.omp', 'natives', version);
  if (listAddonFiles(cacheDir).length > 0) return;

  const packageName = `@oh-my-pi/pi-natives-${tag}-${process.arch}`;
  const sourceDir = resolvePackageDir(packageName);
  if (sourceDir) {
    const files = listAddonFiles(sourceDir);
    if (files.length > 0) {
      fs.mkdirSync(cacheDir, { recursive: true });
      for (const file of files) {
        fs.copyFileSync(path.join(sourceDir, file), path.join(cacheDir, file));
      }
      console.log(`[omp-host] staged omp natives ${packageName}@${version} into ${cacheDir}`);
      return;
    }
  }

  try {
    await downloadPlatformPackage(packageName, version, cacheDir);
  } catch (error) {
    console.warn(`[omp-host] omp natives staging failed: ${error?.message ?? error}`);
  }
};
