/**
 * OpenCode 托管运行时的二进制与运行环境解析工厂。
 *
 * createOpenCodeEnvRuntime(deps) 在启动/重启时负责两件事：把用户 login
 * shell 的环境并入 process.env（PATH 合并、缺失变量补齐），以及解析各
 * 运行时二进制的绝对路径——omp host 运行时（Bun）、node、bun、git，以
 * 及 Windows 上 opencode 包装脚本的启动规格（native 二进制 / node 或
 * bun 解释器 / cmd 包装）。解析结果缓存进 state 供重启复用，并把二进制
 * 所在目录前置到 PATH，使后续子进程直接可见。
 */
import { spawnSync } from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { clearAppImageArgv0FromProcessEnv } from '../inherited-env.js';
import { mergePathValues } from './path-utils.js';

/** login-shell 探测的超时上限（毫秒）：超时即放弃该候选 shell，落入下一个，避免拖住服务启动。 */
// Login-shell probes source the user's rc files. A slow or interactive rc
// (nvm, pyenv, a prompt waiting for input) must not hold server startup
// hostage: a probe that overruns is abandoned and resolution falls through
// to the next candidate. Electron's own login-shell probe uses the same bound.
const SHELL_PROBE_TIMEOUT_MS = 5_000;

/**
 * 创建 OpenCode 运行环境解析器。
 *
 * deps.state 为跨调用共享的可变状态对象（缓存已解析二进制与环境快照）；
 * spawnSync 与 homedir 可注入替代实现（测试用），默认取
 * node:child_process 与 os.homedir。返回的方法无内部锁，按"首次解析、
 * 之后读缓存"的方式工作。
 */
export const createOpenCodeEnvRuntime = (deps) => {
  const {
    state,
    normalizeDirectoryPath,
  } = deps;
  /** 实际使用的 spawnSync：优先注入实现，默认 node:child_process 的同名函数。 */
  const runSpawnSync = typeof deps.spawnSync === 'function' ? deps.spawnSync : spawnSync;
  /** 实际使用的 home 目录解析函数：优先注入，默认 os.homedir。 */
  const resolveHomeDir = typeof deps.homedir === 'function' ? deps.homedir : () => os.homedir();

  /**
   * 解析 NUL 分隔的环境快照（env -0 / PowerShell 输出）为普通对象。
   * 空串或无有效条目返回 null；Windows 上若缺少大写 PATH 键，则把任意
   * 大小写变体的 path 条目补一份大写键（spawn 需要）。
   */
  const parseNullSeparatedEnvSnapshot = (raw) => {
    if (typeof raw !== 'string' || raw.length === 0) {
      return null;
    }

    const result = {};
    const entries = raw.split('\0');
    for (const entry of entries) {
      if (!entry) {
        continue;
      }
      const idx = entry.indexOf('=');
      if (idx <= 0) {
        continue;
      }
      const key = entry.slice(0, idx);
      const value = entry.slice(idx + 1);
      result[key] = value;
    }

    if (Object.keys(result).length === 0) {
      return null;
    }

    if (process.platform === 'win32' && typeof result.PATH !== 'string') {
      const pathEntry = Object.entries(result).find(([key]) => key.toLowerCase() === 'path');
      if (pathEntry && typeof pathEntry[1] === 'string') {
        result.PATH = pathEntry[1];
      }
    }

    return result;
  };

  /**
   * 判断路径是否为可执行文件：必须存在且为普通文件。Windows 按扩展名
   * 判断（.exe/.cmd/.bat/.com，无扩展名视为可执行）；其它平台用
   * accessSync(X_OK) 检查执行位。任何异常都按"不可执行"处理。
   */
  const isExecutable = (filePath) => {
    try {
      const stat = fs.statSync(filePath);
      if (!stat.isFile()) return false;
      if (process.platform === 'win32') {
        const ext = path.extname(filePath).toLowerCase();
        if (!ext) return true;
        return ['.exe', '.cmd', '.bat', '.com'].includes(ext);
      }
      fs.accessSync(filePath, fs.constants.X_OK);
      return true;
    } catch {
      return false;
    }
  };

  /**
   * Windows 专用：补全候选路径的可执行扩展名并验证。
   * 非字符串、空串或非 Windows 平台原样返回；已带扩展名直接验证；
   * 无扩展名则按 PATHEXT 逐个拼接验证，全部失败再试原始名，都不行返回 null。
   */
  const resolveWindowsExecutablePath = (candidate) => {
    if (process.platform !== 'win32' || typeof candidate !== 'string' || candidate.trim().length === 0) {
      return candidate;
    }

    const trimmed = candidate.trim();
    const ext = path.extname(trimmed).toLowerCase();
    if (ext) {
      return isExecutable(trimmed) ? trimmed : null;
    }

    const pathExt = process.env.PATHEXT || process.env.PathExt || '.COM;.EXE;.BAT;.CMD';
    for (const rawExt of pathExt.split(';')) {
      const normalizedExt = rawExt.trim();
      if (!normalizedExt) continue;
      const withExt = `${trimmed}${normalizedExt.startsWith('.') ? normalizedExt : `.${normalizedExt}`}`;
      if (isExecutable(withExt)) {
        return withExt;
      }
    }

    return isExecutable(trimmed) ? trimmed : null;
  };

  /**
   * 在指定 PATH（默认 process.env.PATH）中查找可执行文件。
   * Windows 上无扩展名的名字先按 PATHEXT 展开为多个候选（大小写去重）
   * 再加上原始名；逐目录逐候选验证，命中返回绝对路径，找不到返回 null。
   */
  const searchPathFor = (binaryName, searchPath = process.env.PATH || '') => {
    const trimmed = typeof binaryName === 'string' ? binaryName.trim() : '';
    if (!trimmed) {
      return null;
    }

    const parts = searchPath.split(path.delimiter).filter(Boolean);
    const candidateNames = [];

    if (process.platform === 'win32' && !path.extname(trimmed)) {
      const pathExt = process.env.PATHEXT || process.env.PathExt || '.COM;.EXE;.BAT;.CMD';
      for (const ext of pathExt.split(';')) {
        const normalizedExt = ext.trim();
        if (!normalizedExt) continue;
        const candidateName = `${trimmed}${normalizedExt.startsWith('.') ? normalizedExt : `.${normalizedExt}`}`;
        if (!candidateNames.some((existing) => existing.toLowerCase() === candidateName.toLowerCase())) {
          candidateNames.push(candidateName);
        }
      }
    }

    candidateNames.push(trimmed);

    for (const dir of parts) {
      for (const candidateName of candidateNames) {
        const candidate = path.join(dir, candidateName);
        if (isExecutable(candidate)) {
          return candidate;
        }
      }
    }
    return null;
  };

  /** 把目录前置到 process.env.PATH（目录已存在或为空时跳过），使后续子进程优先命中。 */
  const prependToPath = (dir) => {
    const trimmed = typeof dir === 'string' ? dir.trim() : '';
    if (!trimmed) return;
    const current = process.env.PATH || '';
    const parts = current.split(path.delimiter).filter(Boolean);
    if (parts.includes(trimmed)) return;
    process.env.PATH = [trimmed, ...parts].join(path.delimiter);
  };

  /**
   * 抓取 Windows 登录 shell 的完整环境快照。
   * 依次尝试 pwsh.exe、powershell.exe 与系统绝对路径的 PowerShell：用
   * 脚本合并 Machine/User/Process 三级 Path 后以 NUL 分隔输出全部变量；
   * 全部失败退回 ComSpec 的 set 输出（换行转 NUL 再解析）。
   * 任一级成功即返回解析结果，彻底失败返回 null。
   */
  const getWindowsShellEnvSnapshot = () => {
  /** 把子进程 stdout 解析为环境快照（复用 NUL 解析器）。 */
    const parseResult = (stdout) => parseNullSeparatedEnvSnapshot(typeof stdout === 'string' ? stdout : '');

    // PowerShell 脚本：合并三级 Path 并按 NUL 分隔输出全部环境变量。
    const psScript = [
      '$entries = [ordered]@{}',
      'Get-ChildItem Env: | ForEach-Object { $entries[$_.Name] = $_.Value }',
      "$pathValues = @([Environment]::GetEnvironmentVariable('Path', 'Machine'), [Environment]::GetEnvironmentVariable('Path', 'User'), [Environment]::GetEnvironmentVariable('Path', 'Process')) | Where-Object { $_ }",
      "if ($pathValues.Count -gt 0) { $entries['Path'] = ($pathValues -join ';') }",
      "$entries.GetEnumerator() | ForEach-Object { [Console]::Out.Write($_.Name); [Console]::Out.Write('='); [Console]::Out.Write($_.Value); [Console]::Out.Write([char]0) }",
    ].join('; ');

    // 依次尝试的 PowerShell 候选：pwsh、内置 powershell 及系统绝对路径。
    const powershellCandidates = [
      'pwsh.exe',
      'powershell.exe',
      path.join(process.env.SystemRoot || 'C:\\Windows', 'System32', 'WindowsPowerShell', 'v1.0', 'powershell.exe'),
    ];

    for (const shellPath of powershellCandidates) {
      try {
        const result = runSpawnSync(shellPath, ['-NoLogo', '-Command', psScript], {
          encoding: 'utf8',
          stdio: ['ignore', 'pipe', 'pipe'],
          maxBuffer: 10 * 1024 * 1024,
          windowsHide: true,
        });
        if (result.status !== 0) {
          continue;
        }
        const parsed = parseResult(result.stdout);
        if (parsed) {
          return parsed;
        }
      } catch {
      }
    }

    // 最后退路：用 cmd.exe 的 set 输出抓取环境。
    const comspec = process.env.ComSpec || 'cmd.exe';
    try {
      const result = runSpawnSync(comspec, ['/d', '/s', '/c', 'set'], {
        encoding: 'utf8',
        stdio: ['ignore', 'pipe', 'pipe'],
        maxBuffer: 10 * 1024 * 1024,
        windowsHide: true,
      });
      if (result.status === 0 && typeof result.stdout === 'string' && result.stdout.length > 0) {
        return parseNullSeparatedEnvSnapshot(result.stdout.replace(/\r?\n/g, '\0'));
      }
    } catch {
    }

    return null;
  };

  /**
   * 获取（并缓存到 state.cachedLoginShellEnvSnapshot）登录 shell 的环境
   * 快照。Windows 走 getWindowsShellEnvSnapshot；其它平台依次探测
   * $SHELL、/bin/zsh、/bin/bash、/bin/sh，以登录+交互模式执行 env -0
   * （受 SHELL_PROBE_TIMEOUT_MS 约束），首个成功者胜出；全部失败缓存
   * null。undefined 表示尚未探测过，之后调用直接读缓存。
   */
  const getLoginShellEnvSnapshot = () => {
    if (state.cachedLoginShellEnvSnapshot !== undefined) {
      return state.cachedLoginShellEnvSnapshot;
    }

    if (process.platform === 'win32') {
      const windowsSnapshot = getWindowsShellEnvSnapshot();
      state.cachedLoginShellEnvSnapshot = windowsSnapshot;
      return windowsSnapshot;
    }

    // 依次探测的登录 shell 候选：$SHELL 优先，随后是常见默认 shell。
    const shellCandidates = [process.env.SHELL, '/bin/zsh', '/bin/bash', '/bin/sh'].filter(Boolean);

    for (const shellPath of shellCandidates) {
      if (!isExecutable(shellPath)) {
        continue;
      }

      try {
        const result = runSpawnSync(shellPath, ['-lic', 'env -0'], {
          encoding: 'utf8',
          stdio: ['ignore', 'pipe', 'pipe'],
          maxBuffer: 10 * 1024 * 1024,
          windowsHide: true,
          timeout: SHELL_PROBE_TIMEOUT_MS,
        });

        if (result.status !== 0) {
          continue;
        }

        const parsed = parseNullSeparatedEnvSnapshot(result.stdout || '');
        if (parsed) {
          state.cachedLoginShellEnvSnapshot = parsed;
          return parsed;
        }
      } catch {
      }
    }

    state.cachedLoginShellEnvSnapshot = null;
    return null;
  };

  /**
   * 把登录 shell 环境快照并入当前 process.env。
   * 先无条件清理 AppImage 泄漏的 ARGV0（#2588，即使没有快照也要清）；
   * 随后仅补齐当前缺失（空值）的变量，绝不覆盖已有值；PWD/OLDPWD/
   * SHLVL/_/ARGV0 一律跳过；最后把快照 PATH 与当前 PATH 合并（快照优先）。
   */
  const applyLoginShellEnvSnapshot = () => {
    // Always clear AppImage ARGV0, even when no login-shell snapshot is available.
    // Otherwise a leaked process.env.ARGV0 survives into later child spawns (#2588).
    clearAppImageArgv0FromProcessEnv();

    const snapshot = getLoginShellEnvSnapshot();
    if (!snapshot) {
      return;
    }

    // 不并入的键：工作目录、shell 层级、上一次命令等只对原 shell 有意义。
    const skipKeys = new Set(['PWD', 'OLDPWD', 'SHLVL', '_', 'ARGV0']);
    for (const [key, value] of Object.entries(snapshot)) {
      if (skipKeys.has(key)) {
        continue;
      }
      const existing = process.env[key];
      if (typeof existing === 'string' && existing.length > 0) {
        continue;
      }
      process.env[key] = value;
    }

    const currentPath = process.env.PATH || '';
    const shellPath = snapshot.PATH || '';
    if (!shellPath) {
      return;
    }

    process.env.PATH = mergePathValues(shellPath, currentPath, path.delimiter);
  };


  /**
   * 判断候选路径是否为 Windows 版 OpenCode 桌面应用自带的 opencode.exe
   * （LOCALAPPDATA 下的 Programs\opencode\opencode.exe）。该副本与托管
   * omp host 不兼容，解析 bundled CLI 时需要排除。
   */
  const isWindowsOpenCodeDesktopAppPath = (candidate) => {
    if (process.platform !== 'win32' || typeof candidate !== 'string') {
      return false;
    }
    const normalized = path.resolve(candidate).toLowerCase();
    const localAppData = typeof process.env.LOCALAPPDATA === 'string' && process.env.LOCALAPPDATA.trim()
      ? path.resolve(process.env.LOCALAPPDATA).toLowerCase()
      : '';
    if (!localAppData || !normalized.startsWith(`${localAppData}${path.sep}`)) {
      return false;
    }
    return normalized.endsWith(`${path.sep}programs${path.sep}opencode${path.sep}opencode.exe`);
  };

  /** 枚举随应用分发的 opencode CLI 候选路径：OMPCHAMBER_BUNDLED_OPENCODE_CLI_DIR 与 Electron resources 目录下的 opencode-cli，按平台拼文件名。 */
  const bundledOpenCodeCliCandidates = () => {
    const names = process.platform === 'win32' ? ['opencode.exe'] : ['opencode'];
    const roots = [
      process.env.OMPCHAMBER_BUNDLED_OPENCODE_CLI_DIR,
      typeof process.resourcesPath === 'string' ? path.join(process.resourcesPath, 'opencode-cli') : null,
    ]
      .map((value) => (typeof value === 'string' ? value.trim() : ''))
      .filter(Boolean);

    const candidates = [];
    for (const root of roots) {
      for (const name of names) {
        candidates.push(path.join(root, name));
      }
    }
    return candidates;
  };

  /** 返回首个存在且可执行、且不是 Windows 桌面应用自带副本的 bundled CLI 路径；无则 null。 */
  const resolveBundledOpenCodeCliPath = () => {
    for (const candidate of bundledOpenCodeCliCandidates()) {
      if (isExecutable(candidate) && !isWindowsOpenCodeDesktopAppPath(candidate)) {
        return candidate;
      }
    }
    return null;
  };

  /** 求候选路径的规范形态：realpath.native 成功即用真实路径，失败退回 resolve 结果；空串返回 null。 */
  const canonicalExecutablePath = (candidate) => {
    if (typeof candidate !== 'string' || !candidate.trim()) return null;
    try {
      return fs.realpathSync.native(candidate.trim());
    } catch {
      return path.resolve(candidate.trim());
    }
  };

  /** 判断候选路径（按规范形态比较，穿过 symlink）是否就是随应用分发的 opencode CLI。 */
  const isBundledOpenCodeCliPath = (candidate) => {
    const canonicalCandidate = canonicalExecutablePath(candidate);
    if (!canonicalCandidate) return false;
    return bundledOpenCodeCliCandidates().some((bundledCandidate) => (
      canonicalExecutablePath(bundledCandidate) === canonicalCandidate
    ));
  };

  /** bundled CLI 兜底：命中时把来源标记为 'bundled' 并返回路径，否则返回 null。 */
  const bundledOpenCodeCliFallback = () => {
    const bundled = resolveBundledOpenCodeCliPath();
    if (!bundled) return null;
    state.resolvedOpencodeBinarySource = 'bundled';
    return bundled;
  };


  /** 剥掉一整层包裹引号（Windows"复制文件地址"与带引号的 shell 片段）：字面引号不是真实路径的一部分，保留会让一切可执行性检查失效。 */
  // Strip a single wrapping quote pair (Windows "Copy as path" and quoted
  // shell snippets) — literal quotes are never part of a real path and break
  // every executable check.
  const stripWrappingQuotes = (value) => {
    const trimmed = typeof value === 'string' ? value.trim() : '';
    if (trimmed.length >= 2
      && ((trimmed.startsWith('"') && trimmed.endsWith('"'))
        || (trimmed.startsWith("'") && trimmed.endsWith("'")))) {
      return trimmed.slice(1, -1).trim();
    }
    return trimmed;
  };

  /**
   * 解析用于拉起托管 omp host 的运行时（实际是 Bun，保留历史命名）。
   * 顺序：OMPCHAMBER_OMP_HOST_RUNTIME / OPENCODE_BINARY 显式指定（剥
   * 引号）→ PATH 搜索 bun → ~/.bun/bin 与 Homebrew/usr/local 常装位置。
   * 命中即写入 state.resolvedOpencodeBinarySource（env/path/fallback），
   * 全部失败返回 null。
   */
  const resolveOpencodeCliPath = () => {
    // Resolves the RUNTIME that launches the managed omp host (Bun), not an
    // opencode CLI. Kept under its historical name because the resolution
    // snapshot, PATH augmentation, and settings plumbing all flow through it.
    const explicit = [process.env.OMPCHAMBER_OMP_HOST_RUNTIME, process.env.OPENCODE_BINARY]
      .map(stripWrappingQuotes)
      .filter(Boolean);

    for (const candidate of explicit) {
      if (isExecutable(candidate)) {
        state.resolvedOpencodeBinarySource = 'env';
        return candidate;
      }
    }

    const resolvedFromPath = searchPathFor('bun');
    if (resolvedFromPath) {
      state.resolvedOpencodeBinarySource = 'path';
      return resolvedFromPath;
    }

    // bun 的常装位置兜底：用户目录优先，其次 Homebrew 与 /usr/local。
    const home = resolveHomeDir();
    const fallbacks = process.platform === 'win32'
      ? [path.join(home, '.bun', 'bin', 'bun.exe')]
      : [path.join(home, '.bun', 'bin', 'bun'), '/opt/homebrew/bin/bun', '/usr/local/bin/bun'];
    for (const candidate of fallbacks) {
      if (isExecutable(candidate)) {
        state.resolvedOpencodeBinarySource = 'fallback';
        return candidate;
      }
    }


    return null;
  };

  /**
   * 解析 node 可执行文件：NODE_BINARY / OMPCHAMBER_NODE_BINARY 显式
   * 指定 → PATH 搜索 → Unix 常装位置；Windows 再用 where node 兜底；
   * 仍找不到则依次用登录 shell 跑 command -v node（带超时）。
   * 全部失败返回 null。
   */
  const resolveNodeCliPath = () => {
    const explicit = [process.env.NODE_BINARY, process.env.OMPCHAMBER_NODE_BINARY]
      .map((v) => (typeof v === 'string' ? v.trim() : ''))
      .filter(Boolean);

    for (const candidate of explicit) {
      if (isExecutable(candidate)) {
        return candidate;
      }
    }

    const resolvedFromPath = searchPathFor('node');
    if (resolvedFromPath) {
      return resolvedFromPath;
    }

    // node 的 Unix 常见安装位置兜底。
    const unixFallbacks = ['/opt/homebrew/bin/node', '/usr/local/bin/node', '/usr/bin/node', '/bin/node'];
    for (const candidate of unixFallbacks) {
      if (isExecutable(candidate)) {
        return candidate;
      }
    }

    if (process.platform === 'win32') {
      try {
        const result = runSpawnSync('where', ['node'], {
          encoding: 'utf8',
          stdio: ['ignore', 'pipe', 'pipe'],
          windowsHide: true,
        });
        if (result.status === 0) {
          const lines = (result.stdout || '')
            .split(/\r?\n/)
            .map((line) => line.trim())
            .filter(Boolean);
          const found = lines.find((line) => isExecutable(line));
          if (found) return found;
        }
      } catch {
      }
      return null;
    }

    // 登录 shell 探测候选：command -v 只有在 rc 文件加载后才可见。
    const shells = [process.env.SHELL, '/bin/zsh', '/bin/bash', '/bin/sh'].filter(Boolean);
    for (const shell of shells) {
      if (!isExecutable(shell)) continue;
      try {
        const result = runSpawnSync(shell, ['-lic', 'command -v node'], {
          encoding: 'utf8',
          stdio: ['ignore', 'pipe', 'pipe'],
          windowsHide: true,
          timeout: SHELL_PROBE_TIMEOUT_MS,
        });
        if (result.status === 0) {
          const found = (result.stdout || '').trim().split(/\s+/).pop() || '';
          if (found && isExecutable(found)) {
            return found;
          }
        }
      } catch {
      }
    }

    return null;
  };

  /**
   * 解析 bun 可执行文件：BUN_BINARY / OMPCHAMBER_BUN_BINARY 显式指定
   * → PATH 搜索 → ~/.bun/bin 与 Unix 常装位置；Windows 再试
   * USERPROFILE 下 bun.exe/bun.cmd 与 where bun；最后用登录 shell 跑
   * command -v bun（带超时）。全部失败返回 null。
   */
  const resolveBunCliPath = () => {
    const explicit = [process.env.BUN_BINARY, process.env.OMPCHAMBER_BUN_BINARY]
      .map((v) => (typeof v === 'string' ? v.trim() : ''))
      .filter(Boolean);

    for (const candidate of explicit) {
      if (isExecutable(candidate)) {
        return candidate;
      }
    }

    const resolvedFromPath = searchPathFor('bun');
    if (resolvedFromPath) {
      return resolvedFromPath;
    }

    const home = os.homedir();
    // bun 的 Unix 常见安装位置兜底（用户目录优先）。
    const unixFallbacks = [
      path.join(home, '.bun', 'bin', 'bun'),
      '/opt/homebrew/bin/bun',
      '/usr/local/bin/bun',
      '/usr/bin/bun',
      '/bin/bun',
    ];
    for (const candidate of unixFallbacks) {
      if (isExecutable(candidate)) {
        return candidate;
      }
    }

    if (process.platform === 'win32') {
      const userProfile = process.env.USERPROFILE || home;
    // Windows 用户目录下的 bun 安装位置兜底。
      const winFallbacks = [
        path.join(userProfile, '.bun', 'bin', 'bun.exe'),
        path.join(userProfile, '.bun', 'bin', 'bun.cmd'),
      ];
      for (const candidate of winFallbacks) {
        if (isExecutable(candidate)) return candidate;
      }

      try {
        const result = runSpawnSync('where', ['bun'], {
          encoding: 'utf8',
          stdio: ['ignore', 'pipe', 'pipe'],
          windowsHide: true,
        });
        if (result.status === 0) {
          const lines = (result.stdout || '')
            .split(/\r?\n/)
            .map((line) => line.trim())
            .filter(Boolean);
          const found = lines.find((line) => isExecutable(line));
          if (found) return found;
        }
      } catch {
      }
      return null;
    }

    const shells = [process.env.SHELL, '/bin/zsh', '/bin/bash', '/bin/sh'].filter(Boolean);
    for (const shell of shells) {
      if (!isExecutable(shell)) continue;
      try {
        const result = runSpawnSync(shell, ['-lic', 'command -v bun'], {
          encoding: 'utf8',
          stdio: ['ignore', 'pipe', 'pipe'],
          windowsHide: true,
          timeout: SHELL_PROBE_TIMEOUT_MS,
        });
        if (result.status === 0) {
          const found = (result.stdout || '').trim().split(/\s+/).pop() || '';
          if (found && isExecutable(found)) {
            return found;
          }
        }
      } catch {
      }
    }

    return null;
  };

  /** 确保 bun 可用：命中即缓存到 state.resolvedBunBinary 并把其目录前置 PATH；找不到返回 null。 */
  const ensureBunCliEnv = () => {
    if (state.resolvedBunBinary) {
      return state.resolvedBunBinary;
    }

    const resolved = resolveBunCliPath();
    if (resolved) {
      prependToPath(path.dirname(resolved));
      state.resolvedBunBinary = resolved;
      return resolved;
    }

    return null;
  };

  /** 确保 node 可用：命中即缓存到 state.resolvedNodeBinary 并把其目录前置 PATH；找不到返回 null。 */
  const ensureNodeCliEnv = () => {
    if (state.resolvedNodeBinary) {
      return state.resolvedNodeBinary;
    }

    const resolved = resolveNodeCliPath();
    if (resolved) {
      prependToPath(path.dirname(resolved));
      state.resolvedNodeBinary = resolved;
      return resolved;
    }

    return null;
  };

  /** Windows 批处理包装脚本的扩展名集合（.cmd/.bat/.com）。 */
  const WINDOWS_BATCH_EXTENSIONS = new Set(['.cmd', '.bat', '.com']);

  /** 验证候选可执行路径：trim 后 Windows 走扩展名补全，其它平台直接验证；无效返回 null。 */
  const normalizeExecutableCandidate = (value) => {
    if (typeof value !== 'string') {
      return null;
    }
    const trimmed = value.trim();
    if (!trimmed) {
      return null;
    }
    if (process.platform === 'win32') {
      return resolveWindowsExecutablePath(trimmed);
    }
    return isExecutable(trimmed) ? trimmed : null;
  };

  /**
   * 返回当前 Windows 架构应查找的 opencode 原生包名（按优先级）。
   * arm64 暂用 x64-baseline/x64 规避上游 Bun FFI 问题；x64 优先
   * baseline 构建（无 AVX2 的主机也能直接跑原生二进制）。
   */
  const getWindowsNativeOpencodePackageNames = () => {
    // TEMPORARY WORKAROUND — Windows ARM64: native opencode.exe fails with a Bun
    // FFI/TinyCC dlopen error (https://github.com/anomalyco/opencode/issues/19130).
    // prepare-opencode-cli.mjs bundles x64-baseline instead; match that here so
    // the runtime resolver looks for the same x64-baseline package. Restore the
    // arm64 branch below when the upstream issue is resolved.
    if (process.arch === 'arm64') {
      // --- ORIGINAL (restore when ARM64 is fixed) ---
      // return ['opencode-windows-arm64'];
      return ['opencode-windows-x64-baseline', 'opencode-windows-x64'];
    }
    if (process.arch === 'x64') {
      // Prefer the baseline build when bypassing package-manager wrappers so the
      // direct binary still runs on hosts without AVX2 support.
      return ['opencode-windows-x64-baseline', 'opencode-windows-x64'];
    }
    return [];
  };

  /**
   * 在 node_modules 目录中定位 Windows 原生 opencode.exe：先试
   * opencode-ai/bin 的 shim，再按架构包名在两层位置（直接安装与
   * opencode-ai 的嵌套依赖）查找。命中返回路径，否则 null。
   */
  const resolveNativeOpencodeBinaryFromNodeModules = (nodeModulesDir) => {
    if (typeof nodeModulesDir !== 'string' || nodeModulesDir.trim().length === 0) {
      return null;
    }

    const packageShim = path.join(nodeModulesDir, 'opencode-ai', 'bin', 'opencode.exe');
    if (isExecutable(packageShim)) {
      return packageShim;
    }

    for (const packageName of getWindowsNativeOpencodePackageNames()) {
      const candidates = [
        path.join(nodeModulesDir, packageName, 'bin', 'opencode.exe'),
        path.join(nodeModulesDir, 'opencode-ai', 'node_modules', packageName, 'bin', 'opencode.exe'),
      ];
      for (const candidate of candidates) {
        if (isExecutable(candidate)) {
          return candidate;
        }
      }
    }

    return null;
  };

  /**
   * 把 node_modules 中的 opencode 启动脚本包装成 node 启动规格：
   * launcher（opencode-ai/bin/opencode）存在时，用解析到的 node 二进制
   * （兜底字符串 'node'）执行它，wrapperType 为 'node-launcher'；
   * launcher 不存在返回 null。
   */
  const resolveOpencodeNodeLaunchSpecFromNodeModules = (nodeModulesDir) => {
    if (typeof nodeModulesDir !== 'string' || nodeModulesDir.trim().length === 0) {
      return null;
    }

    const launcher = path.join(nodeModulesDir, 'opencode-ai', 'bin', 'opencode');
    if (!isExecutable(launcher) && !fs.existsSync(launcher)) {
      return null;
    }

    const nodeBinary = ensureNodeCliEnv() || resolveNodeCliPath() || 'node';
    return {
      binary: nodeBinary,
      args: [launcher],
      wrapperType: 'node-launcher',
    };
  };

  /**
   * 从 Windows 批处理包装脚本内容中提取 node_modules 目录：匹配脚本里
   * 引用的 opencode-ai/bin/opencode 路径并回推三层；读取失败或未匹配
   * 返回 null。
   */
  const resolveNodeModulesDirFromCmdWrapper = (wrapperPath) => {
    if (!wrapperPath || typeof wrapperPath !== 'string') {
      return null;
    }

    try {
      const content = fs.readFileSync(wrapperPath, 'utf8');
      const launcherMatch = content.match(/node_modules[\\/]+opencode-ai[\\/]+bin[\\/]+opencode/i);
      if (!launcherMatch) {
        return null;
      }

      const launcherPath = path.resolve(path.dirname(wrapperPath), launcherMatch[0].replace(/[\\/]+/g, path.sep));
      return path.dirname(path.dirname(path.dirname(launcherPath)));
    } catch {
      return null;
    }
  };

  /**
   * 依据 opencode 命令位置反推其 node_modules 目录。识别四类安装形态：
   * bun 全局安装、node_modules/.bin 链接、opencode-ai 包内直装、npm
   * 目录；批处理脚本则解析其内容。候选目录还需实际含有原生二进制或
   * node 启动脚本才算命中，否则 null。
   */
  const resolveOpencodeNodeModulesDir = (opencodePath) => {
    if (typeof opencodePath !== 'string' || opencodePath.trim().length === 0) {
      return null;
    }

    const normalized = path.resolve(opencodePath);
    const lower = normalized.toLowerCase();
    const fileDir = path.dirname(normalized);
    const nodeModulesCandidates = [];
    /** 去重地收集一个 node_modules 候选目录。 */
    const pushCandidate = (candidate) => {
      if (typeof candidate !== 'string' || candidate.trim().length === 0) {
        return;
      }
      if (!nodeModulesCandidates.includes(candidate)) {
        nodeModulesCandidates.push(candidate);
      }
    };

    if (lower.includes(`${path.sep}.bun${path.sep}bin${path.sep}opencode`)) {
      const bunRoot = path.dirname(path.dirname(normalized));
      pushCandidate(path.join(bunRoot, 'install', 'global', 'node_modules'));
    }

    if (lower.endsWith(`${path.sep}node_modules${path.sep}.bin${path.sep}opencode`)
      || lower.endsWith(`${path.sep}node_modules${path.sep}.bin${path.sep}opencode.cmd`)
      || lower.endsWith(`${path.sep}node_modules${path.sep}.bin${path.sep}opencode.bat`)
      || lower.endsWith(`${path.sep}node_modules${path.sep}.bin${path.sep}opencode.exe`)) {
      pushCandidate(path.dirname(fileDir));
    }

    if (lower.endsWith(`${path.sep}node_modules${path.sep}opencode-ai${path.sep}bin${path.sep}opencode`)) {
      pushCandidate(path.dirname(path.dirname(fileDir)));
    }

    if (path.basename(fileDir).toLowerCase() === 'npm') {
      pushCandidate(path.join(fileDir, 'node_modules'));
    }

    if (WINDOWS_BATCH_EXTENSIONS.has(path.extname(normalized).toLowerCase())) {
      pushCandidate(resolveNodeModulesDirFromCmdWrapper(normalized));
    }

    for (const candidate of nodeModulesCandidates) {
      if (resolveNativeOpencodeBinaryFromNodeModules(candidate) || resolveOpencodeNodeLaunchSpecFromNodeModules(candidate)) {
        return candidate;
      }
    }

    return null;
  };

  /**
   * 把（可能只是命令名的）opencode 路径解析成可直接 spawn 的启动规格
   * { binary, args, wrapperType }。非 Windows 直接透传。Windows 依次
   * 尝试：node_modules 中的原生二进制 → node 启动脚本 → shebang 指定
   * 的 node/bun 解释器 → 补全扩展名后的直执文件（批处理走 cmd /c
   * call）；批处理兜底也用 ComSpec 包装，绝不把 .cmd/.bat 直接交给
   * 无 shell 的 spawn。wrapperType 标记实际采用的包装方式（null 表示
   * 原样直执）。
   */
  const resolveManagedOpenCodeLaunchSpec = (opencodePath) => {
    const fallbackBinary = typeof opencodePath === 'string' && opencodePath.trim().length > 0
      ? opencodePath.trim()
      : 'opencode';

    if (process.platform !== 'win32') {
      return { binary: fallbackBinary, args: [], wrapperType: null };
    }

    const ext = path.extname(fallbackBinary).toLowerCase();
    const candidatePaths = [fallbackBinary];
    if (WINDOWS_BATCH_EXTENSIONS.has(ext)) {
      candidatePaths.push(fallbackBinary.slice(0, -ext.length) + '.exe');
    }

    for (const candidate of candidatePaths) {
      const nodeModulesDir = resolveOpencodeNodeModulesDir(candidate);
      const nativeBinary = resolveNativeOpencodeBinaryFromNodeModules(nodeModulesDir);
      if (nativeBinary) {
        return {
          binary: nativeBinary,
          args: [],
          wrapperType: nativeBinary === fallbackBinary ? null : 'native-wrapper',
        };
      }

      const nodeLaunchSpec = resolveOpencodeNodeLaunchSpecFromNodeModules(nodeModulesDir);
      if (nodeLaunchSpec) {
        return nodeLaunchSpec;
      }

      const interpreter = opencodeShimInterpreter(candidate);
      if (interpreter === 'node') {
        return {
          binary: ensureNodeCliEnv() || resolveNodeCliPath() || 'node',
          args: [candidate],
          wrapperType: 'node-shebang',
        };
      }
      if (interpreter === 'bun') {
        return {
          binary: ensureBunCliEnv() || resolveBunCliPath() || 'bun',
          args: [candidate],
          wrapperType: 'bun-shebang',
        };
      }

      const directBinary = normalizeExecutableCandidate(candidate);
      if (directBinary) {
        const directExt = path.extname(directBinary).toLowerCase();
        if (WINDOWS_BATCH_EXTENSIONS.has(directExt)) {
          return {
            binary: process.env.ComSpec || 'cmd.exe',
            args: ['/d', '/s', '/c', 'call', directBinary],
            wrapperType: 'cmd-wrapper',
          };
        }

        return {
          binary: directBinary,
          args: [],
          wrapperType: directBinary === fallbackBinary ? null : 'executable-wrapper',
        };
      }
    }

    // Final fallback: never hand a raw .cmd/.bat to spawn(shell:false) — cmd
    // shims need cmd.exe, and unquoted space-containing paths break there.
    if (WINDOWS_BATCH_EXTENSIONS.has(ext)) {
      return {
        binary: process.env.ComSpec || 'cmd.exe',
        args: ['/d', '/s', '/c', 'call', fallbackBinary],
        wrapperType: 'cmd-wrapper',
      };
    }

    return { binary: fallbackBinary, args: [], wrapperType: null };
  };

  /**
   * 读取文件首行的 shebang 解释器（#! 之后的内容）：只读前 256 字节，
   * 非 #! 开头或为空返回 null；任何 IO 异常也返回 null。
   */
  const readShebang = (opencodePath) => {
    if (!opencodePath || typeof opencodePath !== 'string') {
      return null;
    }
    try {
      const fd = fs.openSync(opencodePath, 'r');
      try {
        const buf = Buffer.alloc(256);
        const bytes = fs.readSync(fd, buf, 0, buf.length, 0);
        const head = buf.subarray(0, bytes).toString('utf8');
        const firstLine = head.split(/\r?\n/, 1)[0] || '';
        if (!firstLine.startsWith('#!')) {
          return null;
        }
        const shebang = firstLine.slice(2).trim();
        if (!shebang) {
          return null;
        }
        return shebang;
      } finally {
        try {
          fs.closeSync(fd);
        } catch {
        }
      }
    } catch {
      return null;
    }
  };

  /** 依据 shebang 判断 shim 脚本需要 node 还是 bun 解释器；无法识别返回 null。 */
  const opencodeShimInterpreter = (opencodePath) => {
    const shebang = readShebang(opencodePath);
    if (!shebang) return null;
    if (/\bnode\b/i.test(shebang)) return 'node';
    if (/\bbun\b/i.test(shebang)) return 'bun';
    return null;
  };

  /** 确保 shim 所需的解释器运行时就绪：node/bun 分别触发对应的 ensure*Env（解析并前置 PATH）。 */
  const ensureOpencodeShimRuntime = (opencodePath) => {
    const runtime = opencodeShimInterpreter(opencodePath);
    if (runtime === 'node') {
      ensureNodeCliEnv();
    }
    if (runtime === 'bun') {
      ensureBunCliEnv();
    }
  };



  /**
   * 确保 omp host 运行时可用并返回其路径。优先复用 state 缓存（同时确保
   * 其 shim 解释器就绪）；其次验证 OPENCODE_BINARY 环境变量；再走完整
   * 解析链，命中后写回 OPENCODE_BINARY、前置 PATH、标记来源并打印
   * 日志。全部失败返回 null。
   */
  const ensureOpencodeCliEnv = () => {
    if (state.resolvedOpencodeBinary) {
      ensureOpencodeShimRuntime(state.resolvedOpencodeBinary);
      return state.resolvedOpencodeBinary;
    }

    const existing = typeof process.env.OPENCODE_BINARY === 'string' ? process.env.OPENCODE_BINARY.trim() : '';
    if (existing && isExecutable(existing)) {
      state.resolvedOpencodeBinary = existing;
      state.resolvedOpencodeBinarySource = state.resolvedOpencodeBinarySource || 'env';
      prependToPath(path.dirname(existing));
      ensureOpencodeShimRuntime(existing);
      return existing;
    }

    const resolved = resolveOpencodeCliPath();
    if (resolved) {
      process.env.OPENCODE_BINARY = resolved;
      prependToPath(path.dirname(resolved));
      ensureOpencodeShimRuntime(resolved);
      state.resolvedOpencodeBinary = resolved;
      state.resolvedOpencodeBinarySource = state.resolvedOpencodeBinarySource || 'unknown';
      console.log(`Resolved omp host runtime: ${resolved}`);
      return resolved;
    }

    return null;
  };

  /**
   * 解析用于 spawn 的 git 可执行文件。非 Windows 直接用 'git'；Windows：
   * 显式 GIT_BINARY/OMPCHAMBER_GIT_BINARY → PATH 搜索 git/git.exe →
   * Program Files 等常见安装位置（候选剥包裹引号），优先 .exe 结尾的
   * 候选，结果缓存到 state.resolvedGitBinary，最终兜底 'git.exe'。
   */
  const resolveGitBinaryForSpawn = () => {
    if (process.platform !== 'win32') {
      return 'git';
    }

    if (state.resolvedGitBinary) {
      return state.resolvedGitBinary;
    }

    const explicit = [process.env.GIT_BINARY, process.env.OMPCHAMBER_GIT_BINARY]
      .map((value) => (typeof value === 'string' ? value.trim() : ''))
      .filter(Boolean);
    for (const candidate of explicit) {
      if (isExecutable(candidate)) {
        state.resolvedGitBinary = candidate;
        return state.resolvedGitBinary;
      }
    }

    // 收集到的可用 git 候选（PATH 与常见安装位置）。
    const candidates = [];
    /** 剥引号后验证候选；无论可执行与否都返回 trim 后的值（不可执行的由调用方继续收集）。 */
    const normalizeGitCandidate = (candidate) => {
      if (typeof candidate !== 'string') {
        return '';
      }
      const trimmed = stripWrappingQuotes(candidate);
      if (trimmed && isExecutable(trimmed)) {
        return trimmed;
      }
      return trimmed;
    };

    const pathCandidate = normalizeGitCandidate(searchPathFor('git'));
    if (pathCandidate && isExecutable(pathCandidate)) {
      candidates.push(pathCandidate);
    }

    const pathExeCandidate = normalizeGitCandidate(searchPathFor('git.exe'));
    if (pathExeCandidate && isExecutable(pathExeCandidate)) {
      candidates.push(pathExeCandidate);
    }

    // Windows 常见 Git 安装根目录。
    const programRoots = [
      process.env.ProgramFiles,
      process.env['ProgramFiles(x86)'],
      process.env.LocalAppData,
    ]
      .map((value) => (typeof value === 'string' ? value.trim() : ''))
      .filter(Boolean);
    for (const root of programRoots) {
      const installCandidates = [
        path.join(root, 'Git', 'cmd', 'git.exe'),
        path.join(root, 'Git', 'bin', 'git.exe'),
        path.join(root, 'Git', 'mingw64', 'bin', 'git.exe'),
        path.join(root, 'Programs', 'Git', 'cmd', 'git.exe'),
        path.join(root, 'Programs', 'Git', 'bin', 'git.exe'),
      ];
      for (const candidate of installCandidates) {
        const normalized = normalizeGitCandidate(candidate);
        if (normalized && isExecutable(normalized)) {
          candidates.push(normalized);
        }
      }
    }

    const preferredExe = candidates.find((candidate) => candidate.toLowerCase().endsWith('.exe'));
    state.resolvedGitBinary = preferredExe || candidates[0] || 'git.exe';
    return state.resolvedGitBinary;
  };

  /** 清空已缓存的 omp host 运行时路径（如运行时被移动/卸载后强制重解析）。 */
  const clearResolvedOpenCodeBinary = () => {
    state.resolvedOpencodeBinary = null;
  };

  // 导出的运行环境方法集合：登录 shell 环境并入、各运行时二进制解析、
  // Windows 启动规格解析与缓存清理。
  return {
    applyLoginShellEnvSnapshot,
    ensureOpencodeCliEnv,
    getLoginShellEnvSnapshot,
    resolveOpencodeCliPath,
    isBundledOpenCodeCliPath,
    resolveManagedOpenCodeLaunchSpec,
    isExecutable,
    searchPathFor,
    resolveGitBinaryForSpawn,
    clearResolvedOpenCodeBinary,
  };
};
