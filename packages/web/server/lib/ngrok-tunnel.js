/**
 * ngrok 隧道封装。负责：检查 ngrok 二进制与 authtoken 配置、探测 ngrok 官方
 * API 可达性、以 JSON 日志模式启动 HTTP 隧道，并从 stdout 与本地 4040 管理 API
 * 两个渠道解析公网 URL；失败时把 ngrok JSON 日志提炼成一句可读错误。返回带
 * stop()/getPublicUrl() 的句柄。
 */
import { spawn, spawnSync } from 'child_process';
import {
  createExecutableSearchEnv,
  resolveExecutableLaunchTarget,
} from './tunnels/executable-search.js';
import { getTunnelDependencyInstallInfo } from './tunnels/install-help.js';
import { TUNNEL_PROVIDER_NGROK } from './tunnels/types.js';

/** 等待 ngrok 公网 URL 出现的启动超时（30s），超时杀进程并抛错。 */
const DEFAULT_STARTUP_TIMEOUT_MS = 30000;
/** ngrok 本地管理 API 地址，用于从 /api/tunnels 轮询公网 URL。 */
const NGROK_API_URL = 'http://127.0.0.1:4040/api/tunnels';
/** 从纯文本输出中抓取 https 地址的正则（是否为 ngrok 域名交由 normalize 判断）。 */
const NGROK_PUBLIC_URL_REGEX = /https:\/\/[^\s"']+/i;
/** authtoken 未配置时提示用户的修复命令。 */
const NGROK_AUTHTOKEN_HELP = 'Run: ngrok config add-authtoken <your-ngrok-token>';
/** 取 ngrok 的安装指引信息（复用 tunnels/install-help 的分平台文案）。 */
const getNgrokInstallInfo = () => getTunnelDependencyInstallInfo(TUNNEL_PROVIDER_NGROK);

/**
 * 检查 ngrok 是否已安装且可执行：经 executable-search 解析启动目标后同步运行
 * `ngrok version`。返回 { available, path, version }；不可用时 path/version 为 null。
 */
export async function checkNgrokAvailable() {
  const target = resolveExecutableLaunchTarget('ngrok');
  if (target) {
    try {
      const result = spawnSync(target.command, ['version'], {
        encoding: 'utf8',
        stdio: ['pipe', 'pipe', 'pipe'],
        windowsHide: true,
        env: target.env,
      });
      if (result.status === 0) {
        return { available: true, path: target.command, version: result.stdout.trim() || result.stderr.trim() };
      }
    } catch {
      // Ignore and report unavailable below.
    }
  }
  return { available: false, path: null, version: null };
}

/**
 * 检查 authtoken 是否已配置：优先认 NGROK_AUTHTOKEN 环境变量；否则运行
 * `ngrok config check`（可用 ngrokPath 指定二进制路径）。返回
 * { configured, detail }，detail 携带命令输出或安装/修复提示，供上层直接展示。
 */
export async function checkNgrokAuthtokenConfigured(ngrokPath = null) {
  if (typeof process.env.NGROK_AUTHTOKEN === 'string' && process.env.NGROK_AUTHTOKEN.trim().length > 0) {
    return { configured: true, detail: 'NGROK_AUTHTOKEN is set.' };
  }

  const target = ngrokPath
    ? { command: ngrokPath, env: createExecutableSearchEnv() }
    : resolveExecutableLaunchTarget('ngrok');
  if (!target) {
    return { configured: false, detail: getNgrokInstallInfo().message };
  }

  try {
    const result = spawnSync(target.command, ['config', 'check'], {
      encoding: 'utf8',
      stdio: ['pipe', 'pipe', 'pipe'],
      windowsHide: true,
      env: target.env,
    });
    const output = `${result.stdout || ''}${result.stderr || ''}`.trim();
    if (result.status === 0) {
      return { configured: true, detail: output || 'ngrok config is valid.' };
    }
    return { configured: false, detail: output || NGROK_AUTHTOKEN_HELP };
  } catch (error) {
    return {
      configured: false,
      detail: error instanceof Error ? error.message : String(error),
    };
  }
}

/**
 * 探测 ngrok 官方 API（api.ngrok.com）是否可达，用于区分"网络不通"与"配置
 * 错误"；可注入 fetchImpl 与超时（默认 5s）。返回 { reachable, status, error }。
 */
export async function checkNgrokApiReachability({ fetchImpl = globalThis.fetch, timeoutMs = 5000 } = {}) {
  if (typeof fetchImpl !== 'function') {
    return { reachable: false, status: null, error: 'Fetch API is unavailable in this runtime.' };
  }

  const controller = new AbortController();
  const timeout = setTimeout(() => controller.abort(), timeoutMs);
  try {
    const response = await fetchImpl('https://api.ngrok.com/', {
      method: 'GET',
      signal: controller.signal,
    });
    return { reachable: true, status: response.status, error: null };
  } catch (error) {
    return {
      reachable: false,
      status: null,
      error: error instanceof Error ? error.message : String(error),
    };
  } finally {
    clearTimeout(timeout);
  }
}

/** 以统一选项启动 ngrok 子进程：管道输出、Windows 隐藏窗口、可执行搜索 env、SIGINT 终止。 */
const spawnNgrok = (args, resolvedBinaryPath = 'ngrok') => spawn(resolvedBinaryPath, args, {
  stdio: ['ignore', 'pipe', 'pipe'],
  windowsHide: true,
  env: createExecutableSearchEnv(),
  killSignal: 'SIGINT',
});

/**
 * 校验并归一公网 URL：必须是 https 且主机名含 'ngrok'，去掉末尾斜杠；
 * 其余情况返回 null（用于过滤非 ngrok 地址）。
 */
const normalizeNgrokPublicUrl = (value) => {
  if (typeof value !== 'string' || value.trim().length === 0) {
    return null;
  }
  const trimmed = value.trim();
  try {
    const parsed = new URL(trimmed);
    if (parsed.protocol === 'https:' && parsed.hostname.includes('ngrok')) {
      return parsed.toString().replace(/\/$/, '');
    }
  } catch {
    return null;
  }
  return null;
};

/**
 * 逐行解析 ngrok 输出提取公网 URL：先按 JSON 行取 url/public_url 字段（容忍
 * 夹杂非 JSON 诊断行），再退回正则抓取 https 地址；两者都经 normalize 校验。
 * 找不到返回 null。
 */
export function extractNgrokPublicUrlFromText(text) {
  if (typeof text !== 'string' || text.trim().length === 0) {
    return null;
  }

  const lines = text.split(/\r?\n/).map((line) => line.trim()).filter(Boolean);
  for (const line of lines) {
    try {
      const parsed = JSON.parse(line);
      const parsedUrl = normalizeNgrokPublicUrl(parsed?.url) || normalizeNgrokPublicUrl(parsed?.public_url);
      if (parsedUrl) {
        return parsedUrl;
      }
    } catch {
      // ngrok may emit non-JSON diagnostics even when log-format=json.
    }

    const match = line.match(NGROK_PUBLIC_URL_REGEX);
    const matchedUrl = normalizeNgrokPublicUrl(match?.[0]);
    if (matchedUrl) {
      return matchedUrl;
    }
  }

  return null;
}

/** 把多行诊断文本压成单行：去 \r、丢弃空行、合并多余空白，便于拼进错误消息。 */
const normalizeNgrokDiagnosticText = (value) => {
  if (typeof value !== 'string') {
    return '';
  }
  return value
    .replace(/\r/g, '')
    .split('\n')
    .map((line) => line.trim())
    .filter(Boolean)
    .join(' ')
    .replace(/\s+/g, ' ')
    .trim();
};

/**
 * 从 ngrok 输出行提炼一句最有价值的错误摘要（附在超时/退出错误后）。
 * 优先级：最新的 eror/error/crit 级 JSON 的 err 字段 → 任意 JSON 行的
 * err/msg（跳过 context canceled 与无失败含义的 msg）→ 'ERROR:' 开头的纯文本
 * 行（最多拼 4 条）→ 最后一行的 err/msg 或原文。空输入返回空串。
 */
export const summarizeNgrokOutput = (lines) => {
  const nonEmptyLines = Array.isArray(lines)
    ? lines.map((line) => String(line || '').trim()).filter(Boolean)
    : [];
  if (nonEmptyLines.length === 0) {
    return '';
  }

  for (const line of [...nonEmptyLines].reverse()) {
    try {
      const parsed = JSON.parse(line);
      const level = typeof parsed?.lvl === 'string' ? parsed.lvl.toLowerCase() : '';
      if (level !== 'eror' && level !== 'error' && level !== 'crit') {
        continue;
      }
      const err = normalizeNgrokDiagnosticText(parsed?.err);
      if (err && err !== '<nil>') {
        return err;
      }
    } catch {
      // Not a JSON ngrok log line.
    }
  }

  for (const line of [...nonEmptyLines].reverse()) {
    try {
      const parsed = JSON.parse(line);
      const err = normalizeNgrokDiagnosticText(parsed?.err);
      if (err && err !== '<nil>' && !/context canceled/i.test(err)) {
        return err;
      }
      const msg = normalizeNgrokDiagnosticText(parsed?.msg);
      if (msg && /failed|error|invalid|auth/i.test(msg)) {
        return msg;
      }
    } catch {
      // Not a JSON ngrok log line.
    }
  }

  const errorLines = nonEmptyLines
    .filter((line) => /^ERROR:/i.test(line))
    .map((line) => normalizeNgrokDiagnosticText(line.replace(/^ERROR:\s*/i, '')))
    .filter(Boolean);
  if (errorLines.length > 0) {
    return errorLines.slice(0, 4).join(' ');
  }

  const lastLine = [...nonEmptyLines].reverse().find((line) => line.trim().length > 0);
  if (!lastLine) {
    return '';
  }
  try {
    const parsed = JSON.parse(lastLine);
    if (typeof parsed?.err === 'string' && parsed.err.trim().length > 0) {
      return normalizeNgrokDiagnosticText(parsed.err);
    }
    if (typeof parsed?.msg === 'string' && parsed.msg.trim().length > 0) {
      return normalizeNgrokDiagnosticText(parsed.msg);
    }
  } catch {
    // Fall through to plain text output.
  }
  return normalizeNgrokDiagnosticText(lastLine);
};

/** 把 summarizeNgrokOutput 的结果以 ": " 附加到错误消息之后；无摘要时原样返回。 */
const appendNgrokOutputSummary = (message, lines) => {
  const summary = summarizeNgrokOutput(lines);
  return summary ? `${message}: ${summary}` : message;
};

/**
 * 轮询 ngrok 本地管理 API（127.0.0.1:4040/api/tunnels）获取公网 URL：优先
 * proto 为 https 的隧道，退回任意可归一化的隧道。无 fetch 或任何失败返回 null。
 */
async function fetchNgrokPublicUrl(fetchImpl = globalThis.fetch) {
  if (typeof fetchImpl !== 'function') {
    return null;
  }
  try {
    const response = await fetchImpl(NGROK_API_URL, { method: 'GET' });
    if (!response.ok) {
      return null;
    }
    const payload = await response.json();
    const tunnels = Array.isArray(payload?.tunnels) ? payload.tunnels : [];
    const httpsTunnel = tunnels.find((entry) => entry?.proto === 'https' && normalizeNgrokPublicUrl(entry?.public_url));
    const fallbackTunnel = tunnels.find((entry) => normalizeNgrokPublicUrl(entry?.public_url));
    return normalizeNgrokPublicUrl(httpsTunnel?.public_url) || normalizeNgrokPublicUrl(fallbackTunnel?.public_url);
  } catch {
    return null;
  }
}

/**
 * 启动 ngrok HTTP 隧道：ngrok http --log=stdout --log-format=json 127.0.0.1:<port>。
 * 前置校验 ngrok 可用、authtoken 已配置、port 为有限数值，任一不满足直接抛错。
 * 启动后同时从 stdout JSON 日志与 4040 API（250ms 轮询）解析公网 URL，30s 内
 * 未拿到则杀进程并抛带输出摘要的错误；进程出错或提前退出同样抛错。stdout 仅
 * 解析不回显，stderr 原样回显。成功返回 { mode: 'quick', stop, process, getPublicUrl }。
 */
export async function startNgrokQuickTunnel({ port }) {
  const ngrokCheck = await checkNgrokAvailable();
  if (!ngrokCheck.available) {
    throw new Error(getNgrokInstallInfo().message);
  }

  const authtokenCheck = await checkNgrokAuthtokenConfigured(ngrokCheck.path);
  if (!authtokenCheck.configured) {
    throw new Error(`ngrok authtoken is not configured. ${authtokenCheck.detail || NGROK_AUTHTOKEN_HELP}`);
  }

  if (!Number.isFinite(port)) {
    throw new Error('A local port is required to start an ngrok tunnel');
  }

  const child = spawnNgrok(['http', '--log=stdout', '--log-format=json', `127.0.0.1:${port}`], ngrokCheck.path);
  let publicUrl = null;
  const recentOutput = [];

  /** 输出捕获：解析公网 URL，并把最近 200 行输出滚动保留，供失败时生成摘要。 */
  const captureOutput = (chunk) => {
    const text = chunk.toString('utf8');
    const parsedUrl = extractNgrokPublicUrlFromText(text);
    if (parsedUrl) {
      publicUrl = parsedUrl;
    }

    for (const line of text.split(/\r?\n/)) {
      const trimmed = line.trim();
      if (!trimmed) {
        continue;
      }
      recentOutput.push(trimmed);
      if (recentOutput.length > 200) {
        recentOutput.shift();
      }
    }
    return text;
  };

  child.stdout.on('data', (chunk) => {
    captureOutput(chunk);
  });

  child.stderr.on('data', (chunk) => {
    const text = captureOutput(chunk);
    process.stderr.write(text);
  });

  await new Promise((resolve, reject) => {
    let settled = false;
    /** 首次 settle 时统一收尾：清定时器、解绑监听后 resolve/reject；重复调用无效。 */
    const finish = (handler, value) => {
      if (settled) {
        return;
      }
      settled = true;
      clearTimeout(timeout);
      clearInterval(checkReady);
      child.off('error', onError);
      child.off('exit', onExit);
      handler(value);
    };

    const timeout = setTimeout(() => {
      try { child.kill('SIGINT'); } catch { /* ignore */ }
      finish(reject, new Error(appendNgrokOutputSummary('Ngrok tunnel URL not received within 30 seconds', recentOutput)));
    }, DEFAULT_STARTUP_TIMEOUT_MS);

    const checkReady = setInterval(async () => {
      publicUrl = publicUrl || await fetchNgrokPublicUrl();
      if (publicUrl) {
        finish(resolve, null);
      }
    }, 250);

    /** spawn 错误监听：直接以失败结束启动等待。 */
    const onError = (error) => {
      finish(reject, new Error(`Ngrok failed to start: ${error.message}`));
    };

    /** 进程退出监听：附带最近输出摘要地以失败结束启动等待。 */
    const onExit = (code) => {
      finish(reject, new Error(appendNgrokOutputSummary(`Ngrok exited while starting (code ${code ?? 'unknown'})`, recentOutput)));
    };

    child.once('error', onError);
    child.once('exit', onExit);
  });

  return {
    mode: 'quick',
    stop: () => {
      try {
        child.kill('SIGINT');
      } catch {
        // Ignore.
      }
    },
    process: child,
    getPublicUrl: () => publicUrl,
  };
}
