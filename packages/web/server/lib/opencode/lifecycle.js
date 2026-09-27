/**
 * OpenCode（omp host 引擎）生命周期运行时。
 *
 * 负责受管引擎进程的启动、健康监控、失败自动重启、配置变更后的刷新，
 * 以及外部（external）OpenCode 服务器的探测与复用。通过
 * createOpenCodeLifecycleRuntime 工厂创建，供 web server 在启动引导、
 * SSE/WebSocket 代理与配置写入等路径中复用。
 */
import { spawn, spawnSync } from 'node:child_process';
import net from 'node:net';
import { stripAppImageArgv0Leak } from '../inherited-env.js';
import { registerManagedProcess, unregisterManagedProcess, reapOrphanedProcesses } from './managed-process-registry.js';
import { applyProviderEnvAliases } from './provider-env-aliases.js';
import { resolveOmpHostLaunchSpec } from './omp-host-launch.js';
import { ensureOmpHostNatives } from './omp-host-natives.js';
import { recordStartupPerformance } from './startup-performance.js';

/**
 * 把环境变量值解析为正整数；非数字、非有限或小于等于 0 时返回 fallback。
 * @param {*} value 原始值（通常是 process.env 里的字符串）
 * @param {number} fallback 解析失败时的回退值
 * @returns {number} 解析出的正整数或 fallback
 */
const parsePositiveInt = (value, fallback) => {
  const parsed = Number.parseInt(String(value ?? ''), 10);
  return Number.isFinite(parsed) && parsed > 0 ? parsed : fallback;
};

// 单次健康检查 HTTP 请求的超时时间（毫秒），可用环境变量覆盖。
const HEALTH_CHECK_TIMEOUT_MS = parsePositiveInt(process.env.OMPCHAMBER_OPENCODE_HEALTH_TIMEOUT_MS, 5000);
// 触发自动重启前允许的连续健康检查失败次数。
const HEALTH_CHECK_MAX_CONSECUTIVE_FAILURES = parsePositiveInt(
  process.env.OMPCHAMBER_OPENCODE_HEALTH_CONSECUTIVE_FAILURES,
  20
);
// 周期健康检查间隔的环境变量覆盖（毫秒）；0 表示沿用调用方传入的默认间隔。
const HEALTH_CHECK_INTERVAL_OVERRIDE_MS = parsePositiveInt(process.env.OMPCHAMBER_OPENCODE_HEALTH_INTERVAL_MS, 0);
// 健康探测结果的缓存时长（毫秒），避免密集触发时重复发起探测。
const HEALTH_CHECK_RESULT_CACHE_MS = parsePositiveInt(process.env.OMPCHAMBER_OPENCODE_HEALTH_CACHE_MS, 750);
// OpenCode 健康检查端点路径。
const OPENCODE_HEALTH_PATH = '/global/health';
// Last-used directory plus the three most recently opened projects — deeper
// tails are unlikely to be the user's first click and just add background work.
// 预热目录数上限：最近使用目录 + 最近打开的三个项目。
const WARMUP_DIRECTORY_LIMIT = 4;
// 单个目录预热请求的超时时间（毫秒）。
const WARMUP_REQUEST_TIMEOUT_MS = 30000;
// 保留的引擎 stderr 尾部快照最大字节数。
const MANAGED_STDERR_TAIL_MAX_BYTES = 32 * 1024;
// 健康失败详情文本的最大长度（截断用）。
const HEALTH_FAILURE_DETAIL_MAX_LENGTH = 256;

/**
 * 按字节保留文本尾部：不超过 maxBytes 时原样返回，否则只保留最后
 * maxBytes 字节（以 Buffer 字节长度为准测量与截断）。
 * @param {*} value 任意输入（先转为字符串）
 * @param {number} maxBytes 尾部最大字节数
 * @returns {string} 截断后的文本
 */
const getBoundedTextTail = (value, maxBytes) => {
  const buffer = Buffer.from(String(value ?? ''));
  if (buffer.byteLength <= maxBytes) return buffer.toString();
  return buffer.subarray(buffer.byteLength - maxBytes).toString();
};

/**
 * 清洗诊断文本，脱敏其中可能出现的凭据：URL 内嵌的 user:password、
 * Bearer token、Authorization 样式的 scheme+credential、查询串里的
 * token/api key/password/secret/credential/private key 参数，以及键值
 * 对形式的敏感字段，统一替换为 [redacted]。用于引擎 stderr 尾部与
 * 错误消息，防止引擎日志把密钥带进服务器日志。
 * @param {*} value 原始输入
 * @returns {string} 脱敏后的字符串
 */
const sanitizeDiagnosticText = (value) => String(value ?? '')
  .replace(/(https?:\/\/)[^/\s:@]+:[^/\s@]+@/gi, '$1[redacted]@')
  .replace(/\b(Bearer)\s+[^\s,;]+/gi, '$1 [redacted]')
  // Unquoted `Authorization: <scheme> <credential>` values must be handled
  // before the generic key/value rule below: that rule stops at whitespace, so
  // it would redact only the scheme word and leave the credential intact.
  // Scoped to authorization-style keys so ordinary prose using "basic" or
  // "token" is not mangled.
  .replace(
    /(^|[\s,{\[])((?:"|')?[a-z0-9_.-]{0,80}authorization[a-z0-9_.-]{0,80}(?:"|')?\s*[:=]\s*(?:"|')?(?:basic|bearer|token)\s+)[^\s,;"']+/gim,
    '$1$2[redacted]',
  )
  .replace(/([?&][^=&#\s]*(?:token|api[_-]?key|password|secret|authorization|credential|private[_-]?key)[^=&#\s]*=)[^&#\s]+/gi, '$1[redacted]')
  .replace(
    /(^|[\s,{\[])((?:"|')?[a-z0-9_.-]{0,80}(?:token|api[_-]?key|password|secret|authorization|credential|private[_-]?key)[a-z0-9_.-]{0,80}(?:"|')?\s*[:=]\s*)("[^"]*"|'[^']*'|[^\s,;]+)/gim,
    '$1$2[redacted]',
  );

/**
 * 从健康检查错误提取「名称: 消息」形式的脱敏详情，并截断到
 * HEALTH_FAILURE_DETAIL_MAX_LENGTH 以内。
 * @param {*} error 任意错误对象或值
 * @returns {string} 可安全记录的失败详情
 */
const getHealthFailureDetail = (error) => {
  const name = String(error?.name || 'Error');
  const message = String(error?.message || error || 'Unknown error');
  return sanitizeDiagnosticText(`${name}: ${message}`).slice(0, HEALTH_FAILURE_DETAIL_MAX_LENGTH);
};

/**
 * 将健康探测异常归类为 timeout / connection_refused / connection_reset /
 * error 之一，并附脱敏详情，供重启诊断与日志区分失败模式。
 * @param {*} error 捕获的异常
 * @returns {{class: string, detail: string}} 错误类别与详情
 */
const classifyHealthProbeError = (error) => {
  const name = String(error?.name || '');
  const code = String(error?.code || '').toUpperCase();
  const message = String(error?.message || error || '');
  const normalizedMessage = message.toLowerCase();

  if (
    name === 'AbortError'
    || name === 'TimeoutError'
    || normalizedMessage.includes('the operation was aborted')
    || normalizedMessage.includes('abortsignal.timeout')
  ) {
    return { class: 'timeout', detail: getHealthFailureDetail(error) };
  }
  if (code === 'ECONNREFUSED' || normalizedMessage.includes('econnrefused')) {
    return { class: 'connection_refused', detail: getHealthFailureDetail(error) };
  }
  if (
    code === 'ECONNRESET'
    || normalizedMessage.includes('econnreset')
    || normalizedMessage.includes('socket hang up')
  ) {
    return { class: 'connection_reset', detail: getHealthFailureDetail(error) };
  }
  return { class: 'error', detail: getHealthFailureDetail(error) };
};


/**
 * 创建 OpenCode 生命周期运行时：管理受管引擎的启动/重启、健康监控、
 * 外部服务器探测与配置变更刷新。
 *
 * @param {object} deps 依赖注入集合
 * @param {object} deps.state 跨模块共享的服务器状态（openCodeProcess、openCodePort、isOpenCodeReady 等）
 * @param {object} deps.env 解析后的环境配置（端口、hostname、OPENCODE_HOST 等）
 * @param {Function} deps.syncToHmrState 把状态同步到 HMR 全局，保证热重载后状态延续
 * @param {Function} deps.syncFromHmrState 从 HMR 全局恢复状态
 * @param {Function} deps.getOpenCodeAuthHeaders 返回访问 OpenCode 所需的认证 headers
 * @param {Function} deps.buildOpenCodeUrl 拼接 OpenCode 服务基础 URL 与路径
 * @param {Function} deps.waitForReady 等待给定 URL 的服务通过健康检查
 * @param {Function} deps.normalizeApiPrefix 规范化 API 路径前缀
 * @param {Function} deps.ensureOpencodeCliEnv 确保 CLI 环境就绪
 * @param {Function} deps.ensureLocalOpenCodeServerPassword 获取/轮换本地 OpenCode server 密码
 * @param {Function} deps.resolveManagedOpenCodeLaunchSpec 解析受管引擎启动规格
 * @param {Function} deps.setOpenCodePort 把引擎端口写入共享状态
 * @param {Function} deps.setDetectedOpenCodeApiPrefix 把探测到的 API 前缀写入共享状态
 * @param {Function} deps.setupProxy 在 express app 上（重）挂载 OpenCode 反向代理
 * @param {Function} deps.ensureOpenCodeApiPrefix 确保 API 前缀已完成探测
 * @param {Function} deps.clearResolvedOpenCodeBinary 清除已解析的二进制缓存
 * @param {Function} deps.buildAugmentedPath 构建增强版 PATH（旧接口）
 * @param {Function} deps.buildManagedOpenCodePath 构建受管引擎专用 PATH
 * @param {Function} deps.getManagedOpenCodeShellEnvSnapshot 获取注入给引擎的 shell 环境快照
 * @param {Function} [deps.getManagedOpenCodeEnv] 获取引擎进程的额外环境变量
 * @param {Function} [deps.getActiveSessionCount] 返回当前 busy 会话数，用于繁忙时抑制重启
 * @param {Function} [deps.reapManagedOrphanedProcesses] 回收上次运行遗留的孤儿引擎进程
 * @param {Function} [deps.getWarmupDirectories] 返回启动后需要预热的目录列表
 * @param {Function} [deps.onOpenCodeRestarted] 重启成功后的回调（重绑事件流用）
 * @param {Function} [deps.now] 可注入的时钟（测试用）
 * @returns {object} 生命周期 API（startOpenCode、restartOpenCode、startHealthMonitoring 等）
 */
export const createOpenCodeLifecycleRuntime = (deps) => {
  const {
    state,
    env,
    syncToHmrState,
    syncFromHmrState,
    getOpenCodeAuthHeaders,
    buildOpenCodeUrl,
    waitForReady,
    normalizeApiPrefix,
    ensureOpencodeCliEnv,
    ensureLocalOpenCodeServerPassword,
    resolveManagedOpenCodeLaunchSpec,
    setOpenCodePort,
    setDetectedOpenCodeApiPrefix,
    setupProxy,
    ensureOpenCodeApiPrefix,
    clearResolvedOpenCodeBinary,
    buildAugmentedPath,
    buildManagedOpenCodePath,
    getManagedOpenCodeShellEnvSnapshot,
    getManagedOpenCodeEnv = async () => ({}),
    getActiveSessionCount = () => 0,
    reapManagedOrphanedProcesses = reapOrphanedProcesses,
    getWarmupDirectories = async () => [],
    onOpenCodeRestarted = null,
    now = Date.now,
  } = deps;

  /**
   * Windows 实现：用 PowerShell Get-NetTCPConnection 查出监听该端口的
   * 进程（排除自身 pid），再逐个 taskkill /F 强杀。全部 best-effort，
   * 任何失败都静默忽略。
   * @param {number|string} port 目标端口
   */
  const killProcessOnPortWin32 = (port) => {
    try {
      // Get-NetTCPConnection reads the same locale-independent WinNT API
      // netstat's display layer translates (e.g. "LISTENING" renders as
      // "ABHÖREN"/"ÉCOUTE"/"ESCUTANDO" on non-English Windows), so this
      // works regardless of the OS display language.
      const result = spawnSync(
        'powershell',
        [
          '-NoProfile',
          '-NonInteractive',
          '-Command',
          `Get-NetTCPConnection -State Listen -LocalPort ${Number.parseInt(port, 10)} -ErrorAction SilentlyContinue | Select-Object -ExpandProperty OwningProcess`,
        ],
        { encoding: 'utf8', timeout: 5000, windowsHide: true }
      );
      const output = result.stdout || '';
      const myPid = process.pid;
      const pids = new Set();
      for (const line of output.split(/\r?\n/)) {
        const pid = Number.parseInt(line.trim(), 10);
        if (pid && pid !== myPid) pids.add(pid);
      }
      for (const pid of pids) {
        try {
          spawnSync('taskkill', ['/PID', String(pid), '/F'], { stdio: 'ignore', timeout: 3000, windowsHide: true });
        } catch {
        }
      }
    } catch {
    }
  };

  /**
   * 强杀占用指定端口的其它进程：Windows 委托 killProcessOnPortWin32，
   * 其它平台用 lsof -t 找 pid 后 kill -9（跳过自身）。best-effort。
   * @param {number|string} port 目标端口
   */
  const killProcessOnPort = (port) => {
    if (!port) return;
    if (process.platform === 'win32') {
      killProcessOnPortWin32(port);
      return;
    }
    try {
      const result = spawnSync('lsof', ['-ti', `:${port}`], { encoding: 'utf8', timeout: 5000, windowsHide: true });
      const output = result.stdout || '';
      const myPid = process.pid;
      for (const pidStr of output.split(/\s+/)) {
        const pid = parseInt(pidStr.trim(), 10);
        if (pid && pid !== myPid) {
          try {
            spawnSync('kill', ['-9', String(pid)], { stdio: 'ignore', timeout: 2000 });
          } catch {
          }
        }
      }
    } catch {
    }
  };

  /**
   * 判断子进程是否已退出：句柄缺失，或 exitCode/signalCode 已设置。
   * @param {object|null} child 子进程句柄
   * @returns {boolean} 已退出（或句柄无效）时为 true
   */
  const hasChildProcessExited = (child) => !child
    || (child.exitCode !== null && child.exitCode !== undefined)
    || (child.signalCode !== null && child.signalCode !== undefined);

  /**
   * 判断受管引擎进程是否存活：句柄未退出且（有 pid 时）signal 0 探测
   * 成功。无 pid 的存活句柄按存活处理。
   * @returns {boolean}
   */
  const isManagedOpenCodeProcessAlive = () => {
    const child = state.openCodeProcess;
    if (!child || hasChildProcessExited(child)) return false;
    if (!child.pid) return true;
    try {
      process.kill(child.pid, 0);
      return true;
    } catch {
      return false;
    }
  };

  /**
   * 拍摄受管引擎进程快照（pid/exitCode/signalCode/脱敏 stderr 尾部），
   * 写入 state.lastManagedOpenCodeProcess 并返回。
   * @param {object} [child] 待拍摄的句柄，默认 state.openCodeProcess
   * @returns {object|null} 快照；无句柄时为 null
   */
  const snapshotManagedOpenCodeProcess = (child = state.openCodeProcess) => {
    if (!child) return null;
    const snapshot = {
      pid: child.pid || null,
      exitCode: child.exitCode ?? null,
      signalCode: child.signalCode ?? null,
      stderrTail: getBoundedTextTail(
        sanitizeDiagnosticText(child.stderrTail ?? ''),
        MANAGED_STDERR_TAIL_MAX_BYTES,
      ),
    };
    state.lastManagedOpenCodeProcess = snapshot;
    return snapshot;
  };

  /**
   * 重启前采集诊断：原因、最近一次健康失败、进程快照（含存活位）、
   * busy 会话数与时间戳，写入 state.lastOpenCodeRestartDiagnostics 并
   * 打印警告日志。
   * @param {string} reason 触发重启的原因标识
   */
  const captureRestartDiagnostics = (reason) => {
    const processSnapshot = snapshotManagedOpenCodeProcess();
    const diagnostics = {
      reason: sanitizeDiagnosticText(String(reason || 'managed-restart')).slice(0, HEALTH_FAILURE_DETAIL_MAX_LENGTH),
      healthFailure: state.lastOpenCodeHealthFailure ? { ...state.lastOpenCodeHealthFailure } : null,
      process: processSnapshot
        ? { ...processSnapshot, alive: isManagedOpenCodeProcessAlive() }
        : null,
      busySessionCount: getActiveSessionCount(),
      at: new Date(now()).toISOString(),
    };
    state.lastOpenCodeRestartDiagnostics = diagnostics;
    console.warn('[lifecycle] managed OpenCode restart diagnostics', diagnostics);
  };

  /**
   * 等待子进程退出（close/error 事件），最多 timeoutMs。
   * @param {object} child 子进程句柄
   * @param {number} timeoutMs 最长等待毫秒数
   * @returns {Promise<boolean>} 超时前是否确认退出
   */
  const waitForChildProcessClose = (child, timeoutMs) => new Promise((resolve) => {
    if (!child || hasChildProcessExited(child)) {
      resolve(true);
      return;
    }

    let done = false;
    // 幂等收口：只结算一次，清理定时器并解绑监听。
    const finish = (closed) => {
      if (done) return;
      done = true;
      clearTimeout(timer);
      child.off('close', onClose);
      child.off('error', onError);
      resolve(closed);
    };

    // close 事件：确认已退出。
    const onClose = () => finish(true);
    // error 事件：按句柄状态判断是否已退出。
    const onError = () => finish(hasChildProcessExited(child));
    const timer = setTimeout(() => finish(hasChildProcessExited(child)), timeoutMs);

    child.once('close', onClose);
    child.once('error', onError);
  });

  /**
   * 轮询探测端口直到连接被拒绝（说明已释放）或超时。通配绑定地址
   * （0.0.0.0/::）统一改用 127.0.0.1 探测；每 150ms 重试一次。
   * @param {number} port 目标端口
   * @param {number} timeoutMs 最长等待毫秒数
   * @param {string} [hostname] 探测目标主机名
   * @returns {Promise<boolean>} 端口是否已释放
   */
  const waitForPortRelease = (port, timeoutMs, hostname = env.ENV_CONFIGURED_OPENCODE_HOSTNAME) => {
    if (!port) {
      return Promise.resolve(true);
    }

    const probeHost = !hostname || hostname === '0.0.0.0' || hostname === '::' || hostname === '[::]'
      ? '127.0.0.1'
      : hostname;
    const deadline = Date.now() + timeoutMs;

    return new Promise((resolve) => {
      // 发起一次 TCP 连接探测：能连上说明端口仍被占用。
      const attempt = () => {
        const socket = net.connect({ port, host: probeHost });
        let settled = false;

        // 结算本次探测：销毁 socket，未释放且未到截止时间则 150ms 后重试。
        const finish = (released) => {
          if (settled) return;
          settled = true;
          socket.removeAllListeners();
          socket.destroy();
          if (released || Date.now() >= deadline) {
            resolve(released);
            return;
          }
          setTimeout(attempt, 150);
        };

        socket.once('connect', () => finish(false));
        socket.once('timeout', () => finish(true));
        socket.once('error', (error) => {
          if (error && typeof error === 'object' && (error.code === 'ECONNREFUSED' || error.code === 'EHOSTUNREACH')) {
            finish(true);
            return;
          }
          finish(false);
        });
        socket.setTimeout(500);
      };

      attempt();
    });
  };

  /**
   * 终止子进程及其进程组。Windows：先温和 kill，再 taskkill /t，最后
   * taskkill /f /t 逐级升级；类 Unix：先对进程组 SIGTERM，2.5s 未退出
   * 再 SIGKILL。各阶段失败均静默忽略，最后只等待收尾。
   * @param {object} child 子进程句柄
   * @returns {Promise<void>}
   */
  const terminateChildProcess = async (child) => {
    if (!child) {
      return;
    }

    const pid = child.pid;
    if (!pid || hasChildProcessExited(child)) {
      await waitForChildProcessClose(child, 250);
      return;
    }

    // 对进程组（非 Windows）与子进程本身发送同一信号。
    const signalProcessTree = (signal) => {
      if (process.platform !== 'win32') {
        try {
          process.kill(-pid, signal);
        } catch {
        }
      }

      try {
        child.kill(signal);
      } catch {
      }
    };

    if (process.platform === 'win32') {
      try {
        child.kill();
      } catch {
      }

      if (await waitForChildProcessClose(child, 800)) {
        return;
      }

      try {
        spawnSync('taskkill', ['/pid', String(pid), '/t'], {
          stdio: 'ignore',
          timeout: 3000,
          windowsHide: true,
        });
      } catch {
      }

      if (await waitForChildProcessClose(child, 1500)) {
        return;
      }

      try {
        spawnSync('taskkill', ['/pid', String(pid), '/f', '/t'], {
          stdio: 'ignore',
          timeout: 5000,
          windowsHide: true,
        });
      } catch {
      }

      await waitForChildProcessClose(child, 3000);
      return;
    }

    signalProcessTree('SIGTERM');

    if (await waitForChildProcessClose(child, 2500)) {
      return;
    }

    signalProcessTree('SIGKILL');

    await waitForChildProcessClose(child, 1000);
  };

  /**
   * 关闭受管引擎子进程并注销受管进程注册：仅在进程确实退出后才注销，
   * 侥幸存活的子进程保留注册，留给下一次启动的回收器处理。
   * @param {object} child 子进程句柄
   * @returns {Promise<void>}
   */
  const closeManagedOpenCodeChild = async (child) => {
    const pid = child?.pid;
    try {
      await terminateChildProcess(child);
    } finally {
      // Drop it from the registry only once it has actually exited, so a child
      // that survived teardown stays eligible for the next run's reaper.
      if (Number.isInteger(pid) && hasChildProcessExited(child)) {
        await unregisterManagedProcess(pid);
      }
    }
  };

  /**
   * 把捕获的 stdout/stderr 组装成带标签的文本（用于启动失败错误消息）。
   * @param {{stdout?: string, stderr?: string}} output 捕获输出
   * @returns {string} 格式化文本；两者皆空时返回占位说明
   */
  const formatCapturedOutput = ({ stdout, stderr }) => {
    const parts = [];
    if (stdout.trim()) {
      parts.push(`stdout:\n${stdout.trim()}`);
    }
    if (stderr.trim()) {
      parts.push(`stderr:\n${stderr.trim()}`);
    }
    return parts.length > 0 ? parts.join('\n\n') : 'No stdout/stderr captured';
  };

  /**
   * 拉起受管引擎进程（omp host）并等待其输出 listening 就绪行。
   *
   * 流程：解析启动规格（环境变量覆盖或内置二进制；source 启动需先准备
   * natives 插件）→ 记录启动诊断 → spawn 子进程 → 监听 stdout 中的
   * "opencode server listening on <url>" 行并解析 URL → 就绪后把子进程
   * 登记进受管进程注册表（pid/ownerPid/port/binary/runtime）。
   * 就绪失败（超时/提前退出/解析失败）会先杀掉子进程再抛错，保证不
   * 泄漏未跟踪的引擎进程。
   *
   * @param {object} options 启动参数
   * @param {string} options.hostname 绑定 hostname
   * @param {number} options.port 监听端口
   * @param {number} options.timeout 等待就绪行的超时（毫秒）
   * @param {string} options.cwd 子进程工作目录
   * @param {object} options.env 子进程环境变量
   * @param {number} [options.shellEnvKeysCount] shell 环境快照键数（诊断用）
   * @returns {Promise<object>} 引擎句柄：url、pid、exitCode/signalCode/
   *   stderrTail getter 与 close() 方法
   */
  const createManagedOpenCodeServerProcess = async ({ hostname, port, timeout, cwd, env: processEnv, shellEnvKeysCount = 0 }) => {
    // The managed engine is the OMPChamber omp host (Bun + @oh-my-pi/
    // pi-coding-agent), launched with the same `serve --hostname --port`
    // shape `opencode serve` used, including the readiness stdout line.
    const launch = resolveOmpHostLaunchSpec({ hostname, port });
    if (launch.source !== 'env-host' && launch.source !== 'bundled') {
      // Source launches need the pi_natives addon in the per-user cache;
      // compiled host binaries already ship it beside the executable.
      await ensureOmpHostNatives();
    }
    const binary = launch.binary;
    const args = launch.args;
    const sourceBinary = binary;
    const launchWrapperType = null;

    const pathValue = typeof processEnv?.PATH === 'string' ? processEnv.PATH : '';
    const pathEntryCount = pathValue ? pathValue.split(process.platform === 'win32' ? ';' : ':').filter(Boolean).length : 0;
    state.lastOpenCodeLaunchDiagnostics = {
      launchedAt: new Date().toISOString(),
      sourceBinary,
      binary,
      args,
      cwd,
      hostname,
      port,
      wrapperType: launchWrapperType,
      runtimeSource: launch.source,
      pathEntryCount,
      hasShellEnv: shellEnvKeysCount > 0,
      shellEnvKeysCount,
    };
    console.log('[omp-host] Launching managed engine', state.lastOpenCodeLaunchDiagnostics);

    const child = spawn(binary, args, {
      cwd,
      env: processEnv,
      detached: process.platform !== 'win32',
      windowsHide: true,
      stdio: ['ignore', 'pipe', 'pipe'],
    });
    // 引擎 stderr 滚动尾部缓冲、监听是否已挂载、观测到的退出码与信号。
    let runtimeStderrTail = '';
    let runtimeStderrAttached = false;
    let observedExitCode = null;
    let observedSignalCode = null;

    /** 构造引擎进程当前快照（pid/退出码/信号/脱敏 stderr 尾部）。 */
    const getManagedProcessSnapshot = () => ({
      pid: child.pid || null,
      exitCode: observedExitCode ?? child.exitCode ?? null,
      signalCode: observedSignalCode ?? child.signalCode ?? null,
      stderrTail: getBoundedTextTail(sanitizeDiagnosticText(runtimeStderrTail), MANAGED_STDERR_TAIL_MAX_BYTES),
    });
    /** 记录引擎退出码/信号（exit 与 close 都会触发），并刷新共享状态里的快照。 */
    const recordManagedProcessExit = (code, signal) => {
      if (code !== null && code !== undefined) observedExitCode = code;
      if (signal !== null && signal !== undefined) observedSignalCode = signal;
      state.lastManagedOpenCodeProcess = getManagedProcessSnapshot();
    };
    /** 幂等挂载 stderr 捕获：把输出滚动保留在尾部缓冲里（就绪后才开始收集）。 */
    const attachRuntimeStderrCapture = () => {
      if (runtimeStderrAttached) return;
      runtimeStderrAttached = true;
      child.stderr?.on('data', (chunk) => {
        runtimeStderrTail = getBoundedTextTail(
          `${runtimeStderrTail}${chunk.toString()}`,
          MANAGED_STDERR_TAIL_MAX_BYTES,
        );
      });
    };
    child.on('exit', recordManagedProcessExit);
    child.on('close', recordManagedProcessExit);

    /** Promise：等待 stdout 出现 listening 行并解析出服务 URL；提前退出、解析失败或超时则 reject（附带捕获的输出）。 */
    const waitForListeningLine = new Promise((resolve, reject) => {
      let stdout = '';
      let stderr = '';
      let done = false;
      // 首次结果生效：停表并解绑全部监听后结束 Promise。
      const finish = (handler, value) => {
        if (done) return;
        done = true;
        clearTimeout(timer);
        child.stdout?.off('data', onStdout);
        child.stderr?.off('data', onStderr);
        child.off('exit', onExit);
        child.off('error', onError);
        handler(value);
      };

      // 累积 stdout 并逐行查找 listening 行；找到即停止监听并 resolve URL。
      const onStdout = (chunk) => {
        stdout += chunk.toString();
        const lines = stdout.split('\n');
        for (const line of lines) {
          if (!line.startsWith('opencode server listening')) continue;
          const match = line.match(/on\s+(https?:\/\/[^\s]+)/);
          if (!match) {
            finish(reject, new Error(`Failed to parse server url from output: ${line}`));
            return;
          }
          attachRuntimeStderrCapture();
          finish(resolve, match[1]);
          return;
        }
      };

      // 仅累积 stderr，供失败时拼进错误消息。
      const onStderr = (chunk) => {
        stderr += chunk.toString();
      };

      // 进程在就绪前退出：拼装原因与（可能的 macOS 桌面 App 误配提示）后 reject。
      const onExit = (code, signal) => {
        const reason = signal ? `signal ${signal}` : `code ${code}`;
        const appBundleHint = process.platform === 'darwin' && /\/OpenCode\.app\/Contents\/MacOS\/(?:OpenCode|opencode-cli)$/i.test(binary)
          ? ' The configured binary appears to point at the macOS desktop app bundle; OMPChamber needs the standalone opencode CLI.'
          : '';
        finish(reject, new Error(`OpenCode process exited before serving with ${reason}. Binary used: ${binary}.${appBundleHint} ${formatCapturedOutput({ stdout, stderr })}`));
      };

      // spawn 本身失败（如二进制不存在）：直接 reject。
      const onError = (error) => {
        finish(reject, error);
      };

      const timer = setTimeout(() => {
        finish(reject, new Error(`Timeout waiting for OpenCode to start after ${timeout}ms`));
      }, timeout);

      child.stdout?.on('data', onStdout);
      child.stderr?.on('data', onStderr);
      child.on('exit', onExit);
      child.on('error', onError);
    });

    // Readiness failure must never leak the child: registry ownership (and
    // with it teardown) is granted only after readiness, so a spawn that never
    // prints the listening line — or times out doing so — is killed here
    // instead of surviving as an untracked engine process.
    let url;
    try {
      url = await waitForListeningLine;
    } catch (error) {
      await terminateChildProcess(child);
      throw error;
    }

    // Record this child so a future run can reap it if we crash before teardown.
    // The web-server lifecycle runs in-process inside multiple hosts, so tag the
    // actual host (Electron sets OMPCHAMBER_RUNTIME='desktop'; the standalone
    // web CLI leaves it unset → 'web'; SSH remote → 'ssh-remote') rather than a
    // hardcoded label, matching the server's existing runtimeName convention.
    await registerManagedProcess({
      pid: child.pid,
      ownerPid: process.pid,
      port,
      binary,
      runtime: process.env.OMPCHAMBER_RUNTIME || 'web',
    });

    // 返回对外稳定的引擎句柄：动态 getter 反映最新观测值，close() 负责终止与注销。
    return {
      url,
      pid: child.pid || null,
      get exitCode() {
        return observedExitCode ?? child.exitCode;
      },
      get signalCode() {
        return observedSignalCode ?? child.signalCode;
      },
      get stderrTail() {
        return getManagedProcessSnapshot().stderrTail;
      },
      async close() {
        await closeManagedOpenCodeChild(child);
      },
    };
  };

  /**
   * 确定受管引擎监听端口：显式正整数直接采用；否则在 hostname 上
   * listen(0) 让操作系统分配一个空闲端口（随即关闭再返回）。
   * @param {number} requestedPort 请求端口；0 或无效值表示自动分配
   * @param {string} [hostname] 自动分配时绑定的 hostname
   * @returns {Promise<number>} 最终端口号
   */
  const resolveManagedOpenCodePort = async (requestedPort, hostname = '127.0.0.1') => {
    if (typeof requestedPort === 'number' && Number.isFinite(requestedPort) && requestedPort > 0) {
      return requestedPort;
    }

    return await new Promise((resolve, reject) => {
      const server = net.createServer();
      // 移除 server 上的临时监听器，避免泄漏。
      const cleanup = () => {
        server.removeAllListeners('error');
        server.removeAllListeners('listening');
      };

      server.once('error', (error) => {
        cleanup();
        reject(error);
      });

      server.once('listening', () => {
        const address = server.address();
        const port = address && typeof address === 'object' ? address.port : 0;
        server.close(() => {
          cleanup();
          if (port > 0) {
            resolve(port);
            return;
          }
          reject(new Error('Failed to allocate OpenCode port'));
        });
      });

      server.listen(0, hostname);
    });
  };

  /**
   * 调用引擎 /global/health 做一次详细健康探测。
   * @returns {Promise<{healthy: boolean, failure: {class: string, detail: string}|null}>}
   *   失败时 failure.class 为 invalid_response（非 2xx/坏 JSON/未报
   *   healthy=true）或 classifyHealthProbeError 归出的网络错误类别
   */
  const probeOpenCodeHealthDetailed = async () => {
    if (!state.openCodeProcess || !state.openCodePort) {
      return {
        healthy: false,
        failure: {
          class: 'error',
          detail: 'Managed OpenCode process or port is unavailable',
        },
      };
    }

    try {
      const response = await fetch(buildOpenCodeUrl(OPENCODE_HEALTH_PATH, ''), {
        method: 'GET',
        headers: {
          Accept: 'application/json',
          ...getOpenCodeAuthHeaders(),
        },
        signal: AbortSignal.timeout(HEALTH_CHECK_TIMEOUT_MS),
      });
      if (!response.ok) {
        return {
          healthy: false,
          failure: {
            class: 'invalid_response',
            detail: `Health endpoint returned HTTP ${response.status ?? 'unknown'}`,
          },
        };
      }
      let body;
      try {
        body = await response.json();
      } catch {
        return {
          healthy: false,
          failure: {
            class: 'invalid_response',
            detail: 'Health endpoint returned invalid JSON',
          },
        };
      }
      if (body?.healthy !== true) {
        return {
          healthy: false,
          failure: {
            class: 'invalid_response',
            detail: 'Health endpoint did not report healthy=true',
          },
        };
      }
      return { healthy: true, failure: null };
    } catch (error) {
      return {
        healthy: false,
        failure: classifyHealthProbeError(error),
      };
    }
  };

  /** probeOpenCodeHealthDetailed 的布尔封装：当前引擎是否健康。 */
  const isOpenCodeProcessHealthy = async () => (await probeOpenCodeHealthDetailed()).healthy;

  /**
   * 探测外部（非受管）OpenCode 服务器：3 秒超时内请求 /global/health
   * 且响应 healthy=true 才算健康；任何异常都按不健康处理。
   * @param {number} port 目标端口
   * @param {string} [origin] 完整 origin；缺省按 http://127.0.0.1:port
   * @returns {Promise<boolean>} 是否健康
   */
  const probeExternalOpenCode = async (port, origin) => {
    if (!port || port <= 0) {
      return false;
    }

    try {
      const controller = new AbortController();
      const timeout = setTimeout(() => controller.abort(), 3000);
      const base = origin ?? `http://127.0.0.1:${port}`;
      const response = await fetch(`${base}${OPENCODE_HEALTH_PATH}`, {
        method: 'GET',
        headers: {
          Accept: 'application/json',
          ...getOpenCodeAuthHeaders(),
        },
        signal: controller.signal,
      });
      clearTimeout(timeout);
      if (!response.ok) return false;
      const body = await response.json().catch(() => null);
      return body?.healthy === true;
    } catch {
      return false;
    }
  };

  /**
   * 轮询等待共享状态里的 openCodePort 被赋值（由启动流程写入）。
   * @param {number} [timeoutMs] 最长等待毫秒数
   * @returns {Promise<number>} 端口号；超时抛错
   */
  const waitForOpenCodePort = async (timeoutMs = 15000) => {
    if (state.openCodePort !== null) {
      return state.openCodePort;
    }

    const deadline = Date.now() + timeoutMs;
    while (Date.now() < deadline) {
      await new Promise((resolve) => setTimeout(resolve, 50));
      if (state.openCodePort !== null) {
        return state.openCodePort;
      }
    }

    throw new Error('Timed out waiting for OpenCode port');
  };

  // 受管引擎启动的最大尝试次数（失败后带退避重试一次）。
  const START_OPEN_CODE_MAX_ATTEMPTS = 2;

  /**
   * 毫秒级延时工具。
   * @param {number} ms 延时毫秒数
   * @returns {Promise<void>}
   */
  const delay = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

  /**
   * 执行一次完整的引擎启动尝试：确定端口 → 准备 CLI 环境与本地 server
   * 密码（managed 模式下轮换）→ 组装 PATH/shell 环境/引擎专属环境变量
   * → createManagedOpenCodeServerProcess 拉起进程 → waitForReady 验证
   * 健康 → 写入端口与 API 前缀并标记就绪。各阶段记录 startup
   * performance 打点；失败时清理端口、同步 HMR 状态、记录
   * lastOpenCodeError 后原样抛出。
   * @param {number} attempt 尝试序号（从 1 开始，用于打点与日志）
   * @returns {Promise<object>} 就绪的引擎句柄
   */
  const startOpenCodeOnce = async (attempt) => {
    const attemptStartedAt = performance.now();
    let phaseStartedAt = attemptStartedAt;
    recordStartupPerformance('opencode.attempt.start', { attempt });
    const desiredPort = env.ENV_CONFIGURED_OPENCODE_PORT ?? 0;
    const spawnPort = await resolveManagedOpenCodePort(desiredPort, env.ENV_CONFIGURED_OPENCODE_HOSTNAME);
    console.log(
      desiredPort > 0
        ? `Starting OpenCode on requested port ${desiredPort}...`
        : `Starting OpenCode on allocated port ${spawnPort}...`
    );

    ensureOpencodeCliEnv();
    recordStartupPerformance('opencode.binary.ready', {
      attempt,
      durationMs: performance.now() - phaseStartedAt,
      totalDurationMs: performance.now() - attemptStartedAt,
    });
    phaseStartedAt = performance.now();
    const openCodePassword = await ensureLocalOpenCodeServerPassword({ rotateManaged: true });
    let envPath = process.env.PATH;
    if (typeof buildManagedOpenCodePath === 'function') {
      envPath = buildManagedOpenCodePath();
    } else if (typeof buildAugmentedPath === 'function') {
      envPath = buildAugmentedPath();
    }
    const shellEnv = typeof getManagedOpenCodeShellEnvSnapshot === 'function'
      ? getManagedOpenCodeShellEnvSnapshot() || {}
      : {};
    const managedOpenCodeEnv = await getManagedOpenCodeEnv();
    recordStartupPerformance('opencode.environment.ready', {
      attempt,
      durationMs: performance.now() - phaseStartedAt,
      totalDurationMs: performance.now() - attemptStartedAt,
    });
    phaseStartedAt = performance.now();

    try {
      const serverInstance = await createManagedOpenCodeServerProcess({
        hostname: env.ENV_CONFIGURED_OPENCODE_HOSTNAME,
        port: spawnPort,
        timeout: parsePositiveInt(process.env.OMPCHAMBER_OMP_HOST_READY_TIMEOUT_MS, 30000),
        cwd: state.openCodeWorkingDirectory,
        shellEnvKeysCount: Object.keys(shellEnv).length,
        env: stripAppImageArgv0Leak(applyProviderEnvAliases({
          ...shellEnv,
          ...process.env,
          ...managedOpenCodeEnv,
          PATH: envPath,
          OPENCODE_SERVER_PASSWORD: openCodePassword,
        })),
      });

      if (!serverInstance || !serverInstance.url) {
        throw new Error('OpenCode server started but URL is missing');
      }
      recordStartupPerformance('opencode.process.ready', {
        attempt,
        durationMs: performance.now() - phaseStartedAt,
        totalDurationMs: performance.now() - attemptStartedAt,
      });
      phaseStartedAt = performance.now();

      const url = new URL(serverInstance.url);
      const port = parseInt(url.port, 10);
      const prefix = normalizeApiPrefix(url.pathname);

      if (await waitForReady(serverInstance.url, 10000)) {
        setOpenCodePort(port);
        setDetectedOpenCodeApiPrefix(prefix);

        state.isOpenCodeReady = true;
        state.lastOpenCodeError = null;
        state.openCodeNotReadySince = 0;

        recordStartupPerformance('opencode.health.ready', {
          attempt,
          durationMs: performance.now() - phaseStartedAt,
          totalDurationMs: performance.now() - attemptStartedAt,
          outcome: 'ready',
        });

        return serverInstance;
      }

      try {
        await serverInstance.close();
      } catch {
      }
      throw new Error('Server started but health check failed (timeout)');
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error);
      state.lastOpenCodeError = message;
      state.openCodePort = null;
      syncToHmrState();
      recordStartupPerformance('opencode.attempt.error', {
        attempt,
        totalDurationMs: performance.now() - attemptStartedAt,
        outcome: 'error',
      });
      console.error(`Failed to start OpenCode: ${message}`);
      throw error;
    }
  };

  /**
   * 带重试的启动入口：最多 START_OPEN_CODE_MAX_ATTEMPTS 次，失败之间
   * 以 750ms * attempt 退避；全部失败时抛出最后一次的错误。
   * @returns {Promise<object>} 就绪的引擎句柄
   */
  const startOpenCode = async () => {
    let lastError = null;
    for (let attempt = 1; attempt <= START_OPEN_CODE_MAX_ATTEMPTS; attempt += 1) {
      try {
        return await startOpenCodeOnce(attempt);
      } catch (error) {
        lastError = error;
        if (attempt >= START_OPEN_CODE_MAX_ATTEMPTS) {
          break;
        }

        const message = error instanceof Error ? error.message : String(error);
        console.warn(`[OpenCode] Managed server startup failed on attempt ${attempt}/${START_OPEN_CODE_MAX_ATTEMPTS}; retrying: ${message}`);
        state.openCodePort = null;
        state.isOpenCodeReady = false;
        state.openCodeNotReadySince = Date.now();
        syncToHmrState();
        await delay(750 * attempt);
      }
    }

    throw lastError;
  };

  /**
   * 重启 OpenCode。外部模式：只重新探测外部服务器健康并重挂代理；
   * 受管模式：拍诊断快照 → 关闭旧进程 → killProcessOnPort 并等端口
   * 释放 → 重置端口与 API 前缀 → startOpenCode 重新拉起 → 重挂代理 →
   * 触发 onOpenCodeRestarted 让事件流重绑到（可能变化的）新端口。
   * 通过 state.currentRestartPromise 去重，并发调用等待同一次重启完成；
   * 正在关闭时直接跳过。
   * @param {string} [reason] 重启原因（写入诊断与日志）
   * @returns {Promise<void>} 失败时复位相关状态并抛错
   */
  const restartOpenCode = async (reason = 'managed-restart') => {
    if (state.isShuttingDown) return;
    if (state.currentRestartPromise) {
      await state.currentRestartPromise;
      return;
    }

    state.currentRestartPromise = (async () => {
      state.isRestartingOpenCode = true;
      state.isOpenCodeReady = false;
      state.openCodeNotReadySince = Date.now();
      console.log('Restarting OpenCode process...');

      if (state.isExternalOpenCode) {
        console.log('Re-probing external OpenCode server...');
        const probePort = state.openCodePort ?? env.ENV_EFFECTIVE_PORT ?? 4096;
        const probeOrigin = state.openCodeBaseUrl ?? env.ENV_CONFIGURED_OPENCODE_HOST?.origin;
        const healthy = await probeExternalOpenCode(probePort, probeOrigin);
        if (healthy) {
          console.log(`External OpenCode server on port ${probePort} is healthy`);
          state.openCodeBaseUrl = probeOrigin ?? null;
          setOpenCodePort(probePort);
          state.isOpenCodeReady = true;
          state.lastOpenCodeError = null;
          state.openCodeNotReadySince = 0;
          syncToHmrState();
        } else {
          state.lastOpenCodeError = `External OpenCode server on port ${probePort} is not responding`;
          console.error(state.lastOpenCodeError);
          throw new Error(state.lastOpenCodeError);
        }

        if (state.expressApp) {
          setupProxy(state.expressApp);
          ensureOpenCodeApiPrefix();
        }
        return;
      }

      captureRestartDiagnostics(reason);
      const portToKill = state.openCodePort;

      if (state.openCodeProcess) {
        console.log('Stopping existing OpenCode process...');
        try {
          await state.openCodeProcess.close();
        } catch (error) {
          console.warn('Error closing engine process:', error);
        }
        state.openCodeProcess = null;
        syncToHmrState();
      }

      killProcessOnPort(portToKill);
      if (!(await waitForPortRelease(portToKill, 5000))) {
        console.warn(`Timed out waiting for OpenCode port ${portToKill} to be released`);
      }

      if (env.ENV_CONFIGURED_OPENCODE_PORT) {
        console.log(`Using OpenCode port from environment: ${env.ENV_CONFIGURED_OPENCODE_PORT}`);
        setOpenCodePort(env.ENV_CONFIGURED_OPENCODE_PORT);
      } else {
        state.openCodePort = null;
        syncToHmrState();
      }

      state.openCodeApiPrefixDetected = true;
      state.openCodeApiPrefix = '';
      if (state.openCodeApiDetectionTimer) {
        clearTimeout(state.openCodeApiDetectionTimer);
        state.openCodeApiDetectionTimer = null;
      }

      state.lastOpenCodeError = null;
      state.openCodeProcess = await startOpenCode();
      syncToHmrState();

      if (state.expressApp) {
        setupProxy(state.expressApp);
        ensureOpenCodeApiPrefix();
      }

      // The restart may have landed on a NEW port (the old one can remain
      // occupied if killProcessOnPort/waitForPortRelease didn't free it in
      // time, on any platform). Upstream event readers pinned to the old
      // process would keep the UI silent forever, so rebind them to the
      // current port. Best effort: a failure here must not fail the restart
      // itself.
      try {
        onOpenCodeRestarted?.();
      } catch (error) {
        console.warn('Failed to rebind event stream after OpenCode restart:', error?.message ?? error);
      }
    })();

    try {
      await state.currentRestartPromise;
    } catch (error) {
      console.error(`Failed to restart OpenCode: ${error.message}`);
      state.lastOpenCodeError = error.message;
      if (!env.ENV_EFFECTIVE_PORT) {
        state.openCodePort = null;
        syncToHmrState();
      }
      state.openCodeApiPrefixDetected = true;
      state.openCodeApiPrefix = '';
      throw error;
    } finally {
      state.currentRestartPromise = null;
      state.isRestartingOpenCode = false;
    }
  };

  /**
   * 轮询 /global/health 直到引擎报告 healthy=true 或超时。成功会置位
   * isOpenCodeReady 并清除错误状态；失败把最后错误写入
   * state.lastOpenCodeError 后抛出。
   * @param {number} [timeoutMs] 总超时毫秒数
   * @param {number} [intervalMs] 轮询间隔毫秒数
   * @returns {Promise<void>}
   */
  const waitForOpenCodeReady = async (timeoutMs = 20000, intervalMs = 400) => {
    if (!state.openCodePort) {
      throw new Error('OpenCode port is not available');
    }

    const deadline = Date.now() + timeoutMs;
    let lastError = null;

    while (Date.now() < deadline) {
      let timeout = null;
      try {
        const controller = new AbortController();
        timeout = setTimeout(() => controller.abort(), HEALTH_CHECK_TIMEOUT_MS);
        const response = await fetch(buildOpenCodeUrl(OPENCODE_HEALTH_PATH, ''), {
          method: 'GET',
          headers: { Accept: 'application/json', ...getOpenCodeAuthHeaders() },
          signal: controller.signal,
        });
        clearTimeout(timeout);
        timeout = null;

        if (!response.ok) {
          lastError = new Error(`OpenCode health endpoint responded with status ${response.status}`);
          await new Promise((resolve) => setTimeout(resolve, intervalMs));
          continue;
        }

        const body = await response.json().catch(() => null);
        if (body?.healthy !== true) {
          lastError = new Error('OpenCode health endpoint returned unhealthy response');
          await new Promise((resolve) => setTimeout(resolve, intervalMs));
          continue;
        }

        state.isOpenCodeReady = true;
        state.lastOpenCodeError = null;
        return;
      } catch (error) {
        lastError = error;
      } finally {
        if (timeout) {
          clearTimeout(timeout);
        }
      }

      await new Promise((resolve) => setTimeout(resolve, intervalMs));
    }

    if (lastError) {
      state.lastOpenCodeError = lastError.message || String(lastError);
      throw lastError;
    }

    const timeoutError = new Error('Timed out waiting for OpenCode to become ready');
    state.lastOpenCodeError = timeoutError.message;
    throw timeoutError;
  };

  /**
   * 等待 /agent 列表出现指定名称的 agent（配置刷新后新 agent 生效的
   * 标志）；轮询直到超时，超时抛错。
   * @param {string} agentName 目标 agent 名称
   * @param {number} [timeoutMs] 总超时毫秒数
   * @param {number} [intervalMs] 轮询间隔毫秒数
   * @returns {Promise<void>}
   */
  const waitForAgentPresence = async (agentName, timeoutMs = 15000, intervalMs = 300) => {
    if (!state.openCodePort) {
      throw new Error('OpenCode port is not available');
    }

    const deadline = Date.now() + timeoutMs;
    while (Date.now() < deadline) {
      try {
        const response = await fetch(buildOpenCodeUrl('/agent'), {
          method: 'GET',
          headers: { Accept: 'application/json', ...getOpenCodeAuthHeaders() },
        });

        if (response.ok) {
          const agents = await response.json();
          if (Array.isArray(agents) && agents.some((agent) => agent?.name === agentName)) {
            return;
          }
        }
      } catch {
      }

      await new Promise((resolve) => setTimeout(resolve, intervalMs));
    }

    throw new Error(`Agent "${agentName}" not available after OpenCode restart`);
  };

  /**
   * 配置写盘后刷新 OpenCode：清除二进制缓存并重启引擎，等待就绪
   * （受管模式且指定 agentName 时还会等该 agent 出现）。外部服务器
   * 不归本进程管理，重启只做健康重探——返回 reloaded:false 如实告知
   * 调用方：新配置在盘上，但需用户自行重启外部服务器才生效。
   * @param {string} reason 刷新原因（日志用）
   * @param {{agentName?: string}} [options] agentName 指定要等待的 agent
   * @returns {Promise<{reloaded: boolean, external: boolean}>}
   */
  const refreshOpenCodeAfterConfigChange = async (reason, options = {}) => {
    const { agentName } = options;

    console.log(`Refreshing OpenCode after ${reason}`);
    clearResolvedOpenCodeBinary();


    await restartOpenCode(reason || 'config-change');

    // A managed OpenCode process is restarted (and thus re-reads config from
    // disk) by restartOpenCode(). An external OpenCode server is NOT owned by
    // OMPChamber: restartOpenCode() only re-probes its health, so the freshly
    // written config is on disk but the running server keeps serving its old,
    // startup-cached config until the user restarts it themselves. Report this
    // honestly so callers don't claim the change is live.
    const external = state.isExternalOpenCode === true;

    try {
      await waitForOpenCodeReady();
      state.isOpenCodeReady = true;
      state.openCodeNotReadySince = 0;

      // Waiting for the agent to appear only makes sense when we actually
      // reloaded config. An external server will never surface it here.
      if (agentName && !external) {
        await waitForAgentPresence(agentName);
      }

      state.isOpenCodeReady = true;
      state.openCodeNotReadySince = 0;
    } catch (error) {
      state.isOpenCodeReady = false;
      state.openCodeNotReadySince = Date.now();
      console.error(`Failed to refresh OpenCode after ${reason}:`, error.message);
      throw error;
    }

    return { reloaded: !external, external };
  };

  /**
   * 服务启动时的引擎引导：先回收上次运行遗留的孤儿引擎进程，然后按
   * 优先级选择来源——HMR 状态里仍健康的受管进程 → 显式 skip-start 的
   * 外部服务器 → 环境端口探测命中的外部服务器 → 否则启动自己的受管
   * 实例；最后统一等待端口与就绪。任何失败都不抛出（记录错误后以无
   * OpenCode 集成的方式继续运行），成功则异步触发目录预热。
   * @returns {Promise<void>}
   */
  const bootstrapOpenCodeAtStartup = async () => {
    const bootstrapStartedAt = performance.now();
    let bootstrapError = null;
    recordStartupPerformance('opencode.bootstrap.start');
    try {
      // Before doing anything, reap any OpenCode process WE spawned in a prior
      // run that was orphaned by a crash/hard-exit. Verified + scoped to our own
      // pids, so it never touches a live instance's or the user's own server.
      try {
        const orphanReapStartedAt = performance.now();
        const { reaped } = await reapManagedOrphanedProcesses({ log: (msg) => console.log(msg) });
        recordStartupPerformance('opencode.orphan-reap.ready', {
          durationMs: performance.now() - orphanReapStartedAt,
          totalDurationMs: performance.now() - bootstrapStartedAt,
        });
        if (reaped > 0) console.log(`[lifecycle] startup reaped ${reaped} orphaned OpenCode process(es)`);
      } catch (error) {
        console.warn('[lifecycle] orphan reap failed:', error?.message ?? error);
      }

      syncFromHmrState();
      if (await isOpenCodeProcessHealthy()) {
        console.log(`[HMR] Reusing existing OpenCode process on port ${state.openCodePort}`);
      } else if (env.ENV_SKIP_OPENCODE_START && env.ENV_EFFECTIVE_PORT) {
        const label = env.ENV_CONFIGURED_OPENCODE_HOST ? env.ENV_CONFIGURED_OPENCODE_HOST.origin : `http://localhost:${env.ENV_EFFECTIVE_PORT}`;
        console.log(`Using external OpenCode server at ${label} (skip-start mode)`);
        state.openCodeBaseUrl = env.ENV_CONFIGURED_OPENCODE_HOST?.origin ?? null;
        setOpenCodePort(env.ENV_EFFECTIVE_PORT);
        state.isOpenCodeReady = true;
        state.isExternalOpenCode = true;
        state.lastOpenCodeError = null;
        state.openCodeNotReadySince = 0;
        syncToHmrState();
      } else if (env.ENV_EFFECTIVE_PORT && await probeExternalOpenCode(env.ENV_EFFECTIVE_PORT, env.ENV_CONFIGURED_OPENCODE_HOST?.origin)) {
        const label = env.ENV_CONFIGURED_OPENCODE_HOST ? env.ENV_CONFIGURED_OPENCODE_HOST.origin : `http://localhost:${env.ENV_EFFECTIVE_PORT}`;
        console.log(`Auto-detected existing OpenCode server at ${label}`);
        state.openCodeBaseUrl = env.ENV_CONFIGURED_OPENCODE_HOST?.origin ?? null;
        setOpenCodePort(env.ENV_EFFECTIVE_PORT);
        state.isOpenCodeReady = true;
        state.isExternalOpenCode = true;
        state.lastOpenCodeError = null;
        state.openCodeNotReadySince = 0;
        syncToHmrState();
      } else {
        // We never auto-attach to an arbitrary pre-existing OpenCode instance.
        // Attaching to an external server requires explicit opt-in via env
        // (OPENCODE_HOST / OPENCODE_PORT / OPENCODE_SKIP_START), handled by the
        // branches above. Without that opt-in we always start our OWN managed
        // instance on a freshly-allocated port. A blind probe of the default
        // port 4096 used to hijack a user's separately-running OpenCode (e.g.
        // the OpenCode desktop app), coupling our lifecycle to theirs and
        // breaking init against an unexpected server version/config.
        if (env.ENV_EFFECTIVE_PORT) {
          console.log(`Using OpenCode port from environment: ${env.ENV_EFFECTIVE_PORT}`);
          setOpenCodePort(env.ENV_EFFECTIVE_PORT);
        } else {
          state.openCodePort = null;
          syncToHmrState();
        }

        state.lastOpenCodeError = null;
        state.openCodeProcess = await startOpenCode();
        syncToHmrState();
      }
      await waitForOpenCodePort();
      try {
        await waitForOpenCodeReady();
      } catch (error) {
        bootstrapError = error;
        console.error(`OpenCode readiness check failed: ${error.message}`);
      }
    } catch (error) {
      bootstrapError = error;
      console.error(`Failed to start OpenCode: ${error.message}`);
      console.log('Continuing without OpenCode integration...');
      state.lastOpenCodeError = error.message;
    }
    recordStartupPerformance(
      bootstrapError ? 'opencode.bootstrap.error' : 'opencode.bootstrap.ready',
      {
        totalDurationMs: performance.now() - bootstrapStartedAt,
        outcome: bootstrapError ? 'error' : 'ready',
      },
    );
    if (!bootstrapError) {
      void warmOpenCodeDirectories();
    }
  };

  // OpenCode initializes each project directory lazily on its first
  // directory-scoped request, and that initialization takes seconds on large
  // session stores. Without warming, the user's first session open pays it
  // interactively (the chat waits on the message fetch until the directory
  // finishes initializing). Warm the most recently used directories right
  // after readiness so the work overlaps UI startup instead. Sequential and
  // best-effort: a failed or slow directory never blocks the others for long,
  // and a restart invalidates the pass via the port/readiness guard.
  /**
   * 预热最近使用的项目目录：顺序、尽力而为地请求 /session/status，
   * 让引擎在后台完成目录的惰性初始化，避免用户首次打开会话时交互式
   * 等待。单个失败/超时静默跳过；引擎重启（就绪位或端口变化）会中止。
   * @returns {Promise<void>}
   */
  const warmOpenCodeDirectories = async () => {
    let directories = [];
    try {
      directories = await getWarmupDirectories();
    } catch {
      return;
    }
    if (!Array.isArray(directories) || directories.length === 0) return;

    const warmedPort = state.openCodePort;
    for (const directory of directories.slice(0, WARMUP_DIRECTORY_LIMIT)) {
      if (typeof directory !== 'string' || !directory) continue;
      if (!state.isOpenCodeReady || state.openCodePort !== warmedPort) return;
      let timeout = null;
      try {
        const controller = new AbortController();
        timeout = setTimeout(() => controller.abort(), WARMUP_REQUEST_TIMEOUT_MS);
        const url = `${buildOpenCodeUrl('/session/status', '')}?directory=${encodeURIComponent(directory)}`;
        await fetch(url, {
          method: 'GET',
          headers: { Accept: 'application/json', ...getOpenCodeAuthHeaders() },
          signal: controller.signal,
        });
      } catch {
        // Best-effort — the directory stays lazy and the UI's own request warms it.
      } finally {
        if (timeout) clearTimeout(timeout);
      }
    }
  };

  /**
   * Perform an immediate (one-shot) health check and restart OpenCode if it's
   * not healthy.  Callers on the SSE / WS proxy path use this to trigger
   * recovery without waiting for the next periodic interval (up to 15 s).
   *
   * Skips restart when sessions are actively busy — a busy server under
   * concurrent load can fail the health check timeout without actually
   * being dead (the health endpoint competes with LLM work).
   * Forces restart if sessions stay "busy" and the server stays unhealthy
   * for over 2 minutes (staleness guard against stuck session state).
   */
  // 「不健康 + busy 会话」状态允许持续的最大时长，超过则强制重启。
  const STALE_BUSY_GRACE_MS = 2 * 60 * 1000;
  // 最近一次「不健康且存在 busy 会话」的时间戳；0 表示当前无此状态。
  let lastUnhealthyWithBusySessionsAt = 0;
  // 连续健康失败计数，达到阈值后触发重启。
  let consecutiveHealthFailures = 0;
  // 最近一次计入连续失败的时间戳，用于限制计数频率。
  let lastCountedHealthFailureAt = 0;
  // 进行中的健康探测 Promise（并发去重用）。
  let healthProbePromise = null;
  // 进行中的健康检查周期 Promise（并发去重用）。
  let healthCheckCyclePromise = null;
  // 最近一次健康探测结果（带时间戳，用于短缓存）。
  let lastHealthProbeResult = null;
  // 连续失败计数的最小间隔毫秒数；初值 15s，startHealthMonitoring 会同步为实际间隔。
  let healthFailureCountIntervalMs = 15_000;

  /** 复位健康失败相关状态：连续失败计数、busy 会话时间戳与计数间隔锚点。 */
  const resetHealthFailureState = () => {
    consecutiveHealthFailures = 0;
    lastUnhealthyWithBusySessionsAt = 0;
    lastCountedHealthFailureAt = 0;
  };

  /**
   * 带缓存与并发去重的健康探测：HEALTH_CHECK_RESULT_CACHE_MS 内直接
   * 返回上次结果；已有探测在途时复用同一个 Promise。
   * @returns {Promise<{healthy: boolean, failure: object|null, at?: number}>}
   */
  const probeOpenCodeHealth = async () => {
    const checkedAt = now();
    if (lastHealthProbeResult && checkedAt - lastHealthProbeResult.at < HEALTH_CHECK_RESULT_CACHE_MS) {
      return lastHealthProbeResult;
    }

    if (healthProbePromise) {
      return healthProbePromise;
    }

    healthProbePromise = probeOpenCodeHealthDetailed()
      .then((result) => {
        lastHealthProbeResult = { at: now(), ...result };
        return lastHealthProbeResult;
      })
      .finally(() => {
        healthProbePromise = null;
      });

    return healthProbePromise;
  };

  /**
   * 决定不健康时是否因 busy 会话而暂缓重启：无 busy 会话则不跳过；
   * 有 busy 会话先跳过并记录起点，持续超过 STALE_BUSY_GRACE_MS 后
   * 不再跳过（staleBusy=true，强制重启以打破卡死的会话状态）。
   * @returns {{skip: boolean, staleBusy: boolean}}
   */
  const shouldSkipRestartForBusySessions = () => {
    const activeCount = getActiveSessionCount();
    if (activeCount === 0) {
      lastUnhealthyWithBusySessionsAt = 0;
      return { skip: false, staleBusy: false };
    }

    const checkedAt = now();
    if (!lastUnhealthyWithBusySessionsAt) {
      lastUnhealthyWithBusySessionsAt = checkedAt;
      return { skip: true, staleBusy: false };
    }

    if (checkedAt - lastUnhealthyWithBusySessionsAt >= STALE_BUSY_GRACE_MS) {
      console.warn(
        `[lifecycle] OpenCode unhealthy with ${activeCount} busy session(s) for > 2 min — forcing restart`
      );
      lastUnhealthyWithBusySessionsAt = 0;
      return { skip: false, staleBusy: true };
    }

    return { skip: true, staleBusy: false };
  };

  /**
   * 执行一轮健康检查（periodic 定时器与 immediate 触发共用）：健康时
   * 复位失败状态，并补齐启动窗口漏掉的 ready 标记；不健康时——进程
   * 已退出则直接重启，进程存活则按最小间隔累计连续失败，达到阈值且
   * 未被 busy 会话抑制时重启。通过 healthCheckCyclePromise 去重并发
   * 调用；无探测目标、正在关闭或正在重启时直接返回。
   * @param {string} source 调用来源标识（periodic/immediate，用于日志与诊断）
   * @returns {Promise<void>}
   */
  const runHealthCheckCycle = async (source) => {
    // Probe whenever there is something to probe: the spawned child handle,
    // or (after a missed startup health window left the handle unset) a
    // managed engine still listening on the recorded port.
    const hasProbeTarget = Boolean(state.openCodeProcess)
      || (Boolean(state.openCodePort) && !state.isExternalOpenCode);
    if (!hasProbeTarget || state.isShuttingDown || state.isRestartingOpenCode) return;
    if (healthCheckCyclePromise) return healthCheckCyclePromise;

    healthCheckCyclePromise = (async () => {
      const healthResult = await probeOpenCodeHealth();
      if (!healthResult.healthy) {
        if (!state.openCodeProcess) {
          // No child handle (missed startup window): recovery is adoption
          // only — never restart an engine whose liveness we cannot judge.
          return;
        }
        if (!isManagedOpenCodeProcessAlive()) {
          console.log(`[lifecycle] ${source} health check: OpenCode process exited, restarting...`);
          consecutiveHealthFailures = 0;
          lastHealthProbeResult = null;
          await restartOpenCode(`${source}-process-exited`);
          return;
        }
        const checkedAt = now();
        if (lastCountedHealthFailureAt && checkedAt - lastCountedHealthFailureAt < healthFailureCountIntervalMs) {
          return;
        }
        lastCountedHealthFailureAt = checkedAt;
        consecutiveHealthFailures += 1;
        const healthFailure = healthResult.failure || {
          class: 'error',
          detail: 'Health check failed without diagnostic detail',
        };
        state.lastOpenCodeHealthFailure = {
          class: healthFailure.class,
          detail: healthFailure.detail,
          at: new Date(checkedAt).toISOString(),
          source,
        };
        console.warn(
          `[lifecycle] ${source} health check failed (${consecutiveHealthFailures}/${HEALTH_CHECK_MAX_CONSECUTIVE_FAILURES}) class=${healthFailure.class}`
        );
        if (consecutiveHealthFailures < HEALTH_CHECK_MAX_CONSECUTIVE_FAILURES) return;
        const busyDecision = shouldSkipRestartForBusySessions();
        if (busyDecision.skip) return;
        console.log(`[lifecycle] ${source} health check failure threshold reached, restarting OpenCode...`);
        consecutiveHealthFailures = 0;
        lastHealthProbeResult = null;
        await restartOpenCode(
          busyDecision.staleBusy
            ? `${source}-stale-busy-health-failure`
            : `${source}-health-failure`,
        );
      } else {
        resetHealthFailureState();
        // Recovery: a healthy managed engine must clear a missed startup
        // window — without this, isOpenCodeReady stays false forever and the
        // /api readiness gate 503s a serving engine.
        if (!state.isOpenCodeReady && !state.isExternalOpenCode) {
          state.isOpenCodeReady = true;
          state.openCodeNotReadySince = 0;
          state.lastOpenCodeError = null;
          state.lastOpenCodeHealthFailure = null;
          console.log(`[lifecycle] ${source} health check: engine healthy, marking OpenCode ready`);
          syncToHmrState();
        }
      }
    })().finally(() => {
      healthCheckCyclePromise = null;
    });

    return healthCheckCyclePromise;
  };

  /** 立即触发一轮健康检查（SSE/WS 代理路径使用）；错误只记录不抛出。 */
  const triggerHealthCheck = async () => {
    try {
      await runHealthCheckCycle('immediate');
    } catch (error) {
      console.error(`[lifecycle] immediate health check error: ${error.message}`);
    }
  };

  /**
   * 启动周期健康监控：清掉旧定时器，按环境变量覆盖值或传入默认值设置
   * 间隔，周期执行 runHealthCheckCycle，并同步失败计数的最小间隔。
   * @param {number} healthCheckIntervalMs 默认检查间隔毫秒数
   */
  const startHealthMonitoring = (healthCheckIntervalMs) => {
    if (state.healthCheckInterval) {
      clearInterval(state.healthCheckInterval);
    }

    const effectiveIntervalMs = HEALTH_CHECK_INTERVAL_OVERRIDE_MS || healthCheckIntervalMs;
    healthFailureCountIntervalMs = effectiveIntervalMs;

    state.healthCheckInterval = setInterval(async () => {
      try {
        await runHealthCheckCycle('periodic');
      } catch (error) {
        console.error(`Health check error: ${error.message}`);
      }
    }, effectiveIntervalMs);
  };

  // 暴露给 server 的生命周期 API。
  return {
    killProcessOnPort,
    startOpenCode,
    restartOpenCode,
    waitForOpenCodeReady,
    waitForAgentPresence,
    refreshOpenCodeAfterConfigChange,
    bootstrapOpenCodeAtStartup,
    startHealthMonitoring,
    triggerHealthCheck,
    waitForPortRelease,
  };
};
