/**
 * 可执行文件 PATH 搜索模块。
 *
 * 为隧道 CLI（cloudflared/ngrok 等）的子进程启动提供跨平台的命令定位能力：
 * 解析 PATH 环境变量（兼容 Windows 的 PATH/Path/path 大小写变体）、追加 Windows
 * Store 应用执行别名目录（WindowsApps）、按 PATHEXT 展开候选文件名、校验可执行
 * 权限，并生成注入子进程的净化 PATH 环境副本。env/platform/fsLike 均可注入以便测试。
 */
import fs from 'fs';
import os from 'os';
import path from 'path';

/**
 * 依序从 env 中取第一个非空字符串值（keys 一般为同一变量名的大小写变体）。
 * @param {object} env 环境变量对象
 * @param {string[]} keys 候选键名（按顺序）
 * @returns {string} 全部未命中时返回空串
 */
const getEnvValue = (env, keys) => {
  for (const key of keys) {
    const value = env?.[key];
    if (typeof value === 'string' && value.trim().length > 0) {
      return value;
    }
  }
  return '';
};

/**
 * 生成搜索目录的去重键：Windows 下转小写以忽略大小写差异，其余平台原样返回。
 */
const normalizeSearchDirectoryKey = (directory, platform) => {
  const trimmed = typeof directory === 'string' ? directory.trim() : '';
  return platform === 'win32' ? trimmed.toLowerCase() : trimmed;
};

/**
 * 计算 Windows Store 应用执行别名目录（…\AppData\Local\Microsoft\WindowsApps）。
 * 依次尝试 LOCALAPPDATA、USERPROFILE 推导，最后退回 os.homedir()，保证总有返回值。
 * @param {object} env 环境变量对象
 * @returns {string} WindowsApps 目录的 Windows 风格路径
 */
const getWindowsAppsDirectory = (env) => {
  const localAppData = getEnvValue(env, ['LOCALAPPDATA', 'LocalAppData', 'localappdata']);
  if (localAppData) {
    return path.win32.join(localAppData, 'Microsoft', 'WindowsApps');
  }

  const userProfile = getEnvValue(env, ['USERPROFILE', 'UserProfile', 'userprofile']);
  if (userProfile) {
    return path.win32.join(userProfile, 'AppData', 'Local', 'Microsoft', 'WindowsApps');
  }

  return path.win32.join(os.homedir(), 'AppData', 'Local', 'Microsoft', 'WindowsApps');
};

/**
 * 计算可执行文件搜索目录列表：按平台分隔符（win32 为 ';'，其余为 ':'）切分 PATH，
 * 去空去重（Windows 忽略大小写），并在 Windows 平台额外追加 WindowsApps 别名目录
 * （ngrok 等 Store 应用安装后可能不在 PATH 中）。
 * @param {object} options env 环境变量、platform 平台（默认取当前进程值）
 * @returns {string[]} 去重后的目录列表
 */
export function getExecutableSearchDirectories({ env = process.env, platform = process.platform } = {}) {
  const delimiter = platform === 'win32' ? ';' : ':';
  const pathValue = getEnvValue(env, ['PATH', 'Path', 'path']);
  const directories = pathValue.split(delimiter).map((entry) => entry.trim()).filter(Boolean);

  if (platform === 'win32') {
    directories.push(getWindowsAppsDirectory(env));
  }

  const seen = new Set();
  const unique = [];
  for (const directory of directories) {
    const key = normalizeSearchDirectoryKey(directory, platform);
    if (!key || seen.has(key)) {
      continue;
    }
    seen.add(key);
    unique.push(directory);
  }

  return unique;
}

/**
 * 构造带净化 PATH 的环境变量副本：以 getExecutableSearchDirectories 的结果重拼 PATH；
 * Windows 下同时写入 PATH/Path/path 三个大小写变体，避免子进程读取到旧值。
 * @param {object} options env、platform（默认取当前进程值）
 * @returns {object} 新的环境对象（浅拷贝，不修改入参）
 */
export function createExecutableSearchEnv({ env = process.env, platform = process.platform } = {}) {
  const delimiter = platform === 'win32' ? ';' : ':';
  const pathValue = getExecutableSearchDirectories({ env, platform }).join(delimiter);
  const nextEnv = { ...env };

  if (platform === 'win32') {
    nextEnv.PATH = pathValue;
    nextEnv.Path = pathValue;
    nextEnv.path = pathValue;
  } else {
    nextEnv.PATH = pathValue;
  }

  return nextEnv;
}

/**
 * 返回可执行文件的候选扩展名列表：非 Windows 平台为 ['']（无扩展名拼接）；
 * Windows 从 PATHEXT 解析（缺省 '.EXE;.CMD;.BAT;.COM'），统一为小写并确保带点前缀。
 */
const getExecutableExtensions = ({ env = process.env, platform = process.platform } = {}) => {
  if (platform !== 'win32') {
    return [''];
  }

  return (env.PATHEXT || env.PathExt || env.pathext || '.EXE;.CMD;.BAT;.COM')
    .split(';')
    .map((ext) => ext.trim().toLowerCase())
    .filter(Boolean)
    .map((ext) => (ext.startsWith('.') ? ext : `.${ext}`));
};

/**
 * 在搜索目录中查找 command 对应的可执行文件并返回绝对路径。
 * Windows 下按扩展名逐个拼接候选文件名；非 Windows 额外用 accessSync(X_OK)
 * 校验可执行位。stat/access 的任何异常都视为该候选不存在并继续。
 * @param {string} command 命令名（不含目录部分）
 * @param {object} options env/platform 可注入；fsLike 可替换 fs 实现便于测试
 * @returns {string|null} 命中返回绝对路径，否则 null
 */
export function findExecutableOnPath(command, {
  env = process.env,
  platform = process.platform,
  fsLike = fs,
} = {}) {
  if (typeof command !== 'string' || command.trim().length === 0) {
    return null;
  }

  const pathApi = platform === 'win32' ? path.win32 : path;
  const directories = getExecutableSearchDirectories({ env, platform });
  const extensions = getExecutableExtensions({ env, platform });
  const commandName = command.trim();

  for (const directory of directories) {
    for (const extension of extensions) {
      const fileName = platform === 'win32' ? `${commandName}${extension}` : commandName;
      const candidate = pathApi.join(directory, fileName);
      try {
        const stats = fsLike.statSync(candidate);
        if (!stats.isFile()) {
          continue;
        }
        if (platform !== 'win32') {
          try {
            fsLike.accessSync(candidate, fs.constants.X_OK);
          } catch {
            continue;
          }
        }
        return candidate;
      } catch {
        continue;
      }
    }
  }

  return null;
}

/**
 * 解析子进程的启动目标：PATH 命中时返回 { command: 绝对路径, env: 净化后的环境 }。
 * Windows 下即使 stat 查找失败，只要命令名非空也回退原始命令名（Windows Store
 * 执行别名可被 CreateProcess 启动但会对 stat/access 报 EACCES，交给后续版本探测
 * 判定真伪）；其余情况返回 null 表示无从启动。
 * @param {string} command 命令名
 * @param {object} options 透传给 findExecutableOnPath/createExecutableSearchEnv 的选项
 * @returns {{command: string, env: object}|null}
 */
export function resolveExecutableLaunchTarget(command, options = {}) {
  const platform = options.platform || process.platform;
  const resolvedPath = findExecutableOnPath(command, { ...options, platform });
  const env = createExecutableSearchEnv({ env: options.env || process.env, platform });
  if (resolvedPath) {
    return { command: resolvedPath, env };
  }

  // Windows Store app execution aliases are launchable through CreateProcess
  // but can reject fs.stat/fs.access with EACCES. Let the version probe decide.
  if (platform === 'win32' && typeof command === 'string' && command.trim().length > 0) {
    return { command: command.trim(), env };
  }

  return null;
}
