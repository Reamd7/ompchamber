/**
 * cloudflared 隧道封装。提供三种建隧道方式：quick tunnel（免账号的
 * trycloudflare 临时域名）、managed remote（凭 Cloudflare tunnel token 运行）、
 * managed local（凭本地 cloudflared 配置文件运行）。统一负责二进制可用性检查、
 * 子进程启动（默认关闭 Cloudflare 遥测）、从日志提取公网地址与就绪/致命错误信号、
 * 临时文件清理，并返回带 stop()/getPublicUrl() 的句柄。
 */
import { spawn, spawnSync } from 'child_process';
import fs from 'fs';
import os from 'os';
import path from 'path';
import { fileURLToPath } from 'url';
import yaml from 'yaml';
import {
  createExecutableSearchEnv,
  resolveExecutableLaunchTarget,
} from './tunnels/executable-search.js';

/** ESM 环境下还原 CommonJS 的 __filename（当前源文件绝对路径）。 */
const __filename = fileURLToPath(import.meta.url);
/** ESM 环境下还原 CommonJS 的 __dirname（当前源文件所在目录）。 */
const __dirname = path.dirname(__filename);

/** 从 quick tunnel 输出中捕获 https://<随机子域>.trycloudflare.com 公网地址的正则。 */
const TRY_CF_URL_REGEX = /https:\/\/[a-z0-9-]+\.trycloudflare\.com/i;

/** quick tunnel 等待公网 URL 出现的启动超时（30s）。 */
const DEFAULT_STARTUP_TIMEOUT_MS = 30000;
/** managed tunnel 等待就绪的硬超时（20s）。 */
const MANAGED_TUNNEL_STARTUP_TIMEOUT_MS = 20000;
/** 已见到输出但始终没有就绪日志时的活性兜底时限（6s），到点按成功放行。 */
const MANAGED_TUNNEL_LIVENESS_FALLBACK_MS = 6000;
/** 隧道模式标识：免账号的 quick tunnel。 */
const TUNNEL_MODE_QUICK = 'quick';
/** 隧道模式标识：凭 token 运行的远端托管隧道。 */
const TUNNEL_MODE_MANAGED_REMOTE = 'managed-remote';
/** 隧道模式标识：凭本地 cloudflared 配置文件运行的托管隧道。 */
const TUNNEL_MODE_MANAGED_LOCAL = 'managed-local';

/**
 * 检查 cloudflared 是否已安装且可执行：经 executable-search 解析启动目标后同步
 * 运行 --version。返回 { available, path, version }；不可用时 path/version 为 null。
 */
export async function checkCloudflaredAvailable() {
  const target = resolveExecutableLaunchTarget('cloudflared');
  if (target) {
    try {
      const result = spawnSync(target.command, ['--version'], {
        encoding: 'utf8',
        stdio: ['pipe', 'pipe', 'pipe'],
        windowsHide: true,
        env: target.env,
      });
      if (result.status === 0) {
        return { available: true, path: target.command, version: result.stdout.trim() };
      }
    } catch {
      // Ignore
    }
  }
  return { available: false, path: null, version: null };
}

/** 在终端打印 cloudflared 的分平台安装指引（macOS brew / Windows winget / Linux 从 releases 下载）。 */
function printCloudflareTunnelInstallHelp() {
  const platform = process.platform;
  let installCmd = '';

  if (platform === 'darwin') {
    installCmd = 'brew install cloudflared';
  } else if (platform === 'win32') {
    installCmd = 'winget install --id Cloudflare.cloudflared';
  } else {
    installCmd = 'Download from https://github.com/cloudflare/cloudflared/releases';
  }

  console.log(`
╔══════════════════════════════════════════════════════════════════╗
║  Cloudflare tunnel requires 'cloudflared' to be installed        ║
╚══════════════════════════════════════════════════════════════════╝

Install instructions for your platform:

  macOS:    brew install cloudflared
  Windows:  winget install --id Cloudflare.cloudflared
  Linux:    Download from https://github.com/cloudflare/cloudflared/releases

Or visit: https://developers.cloudflare.com/cloudflare-one/networks/connectors/cloudflare-tunnel/downloads/
`);
}

/**
 * 以统一选项启动 cloudflared 子进程：忽略 stdin、管道捕获输出、Windows 隐藏窗口，
 * 注入可执行搜索 PATH 并默认关闭 CF 遥测（CF_TELEMETRY_DISABLE=1），终止信号用
 * SIGINT。envOverrides 可覆盖 HOME 等变量（quick tunnel 用临时 HOME 隔离凭据）。
 */
const spawnCloudflared = (args, envOverrides = {}, resolvedBinaryPath = 'cloudflared') => spawn(resolvedBinaryPath, args, {
  stdio: ['ignore', 'pipe', 'pipe'],
  windowsHide: true,
  env: {
    ...createExecutableSearchEnv(),
    CF_TELEMETRY_DISABLE: '1',
    ...envOverrides,
  },
  killSignal: 'SIGINT',
});

/**
 * 把用户输入归一为合法主机名：可带协议前缀，取 URL hostname 后转小写；
 * 含通配符（*）的主机或解析失败返回 null。
 */
const normalizeHostname = (value) => {
  if (typeof value !== 'string') {
    return null;
  }
  const trimmed = value.trim();
  if (!trimmed) {
    return null;
  }
  try {
    const parsed = trimmed.includes('://') ? new URL(trimmed) : new URL(`https://${trimmed}`);
    const hostname = parsed.hostname.trim().toLowerCase();
    if (!hostname || hostname.includes('*')) {
      return null;
    }
    return hostname;
  } catch {
    return null;
  }
};

/** normalizeHostname 的导出包装，供外部校验隧道 hostname 输入。 */
export function normalizeCloudflareTunnelHostname(value) {
  return normalizeHostname(value);
}

/**
 * 探测 Cloudflare API（api.trycloudflare.com）是否可达，用于区分"网络不通"与
 * "cloudflared 配置错误"。可注入 fetchImpl 与超时（默认 5s）。返回
 * { reachable, status, error }；运行时没有可用的 fetch 时直接报不可达。
 */
export async function checkCloudflareApiReachability({ fetchImpl = globalThis.fetch, timeoutMs = 5000 } = {}) {
  if (typeof fetchImpl !== 'function') {
    return {
      reachable: false,
      status: null,
      error: 'Fetch API is unavailable in this runtime.',
    };
  }

  const controller = new AbortController();
  const timeout = setTimeout(() => controller.abort(), timeoutMs);
  try {
    const response = await fetchImpl('https://api.trycloudflare.com/', {
      method: 'GET',
      signal: controller.signal,
    });
    return {
      reachable: true,
      status: response.status,
      error: null,
    };
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error);
    return {
      reachable: false,
      status: null,
      error: message,
    };
  } finally {
    clearTimeout(timeout);
  }
}

/** cloudflared 输出中表示隧道已注册成功/连上 edge 的就绪日志模式集合。 */
const READY_LOG_PATTERNS = [
  /registered tunnel connection/i,
  /connection[^\n]*registered/i,
  /starting metrics server/i,
  /connected to edge/i,
];

/** 本地配置文件允许的最大字节数（256KB），防止读入超大文件。 */
const MANAGED_LOCAL_CONFIG_MAX_BYTES = 256 * 1024;
/** 本地隧道配置允许的扩展名白名单（.yml/.yaml/.json）。 */
const MANAGED_LOCAL_CONFIG_ALLOWED_EXTENSIONS = new Set(['.yml', '.yaml', '.json']);

/** cloudflared 输出中表示启动注定失败的致命错误模式（配置解析失败、token 无效等）。 */
const FATAL_LOG_PATTERNS = [
  /error parsing.*config/i,
  /failed to .*config/i,
  /invalid token/i,
  /unauthorized/i,
  /credentials file .* not found/i,
  /provided tunnel credentials are invalid/i,
];

/** 判断一行 cloudflared 日志是否命中就绪模式（见 READY_LOG_PATTERNS）。 */
function isCloudflaredReadyLogLine(line) {
  if (!line) {
    return false;
  }
  return READY_LOG_PATTERNS.some((pattern) => pattern.test(line));
}

/** 判断一行 cloudflared 日志是否命中致命错误模式（见 FATAL_LOG_PATTERNS）。 */
function isCloudflaredFatalLogLine(line) {
  if (!line) {
    return false;
  }
  return FATAL_LOG_PATTERNS.some((pattern) => pattern.test(line));
}

/**
 * 校验用户提供的配置文件可安全读取：必须存在且是普通文件、扩展名在白名单内、
 * 非空、不超过大小上限，且当前进程有读权限。任一检查失败都抛出带用户可读提示
 * 的 Error，contextLabel 用于拼进消息指明是哪类文件。
 */
function assertReadableFile(filePath, contextLabel) {
  let stats;
  try {
    stats = fs.statSync(filePath);
  } catch {
    throw new Error(`${contextLabel} file was not found. Select a valid cloudflared config file.`);
  }

  if (!stats.isFile()) {
    throw new Error(`${contextLabel} path is not a file. Select a cloudflared config file.`);
  }

  const extension = path.extname(filePath).toLowerCase();
  if (!MANAGED_LOCAL_CONFIG_ALLOWED_EXTENSIONS.has(extension)) {
    throw new Error(`${contextLabel} must be a .yml, .yaml, or .json file.`);
  }

  if (stats.size <= 0) {
    throw new Error(`${contextLabel} file is empty.`);
  }
  if (stats.size > MANAGED_LOCAL_CONFIG_MAX_BYTES) {
    throw new Error(`${contextLabel} file is too large (max ${MANAGED_LOCAL_CONFIG_MAX_BYTES} bytes).`);
  }

  try {
    fs.accessSync(filePath, fs.constants.R_OK);
  } catch {
    throw new Error(`${contextLabel} file is not readable. Check file permissions and try again.`);
  }
}

/**
 * 解析 cloudflared 配置文件并返回 { hostname, parseError }：取 ingress 规则中
 * 第一个可归一化的 hostname。读取失败与 YAML/JSON 解析失败都转为 parseError
 * 而非抛出，供调用方区分"文件坏了"与"文件里没有 hostname"。
 */
function extractHostnameFromCloudflaredConfigDetailed(configPath) {
  if (typeof configPath !== 'string' || configPath.trim().length === 0) {
    return { hostname: null, parseError: null };
  }

  let raw;
  try {
    raw = fs.readFileSync(configPath, 'utf8');
  } catch {
    return {
      hostname: null,
      parseError: new Error('Could not read the managed local tunnel config file. Check that the file exists and is accessible.'),
    };
  }

  let parsed;
  try {
    parsed = yaml.parse(raw);
  } catch {
    return {
      hostname: null,
      parseError: new Error('Managed local tunnel config is invalid. Use a valid cloudflared YAML/JSON config file.'),
    };
  }

  const ingress = Array.isArray(parsed?.ingress) ? parsed.ingress : [];
  for (const rule of ingress) {
    const hostname = normalizeHostname(rule?.hostname);
    if (hostname) {
      return { hostname, parseError: null };
    }
  }

  return { hostname: null, parseError: null };
}

/** 仅取 hostname 字段的简便封装；解析出错时得到 null。 */
const extractHostnameFromCloudflaredConfig = (configPath) => {
  return extractHostnameFromCloudflaredConfigDetailed(configPath).hostname;
};

/** cloudflared 的默认配置路径：~/.cloudflared/config.yml。 */
const getDefaultCloudflaredConfigPath = () => path.join(os.homedir(), '.cloudflared', 'config.yml');

/**
 * 预检 managed local 隧道配置（不抛错）：未显式传 configPath 时改用默认路径。
 * 依次校验文件可读（assertReadableFile）与可解析（提取 ingress hostname）；
 * hostname 优先取入参的归一化值，其次取配置文件内的值，两者皆无则报缺失。
 * 全部通过返回 { ok: true, effectiveConfigPath, resolvedHostname }，
 * 否则返回 { ok: false, effectiveConfigPath, resolvedHostname: null, error }。
 */
export function inspectManagedLocalCloudflareConfig({ configPath, hostname } = {}) {
  const requestedPath = typeof configPath === 'string' ? configPath.trim() : '';
  const effectiveConfigPath = requestedPath || getDefaultCloudflaredConfigPath();

  try {
    if (requestedPath) {
      assertReadableFile(effectiveConfigPath, 'Managed local tunnel config');
    } else {
      assertReadableFile(effectiveConfigPath, 'Managed local tunnel default config');
    }
  } catch (error) {
    return {
      ok: false,
      effectiveConfigPath,
      resolvedHostname: null,
      error: error instanceof Error ? error.message : String(error),
    };
  }

  const configHostnameResult = extractHostnameFromCloudflaredConfigDetailed(effectiveConfigPath);
  if (configHostnameResult.parseError) {
    return {
      ok: false,
      effectiveConfigPath,
      resolvedHostname: null,
      error: configHostnameResult.parseError.message,
    };
  }

  const resolvedHostname = normalizeHostname(hostname) || configHostnameResult.hostname;
  if (!resolvedHostname) {
    return {
      ok: false,
      effectiveConfigPath,
      resolvedHostname: null,
      error: 'Managed local tunnel hostname is required (set --hostname or include ingress hostname in config).',
    };
  }

  return {
    ok: true,
    effectiveConfigPath,
    resolvedHostname,
    error: null,
  };
}

/**
 * 等待 managed 隧道子进程就绪：监听 stdout/stderr，命中就绪日志即 resolve；
 * 命中致命错误日志或进程直接退出则 reject。6s 内只要出现过任何输出就按
 * "进程活着"兜底 resolve（部分版本不打印标准就绪文案）；20s 硬超时 reject。
 * settle 只生效一次，结束时统一解绑监听并清理两个定时器。
 */
async function waitForManagedTunnelReady(child, { modeLabel }) {
  await new Promise((resolve, reject) => {
    let settled = false;
    let sawOutput = false;

    /** 首次 settle 时统一收尾：清掉两个定时器、解绑全部监听后 resolve/reject；重复调用无效。 */
    const finish = (handler, value) => {
      if (settled) {
        return;
      }
      settled = true;
      clearTimeout(fallbackTimer);
      clearTimeout(hardTimeout);
      child.stdout?.off('data', onStdout);
      child.stderr?.off('data', onStderr);
      child.off('exit', onExit);
      handler(value);
    };

    /** 检查一段子进程输出：记录"已见到输出"并逐行判断就绪/致命日志。 */
    const inspectChunk = (chunk) => {
      const text = chunk.toString('utf8');
      if (text.trim().length > 0) {
        sawOutput = true;
      }
      const lines = text.split(/\r?\n/).map((line) => line.trim()).filter(Boolean);
      for (const line of lines) {
        if (isCloudflaredReadyLogLine(line)) {
          finish(resolve, null);
          return;
        }
        if (isCloudflaredFatalLogLine(line)) {
          finish(reject, new Error(`Cloudflared failed to start ${modeLabel}: ${line}`));
          return;
        }
      }
    };

    /** stdout 数据监听：交给 inspectChunk 判断。 */
    const onStdout = (chunk) => {
      inspectChunk(chunk);
    };

    /** stderr 数据监听：交给 inspectChunk 判断。 */
    const onStderr = (chunk) => {
      inspectChunk(chunk);
    };

    /** 进程退出监听：启动阶段退出即视为失败并 reject。 */
    const onExit = (code) => {
      finish(reject, new Error(`Cloudflared exited while starting ${modeLabel} (code ${code ?? 'unknown'})`));
    };

    child.stdout?.on('data', onStdout);
    child.stderr?.on('data', onStderr);
    child.once('exit', onExit);

    const fallbackTimer = setTimeout(() => {
      if (sawOutput) {
        finish(resolve, null);
      }
    }, MANAGED_TUNNEL_LIVENESS_FALLBACK_MS);

    const hardTimeout = setTimeout(() => {
      finish(reject, new Error(`Timed out waiting for cloudflared to initialize ${modeLabel}. Check your tunnel config and credentials.`));
    }, MANAGED_TUNNEL_STARTUP_TIMEOUT_MS);
  });
}

/**
 * 启动免账号的 quick tunnel：cloudflared tunnel --url <originUrl>。
 * 用临时 HOME 隔离凭据，从输出中抓取 trycloudflare.com 公网 URL；30s 内未拿到
 * URL、进程出错或异常退出时杀掉进程、清理临时目录并抛错。成功后返回
 * { mode, stop, process, getPublicUrl }——getPublicUrl 在捕获到 URL 前为 null。
 * 注意 quick tunnel 的 URL 是临时的，进程停止即失效。
 */
export async function startCloudflareQuickTunnel({ originUrl }) {
  const cfCheck = await checkCloudflaredAvailable();

  if (!cfCheck.available) {
    printCloudflareTunnelInstallHelp();
    throw new Error('cloudflared is not installed');
  }

  console.log(`Using cloudflared: ${cfCheck.path} (${cfCheck.version})`);

  const tempDir = fs.mkdtempSync(path.join(os.tmpdir(), 'ompchamber-cf-'));

  const child = spawnCloudflared(['tunnel', '--url', originUrl], { HOME: tempDir }, cfCheck.path);

  let publicUrl = null;
  let tunnelReady = false;

  /** 输出监听：未就绪时从文本抓取 trycloudflare 公网 URL；stderr 原样透传。 */
  const onData = (chunk, isStderr) => {
    const text = chunk.toString('utf8');

    if (!tunnelReady) {
      const match = text.match(TRY_CF_URL_REGEX);
      if (match) {
        publicUrl = match[0];
        tunnelReady = true;
      }
    }

    process.stderr.write(isStderr ? text : '');
  };

  child.stdout.on('data', (chunk) => onData(chunk, false));
  child.stderr.on('data', (chunk) => onData(chunk, true));

  child.on('error', (error) => {
    console.error(`Cloudflared error: ${error.message}`);
    cleanupTempDir();
  });

  /** 删除 quick tunnel 使用的临时 HOME 目录；失败静默忽略。 */
  const cleanupTempDir = () => {
    try {
      if (fs.existsSync(tempDir)) {
        fs.rmSync(tempDir, { recursive: true, force: true });
      }
    } catch {
      // Ignore cleanup errors
    }
  };

  await new Promise((resolve, reject) => {
    const timeout = setTimeout(() => {
      if (!publicUrl) {
        try { child.kill('SIGINT'); } catch { /* ignore */ }
        cleanupTempDir();
        reject(new Error('Tunnel URL not received within 30 seconds'));
      }
    }, DEFAULT_STARTUP_TIMEOUT_MS);

    const checkReady = setInterval(() => {
      if (publicUrl) {
        clearTimeout(timeout);
        clearInterval(checkReady);
        resolve(null);
      }
    }, 100);

    child.on('exit', (code) => {
      clearTimeout(timeout);
      clearInterval(checkReady);
      cleanupTempDir();
      if (code !== null && code !== 0) {
        reject(new Error(`Cloudflared exited with code ${code}`));
      }
    });
  });

  return {
    mode: TUNNEL_MODE_QUICK,
    stop: () => {
      try {
        child.kill('SIGINT');
      } catch {
        // Ignore
      }
    },
    process: child,
    getPublicUrl: () => publicUrl,
  };
}

/**
 * 启动凭 Cloudflare tunnel token 运行的 managed 隧道。token 优先取
 * tokenFilePath 指定的文件；否则把 token 写入 0o600 权限的临时文件，进程
 * 出错/退出时负责删除。stdout 仅排空不落日志（避免泄露敏感输出），stderr 原样
 * 透传。经 waitForManagedTunnelReady 确认就绪后返回句柄，publicUrl 即
 * https://<hostname>。
 */
export async function startCloudflareManagedRemoteTunnel({ token, hostname, tokenFilePath }) {
  const cfCheck = await checkCloudflaredAvailable();

  if (!cfCheck.available) {
    printCloudflareTunnelInstallHelp();
    throw new Error('cloudflared is not installed');
  }

  const normalizedToken = typeof token === 'string' ? token.trim() : '';
  const normalizedHost = typeof hostname === 'string' ? hostname.trim().toLowerCase() : '';

  if (!normalizedToken) {
    throw new Error('Managed remote tunnel token is required');
  }
  if (!normalizedHost) {
    throw new Error('Managed remote tunnel hostname is required');
  }

  let effectiveTokenFilePath = typeof tokenFilePath === 'string' ? tokenFilePath : null;
  let tempTokenFile = null;

  if (!effectiveTokenFilePath) {
    const tempDir = fs.mkdtempSync(path.join(os.tmpdir(), 'ompchamber-cf-token-'));
    effectiveTokenFilePath = path.join(tempDir, 'token');
    fs.writeFileSync(effectiveTokenFilePath, normalizedToken, { encoding: 'utf8', mode: 0o600 });
    tempTokenFile = { dir: tempDir, path: effectiveTokenFilePath };
  }

  const child = spawnCloudflared(['tunnel', 'run', '--token-file', effectiveTokenFilePath], {}, cfCheck.path);
  const publicUrl = `https://${normalizedHost}`;

  child.stdout.on('data', () => {
    // Keep stream drained, but avoid logging potentially sensitive output.
  });

  child.stderr.on('data', (chunk) => {
    const text = chunk.toString('utf8');
    process.stderr.write(text);
  });

  /** 删除 token 临时文件及其所在目录；失败静默忽略。 */
  const cleanupTempTokenFile = () => {
    if (tempTokenFile) {
      try {
        if (fs.existsSync(tempTokenFile.dir)) {
          fs.rmSync(tempTokenFile.dir, { recursive: true, force: true });
        }
      } catch {
        // Ignore cleanup errors
      }
    }
  };

  child.on('error', (error) => {
    console.error(`Cloudflared error: ${error.message}`);
    cleanupTempTokenFile();
  });

  child.on('exit', () => {
    cleanupTempTokenFile();
  });

  try {
    await waitForManagedTunnelReady(child, { modeLabel: 'managed-remote tunnel' });
  } catch (error) {
    try { child.kill('SIGINT'); } catch { /* ignore */ }
    cleanupTempTokenFile();
    throw error;
  }

  return {
    mode: TUNNEL_MODE_MANAGED_REMOTE,
    stop: () => {
      try {
        child.kill('SIGINT');
      } catch {
        // Ignore
      }
      cleanupTempTokenFile();
    },
    process: child,
    getPublicUrl: () => publicUrl,
  };
}

/**
 * 启动凭本地 cloudflared 配置运行的 managed 隧道：configPath 缺省时用默认路径，
 * 复用与 inspect 相同的校验（assertReadableFile + 提取 ingress hostname），
 * hostname 入参优先于配置内值；显式传入路径时向 cloudflared 传 --config。
 * 就绪等待与句柄语义同 remote 版本，额外暴露 getResolvedHostname 与
 * getEffectiveConfigPath。
 */
export async function startCloudflareManagedLocalTunnel({ configPath, hostname }) {
  const cfCheck = await checkCloudflaredAvailable();

  if (!cfCheck.available) {
    printCloudflareTunnelInstallHelp();
    throw new Error('cloudflared is not installed');
  }

  const requestedPath = typeof configPath === 'string' ? configPath.trim() : '';
  const effectiveConfigPath = requestedPath || getDefaultCloudflaredConfigPath();

  if (requestedPath) {
    assertReadableFile(effectiveConfigPath, 'Managed local tunnel config');
  } else {
    assertReadableFile(effectiveConfigPath, 'Managed local tunnel default config');
  }

  const configHostnameResult = extractHostnameFromCloudflaredConfigDetailed(effectiveConfigPath);
  if (configHostnameResult.parseError) {
    throw configHostnameResult.parseError;
  }

  const resolvedHost = normalizeHostname(hostname) || configHostnameResult.hostname;

  if (!resolvedHost) {
    throw new Error('Managed local tunnel hostname is required (use --tunnel-hostname or add an ingress hostname to the cloudflared config)');
  }

  const args = ['tunnel'];
  if (requestedPath) {
    args.push('--config', effectiveConfigPath);
  }
  args.push('run');

  const child = spawnCloudflared(args, {}, cfCheck.path);
  const publicUrl = `https://${resolvedHost}`;

  child.stdout.on('data', () => {
    // Keep stream drained, but avoid logging potentially sensitive output.
  });

  child.stderr.on('data', (chunk) => {
    const text = chunk.toString('utf8');
    process.stderr.write(text);
  });

  child.on('error', (error) => {
    console.error(`Cloudflared error: ${error.message}`);
  });

  try {
    await waitForManagedTunnelReady(child, { modeLabel: 'managed-local tunnel' });
  } catch (error) {
    try { child.kill('SIGINT'); } catch { /* ignore */ }
    throw error;
  }

  return {
    mode: TUNNEL_MODE_MANAGED_LOCAL,
    stop: () => {
      try {
        child.kill('SIGINT');
      } catch {
        // Ignore
      }
    },
    process: child,
    getPublicUrl: () => publicUrl,
    getResolvedHostname: () => resolvedHost,
    getEffectiveConfigPath: () => effectiveConfigPath,
  };
}

/** 兼容旧签名的内部入口：忽略 port 参数，直接转发到 quick tunnel。 */
async function startCloudflareTunnel({ originUrl, port }) {
  void port;
  return startCloudflareQuickTunnel({ originUrl });
}

/** 打印 quick tunnel 的限制提示（提供商配额、URL 临时有效、必须启用密码保护）。 */
export function printTunnelWarning() {
  console.log(`
⚠️  Quick Tunnel Limitations:

   • Provider limits may apply
   • URLs are temporary and will expire when the tunnel stops
   • Password protection is required for tunnel access

   For production use, set up a persistent provider tunnel or static domain.
`);
}
