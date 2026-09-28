/**
 * 终端 shell 发现与解析：维护受支持的 shell id 白名单，
 * 从环境变量、/etc/shells 与增强 PATH 中枚举可用 shell，
 * 并把用户的 shell 偏好解析为可执行文件列表（供 PTY spawn 使用）。
 */
/** 受支持的 shell id 白名单（小写、稳定）：客户端枚举与服务端参数校验共用。 */
const TERMINAL_SHELL_IDS = ['bash', 'zsh', 'sh', 'fish', 'pwsh', 'powershell', 'cmd', 'dash', 'ksh', 'nu'];
/** 由白名单构建的 Set 视图，供 O(1) 成员判断。 */
const TERMINAL_SHELL_ID_SET = new Set(TERMINAL_SHELL_IDS);

/** 规范化 shell 偏好：trim+小写后必须为 'auto' 或白名单 id，否则返回 null（用于 create 参数校验）。 */
export const normalizeTerminalShell = (value) => {
  if (typeof value !== 'string') return null;
  const normalized = value.trim().toLowerCase();
  return normalized === 'auto' || TERMINAL_SHELL_ID_SET.has(normalized) ? normalized : null;
};

/** 从可执行文件路径推断 shell id：取文件名、去 .exe 后缀、小写后查白名单；不认识返回 null。 */
const shellIdFromPath = (value) => {
  const filename = String(value || '').replace(/\\/g, '/').split('/').pop()?.toLowerCase() || '';
  const id = filename.endsWith('.exe') ? filename.slice(0, -4) : filename;
  return TERMINAL_SHELL_ID_SET.has(id) ? id : null;
};

/** 个别 shell 的展示名映射（其余 shell 直接用 id 作为名称）。 */
const SHELL_LABELS = { pwsh: 'PowerShell', powershell: 'Windows PowerShell', cmd: 'Command Prompt' };
/** shell id -> 用户可见展示名。 */
const shellLabel = (id) => SHELL_LABELS[id] ?? id;

/** 返回登录模式启动参数：bash/zsh/ksh 用 -l，fish/nu 用 --login，非 Windows 的 pwsh 用 -Login；不支持登录的 shell 返回 null。 */
export const getTerminalShellLoginArgs = (executable, platform = process.platform) => {
  const id = shellIdFromPath(executable);
  if (id === 'bash' || id === 'zsh' || id === 'ksh') return ['-l'];
  if (id === 'fish' || id === 'nu') return ['--login'];
  if (id === 'pwsh' && platform !== 'win32') return ['-Login'];
  return null;
};

/**
 * 构建 shell 解析器（list/resolve），依赖全部注入便于测试替换。
 * @param {object} deps fs/path 抽象、searchPathFor/isExecutable（PATH 检索与可执行判断）、
 *   buildAugmentedPath（增强 PATH，让 PTY 环境内新装的 shell 也能被发现）、
 *   platform/env（默认取 process.platform/process.env）
 * @returns {{ list: () => Promise<Array<{id:string,name:string,executable:string|null,supportsLogin:boolean}>>,
 *             resolve: (preference?: string) => Promise<{id:string, executables:string[]}> }}
 */
export const createTerminalShellResolver = ({ fs, path, searchPathFor, isExecutable, buildAugmentedPath = () => env.PATH || '', platform = process.platform, env = process.env }) => {
  /** 单个候选解析：含路径分隔符按原值、否则搜增强 PATH；最终以 isExecutable 为准，失败返回 null。 */
  const resolveExecutable = (candidate) => {
    if (!candidate) return null;
    const value = String(candidate);
    const found = value.includes('/') || value.includes('\\') ? value : searchPathFor(value, buildAugmentedPath());
    if (found && isExecutable(found)) return found;
    return isExecutable(value) ? value : null;
  };

  /** 平台默认候选序列：环境覆盖（OMPCHAMBER_TERMINAL_SHELL/SHELL）优先；Windows 落到系统 PowerShell/pwsh/cmd。 */
  const defaultCandidates = () => platform === 'win32'
    ? [
        env.OMPCHAMBER_TERMINAL_SHELL,
        env.SHELL,
        env.ComSpec,
        path.join(env.SystemRoot || 'C:\\Windows', 'System32', 'WindowsPowerShell', 'v1.0', 'powershell.exe'),
        'pwsh.exe',
        'powershell.exe',
        'cmd.exe',
      ]
    : [env.OMPCHAMBER_TERMINAL_SHELL, env.SHELL, '/bin/zsh', '/bin/bash', '/bin/sh', 'zsh', 'bash', 'sh'];

  /** 批量解析候选并按解析结果去重（保序），返回全部真实可执行的路径。 */
  const resolveCandidates = (candidates) => {
    const seen = new Set();
    return candidates
      .map(resolveExecutable)
      .filter((candidate) => candidate && !seen.has(candidate) && seen.add(candidate));
  };

  /** 枚举可用 shell：Windows 用默认候选+全部已知 id 探测；POSIX 额外读取 /etc/shells；首项恒为 'auto'（含默认解析出的可执行文件与登录支持）。 */
  const list = async () => {
    let configuredShells = [];
    if (platform !== 'win32') {
      try {
        const contents = await fs.promises.readFile('/etc/shells', 'utf8');
        configuredShells = contents
          .split(/\r?\n/)
          .map((line) => line.trim())
          .filter((line) => line && !line.startsWith('#'));
      } catch {
        // PATH and platform defaults below remain authoritative fallbacks.
      }
    }

    const candidates = platform === 'win32'
      ? [...defaultCandidates(), ...TERMINAL_SHELL_IDS]
      : [env.OMPCHAMBER_TERMINAL_SHELL, env.SHELL, ...configuredShells, ...TERMINAL_SHELL_IDS, '/bin/zsh', '/bin/bash', '/bin/sh'];
    const autoExecutable = resolveCandidates(defaultCandidates())[0] ?? null;
    const byId = new Map([
      ['auto', {
        id: 'auto',
        name: 'Auto',
        executable: autoExecutable,
        supportsLogin: Boolean(autoExecutable && getTerminalShellLoginArgs(autoExecutable, platform)),
      }],
    ]);
    for (const executable of resolveCandidates(candidates)) {
      const id = shellIdFromPath(executable);
      if (id && !byId.has(id)) {
        byId.set(id, { id, name: shellLabel(id), executable, supportsLogin: Boolean(getTerminalShellLoginArgs(executable, platform)) });
      }
    }
    return [...byId.values()];
  };

  /** 把偏好解析为可执行文件列表：'auto' 返回默认候选全序（逐个尝试）；指定 id 必须能在 list 中找到，否则抛错。 */
  const resolve = async (preference) => {
    const normalized = normalizeTerminalShell(preference ?? 'auto');
    if (!normalized) throw new Error('Invalid terminal shell');
    if (normalized === 'auto') return { id: 'auto', executables: resolveCandidates(defaultCandidates()) };
    const selected = (await list()).find((shell) => shell.id === normalized);
    if (!selected) throw new Error(`Terminal shell "${normalized}" is not available`);
    return { id: selected.id, executables: [selected.executable] };
  };

  return { list, resolve };
};
