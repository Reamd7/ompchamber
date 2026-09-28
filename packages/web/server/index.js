/**
 * @module index
 * OMPChamber 旧版 Node/express web server 入口（参考实现；新实现为
 * packages/web/server-rs 的 Rust 版）。
 *
 * 职责：把几十个 lib/* runtime 组装成一个可运行的服务——
 * - 启动/重启受管 OpenCode 进程并维护其网络与鉴权状态（端口探测、
 *   API prefix、login shell 环境、二进制解析）；
 * - 装配 express：基础状态/鉴权路由、SSE 与 WebSocket 消息流、反向代理、
 *   静态资源、feature 路由（设置/主题/项目/agent memory/定时任务等）；
 * - 周边能力：Web Push / APNs 通知、终端与听写、隧道（Cloudflare/ngrok）、
 *   私有 relay、远程客户端配对、worktree 监听、浏览器控制广播；
 * - 大量可变状态通过 globalThis 上的 HMR state 在 dev-server 热更新后存活，
 *   避免产生僵尸 OpenCode 进程。
 *
 * CLI 直跑（node index.js）时由 runCliEntryIfMain 解析参数并调用 main；
 * 也作为库导出 startWebUiServer 等供 Electron 宿主使用。
 */
import 'reflect-metadata';
import express from 'express';
import compression from 'compression';
import path from 'path';
import { spawn, spawnSync } from 'child_process';
import fs from 'fs';
import http from 'http';
import net from 'net';
import { fileURLToPath } from 'url';
import os from 'os';
import crypto from 'crypto';
import http2 from 'node:http2';
import { createUiAuth } from './lib/ui-auth/ui-auth.js';
import { createTunnelAuth } from './lib/opencode/tunnel-auth.js';
import { createManagedTunnelConfigRuntime } from './lib/tunnels/managed-config.js';
import { createTunnelProviderRegistry } from './lib/tunnels/registry.js';
import { createCloudflareTunnelProvider } from './lib/tunnels/providers/cloudflare.js';
import { createNgrokTunnelProvider } from './lib/tunnels/providers/ngrok.js';
import { createRequestSecurityRuntime } from './lib/security/request-security.js';
import {
  getUnauthenticatedLanErrorMessage,
  isNetworkExposedBindHost,
  isUnsafeUnauthenticatedLanAllowed,
  isDevelopmentServer,
} from './lib/security/bind-host.js';
import {
  TUNNEL_MODE_MANAGED_LOCAL,
  TUNNEL_MODE_MANAGED_REMOTE,
  TUNNEL_MODE_QUICK,
  TUNNEL_PROVIDER_CLOUDFLARE,
  TunnelServiceError,
  isSupportedTunnelMode,
  normalizeOptionalPath,
  normalizeTunnelStartRequest,
  normalizeTunnelMode,
  normalizeTunnelProvider,
} from './lib/tunnels/types.js';
import { prepareNotificationLastMessage } from './lib/notifications/index.js';
import { registerTtsRoutes } from './lib/tts/routes.js';
import { detectSayTtsCapability } from './lib/tts/capability-runtime.js';
import { createTerminalRuntime } from './lib/terminal/runtime.js';
import { createDictationRuntime } from './lib/dictation/runtime.js';
import {
  createGlobalUiEventBroadcaster,
  createGlobalMessageStreamHub,
  createMessageStreamWsRuntime,
  DEFAULT_UPSTREAM_STALL_TIMEOUT_MS,
  UPSTREAM_STALL_TIMEOUT_CONCURRENT_MS,
} from './lib/event-stream/index.js';
import { createFsSearchRuntime as createFsSearchRuntimeFactory } from './lib/fs/search.js';
import { createOpenCodeLifecycleRuntime } from './lib/opencode/lifecycle.js';
import { createOpenCodeEnvRuntime } from './lib/opencode/env-runtime.js';
import { resolveOpenCodeEnvConfig } from './lib/opencode/env-config.js';
import { createHmrStateRuntime } from './lib/opencode/hmr-state-runtime.js';
import { createOpenCodeNetworkRuntime } from './lib/opencode/network-runtime.js';
import { createOpenCodeAuthStateRuntime } from './lib/opencode/auth-state-runtime.js';
import { createProjectDirectoryRuntime } from './lib/opencode/project-directory-runtime.js';
import { createSettingsNormalizationRuntime } from './lib/opencode/settings-normalization-runtime.js';
import { createSettingsHelpers } from './lib/opencode/settings-helpers.js';
import { createThemeRuntime } from './lib/opencode/theme-runtime.js';
import { createFeatureRoutesRuntime } from './lib/opencode/feature-routes-runtime.js';
import { parseServeCliOptions } from './lib/opencode/cli-options.js';
import {
  registerAuthAndAccessRoutes,
  registerCommonRequestMiddleware,
  registerServerStatusRoutes,
} from './lib/opencode/core-routes.js';
import { registerOMPChamberRoutes } from './lib/opencode/openchamber-routes.js';
import { createServerUtilsRuntime } from './lib/opencode/server-utils-runtime.js';
import { createStaticRoutesRuntime } from './lib/opencode/static-routes-runtime.js';
import { createSettingsRuntime } from './lib/opencode/settings-runtime.js';
import { createOpenCodeResolutionRuntime } from './lib/opencode/opencode-resolution-runtime.js';
import { createBootstrapRuntime } from './lib/opencode/bootstrap-runtime.js';
import { createSessionRuntime } from './lib/opencode/session-runtime.js';
import { configureOpenCodeRuntimeProviders, resetOpenCodeRuntimeProviders } from './lib/small-model/runtime-providers.js';
import { createOpenCodeWatcherRuntime } from './lib/opencode/watcher.js';
import { createSessionAssistRuntime } from './lib/session-assist/runtime.js';
import { createSessionGoalRuntime } from './lib/session-goal/runtime.js';
import { createContextObligatoryRuntime } from './lib/context-obligatory/runtime.js';
import { createLinearSessionStatusRuntime } from './lib/linear/status-runtime.js';
import { createSessionKnowledgeRuntime } from './lib/session-knowledge/runtime.js';
import { createScheduledTasksRuntime } from './lib/scheduled-tasks/runtime.js';
import { createServerStartupRuntime } from './lib/opencode/server-startup-runtime.js';
import { createTunnelWiringRuntime } from './lib/opencode/tunnel-wiring-runtime.js';
import { createStartupPipelineRuntime } from './lib/opencode/startup-pipeline-runtime.js';
import { runCliEntryIfMain } from './lib/opencode/cli-entry-runtime.js';
import { registerNotificationRoutes } from './lib/notifications/routes.js';
import { createNotificationEmitterRuntime } from './lib/notifications/emitter-runtime.js';
import { createNotificationTriggerRuntime } from './lib/notifications/runtime.js';
import { createPushRuntime } from './lib/notifications/push-runtime.js';
import { createApnsRuntime } from './lib/notifications/apns-runtime.js';
import { createNotificationTemplateRuntime } from './lib/notifications/template-runtime.js';
import { createPermissionAutoAcceptRuntime } from './lib/permission-auto-accept/runtime.js';
import { createGracefulShutdownRuntime } from './lib/opencode/shutdown-runtime.js';
import { createWorktreeWatcher } from './lib/git/worktree-watcher.js';
import { createProjectConfigRuntime } from './lib/projects/project-config.js';
import { createProjectContextRuntime } from './lib/project-context/runtime.js';
import { createAgentMemoryRuntime } from './lib/agent-memory/runtime.js';
import { createAgentMemoryActions } from './lib/agent-memory/actions.js';
import { createMemoryProjectResolver } from './lib/agent-memory/project-resolution.js';
import { isAgentMemoryFeatureAvailable } from './lib/agent-memory/feature-flag.js';
import { resolvePrimaryWorktreeRoot } from './lib/git/service.js';
import { createRemoteClientAuthRuntime } from './lib/client-auth/remote-clients.js';
import { createClientPairingRuntime } from './lib/client-auth/pairing.js';
import { attachRealtimeProxy } from './lib/realtime-proxy.js';
import { createRelayService } from './lib/relay/service.js';
import { createRelayHostLock } from './lib/relay/host-lock.js';
import { createAgentToolRuntime } from './lib/agent-tool/runtime.js';
import { createBrowserControlBroker } from './lib/browser-control/broker.js';
import { createDevServerScanner } from './lib/dev-servers/routes.js';
import { createDevTunnelRuntime } from './lib/dev-tunnel/runtime.js';
import { registerBrowserControlRoutes } from './lib/browser-control/routes.js';
import { createOMPChamberSessionService } from './lib/openchamber-sessions/routes.js';
import { createScheduledTaskService } from './lib/scheduled-tasks/service.js';
import { createOMPChamberControlService } from './lib/openchamber-control/service.js';
import { OMPChamberControlError } from './lib/openchamber-control/error.js';
import webPush from 'web-push';

/** 当前模块文件的绝对路径（ESM 下等价 CommonJS 的 __filename）。 */
const __filename = fileURLToPath(import.meta.url);
/** 当前模块所在目录（用于定位 package.json 版本号与静态资源）。 */
const __dirname = path.dirname(__filename);

/** 默认监听端口（调用方未指定 port 时使用）。 */
const DEFAULT_PORT = 3000;
/** 桌面通知前缀：stdout 中以此开头的行会被 Electron 宿主转成系统通知。 */
const DESKTOP_NOTIFY_PREFIX = '[OMPChamberDesktopNotify] ';
/** 通知 SSE 客户端集合（HTTP 长连接），writeSseEvent 直接向其写事件。 */
const uiNotificationClients = new Set();
/** 通知 WebSocket 客户端集合（消息流 WS 复用同一广播通道）。 */
const uiNotificationWsClients = new Set();
/** OMPChamber 领域事件（session/worktree/agent-memory 等）的 SSE 客户端集合。 */
const uiOMPChamberEventClients = new Set();
/** 受管 OpenCode 的健康检查轮询间隔（15 秒）。 */
const HEALTH_CHECK_INTERVAL = 15000;
/** 优雅停机硬超时：超过 10 秒仍未完成则强制退出进程。 */
const SHUTDOWN_TIMEOUT = 10000;
/** models.dev 模型元数据 API 地址（补充 provider/model 描述信息）。 */
const MODELS_DEV_API_URL = 'https://models.dev/api.json';
/** models.dev 元数据内存缓存 TTL（5 分钟）。 */
const MODELS_METADATA_CACHE_TTL = 5 * 60 * 1000;
/** 构建产物变化后通知 UI 客户端延迟刷新前的等待时间。 */
const CLIENT_RELOAD_DELAY_MS = 800;
/** OpenCode 就绪宽限期：刚就绪后这段时间内不判为不健康，避免抖动。 */
const OPEN_CODE_READY_GRACE_MS = 12000;
/** 长请求超时预算（4 分钟）：慢上游与长 turn 代理请求的 deadline。 */
const LONG_REQUEST_TIMEOUT_MS = 4 * 60 * 1000;
/** 隧道 bootstrap 会话默认 TTL（30 分钟）。 */
const TUNNEL_BOOTSTRAP_TTL_DEFAULT_MS = 30 * 60 * 1000;
/** 隧道 bootstrap 会话最小 TTL（1 分钟）。 */
const TUNNEL_BOOTSTRAP_TTL_MIN_MS = 60 * 1000;
/** 隧道 bootstrap 会话最大 TTL（24 小时）。 */
const TUNNEL_BOOTSTRAP_TTL_MAX_MS = 24 * 60 * 60 * 1000;
/** 隧道会话默认 TTL（8 小时）。 */
const TUNNEL_SESSION_TTL_DEFAULT_MS = 8 * 60 * 60 * 1000;
/** 隧道会话最小 TTL（5 分钟）。 */
const TUNNEL_SESSION_TTL_MIN_MS = 5 * 60 * 1000;
/** 隧道会话最大 TTL（30 天）。 */
const TUNNEL_SESSION_TTL_MAX_MS = 30 * 24 * 60 * 60 * 1000;

/**
 * 判断某个 header 值（字符串或数组）是否包含 text/event-stream。
 * 用于 SSE 请求/响应的压缩豁免判定。
 * @param {string | string[]} value header 值
 * @returns {boolean} 含 text/event-stream（大小写不敏感）时为 true
 */
function headerIncludesEventStream(value) {
  if (typeof value === 'string') {
    return value.toLowerCase().includes('text/event-stream');
  }

  if (Array.isArray(value)) {
    return value.some((entry) => typeof entry === 'string' && entry.toLowerCase().includes('text/event-stream'));
  }

  return false;
}

/**
 * 必须绕过 compression 中间件的 SSE 端点路径（精确匹配）。
 * 中文补充：compression 的 filter 先于路由执行，此时 Content-Type 尚未
 * 设置，单靠 Accept 头判断对 curl/fetch 等省略 Accept 的客户端不可靠，
 * 因此按路径做确定性兜底（英文原注释见下）。
 */
/**
 * SSE endpoint paths that must never be compressed by the compression middleware.
 *
 * The compression middleware filter runs before route handlers, so
 * `res.getHeader('Content-Type')` is still undefined at that point.
 * This means the Accept-header check alone is not sufficient for
 * non-standard clients (e.g. curl, fetch) that omit Accept.
 * Path-based exclusion acts as a deterministic fallback.
 */
const SSE_PATH_PREFIXES = [
  '/api/event',
  '/api/global/event',
  '/api/notifications/stream',
  '/api/ompchamber/events',
  '/api/ompchamber/realtime-proxy/sse',
];

/**
 * compression 中间件的过滤函数：判定某请求是否跳过压缩。
 * 依次检查：desktop 运行时（一律跳过）→ Accept 含 text/event-stream →
 * /api 路径且配置了 API 压缩豁免 → SSE 白名单路径 → 响应 Content-Type。
 * @param {import('express').Request} req 请求
 * @param {import('express').Response} res 响应
 * @returns {boolean} true 表示不压缩
 */
function shouldSkipCompression(req, res) {
  if (process.env.OMPCHAMBER_RUNTIME === 'desktop') {
    return true;
  }

  if (headerIncludesEventStream(req.headers.accept)) {
    return true;
  }

  const pathname = req.path || req.url || '';
  if ((pathname === '/api' || pathname.startsWith('/api/')) && shouldSkipApiCompression()) {
    return true;
  }

  for (const prefix of SSE_PATH_PREFIXES) {
    if (pathname === prefix) {
      return true;
    }
  }

  return headerIncludesEventStream(res.getHeader('Content-Type'));
}

/**
 * 当前 OMPChamber 版本号：读取上级 package.json 的 version 字段，
 * 读取或解析失败时回退为 'unknown'。
 */
const OMPCHAMBER_VERSION = (() => {
  try {
    const packagePath = path.resolve(__dirname, '..', 'package.json');
    const raw = fs.readFileSync(packagePath, 'utf8');
    const pkg = JSON.parse(raw);
    if (pkg && typeof pkg.version === 'string' && pkg.version.trim().length > 0) {
      return pkg.version.trim();
    }
  } catch {
  }
  return 'unknown';
})();

/**
 * 判断环境变量风格的开关值是否为「启用」：true/1，或字符串 '1'/'true'
 * （忽略大小写与首尾空白）；其余一律视为未启用。
 * @param {unknown} value 环境变量值
 * @returns {boolean} 是否启用
 */
const isEnvFlagEnabled = (value) => {
  if (value === true || value === 1) return true;
  if (typeof value !== 'string') return false;
  const normalized = value.trim().toLowerCase();
  return normalized === '1' || normalized === 'true';
};

/**
 * 判断环境变量风格的开关值是否为「显式禁用」：false/0，或字符串
 * '0'/'false'；与 isEnvFlagEnabled 对称，用于区分「未设置」与「关闭」。
 * @param {unknown} value 环境变量值
 * @returns {boolean} 是否禁用
 */
const isEnvFlagDisabled = (value) => {
  if (value === false || value === 0) return true;
  if (typeof value !== 'string') return false;
  const normalized = value.trim().toLowerCase();
  return normalized === '0' || normalized === 'false';
};

/**
 * /api 路径是否跳过压缩：OMPCHAMBER_SKIP_API_COMPRESSION=1 强制跳过；
 * OMPCHAMBER_COMPRESS_API 显式开/关优先；默认 desktop 运行时跳过，
 * 其余保持开启。
 * @returns {boolean} true 表示 /api 不压缩
 */
const shouldSkipApiCompression = () => {
  if (isEnvFlagEnabled(process.env.OMPCHAMBER_SKIP_API_COMPRESSION)) return true;
  if (isEnvFlagEnabled(process.env.OMPCHAMBER_COMPRESS_API)) return false;
  if (isEnvFlagDisabled(process.env.OMPCHAMBER_COMPRESS_API)) return true;
  return process.env.OMPCHAMBER_RUNTIME === 'desktop';
};

/** 是否输出逐请求的详细访问日志（OMPCHAMBER_VERBOSE_REQUEST_LOGS）。 */
const OMPCHAMBER_VERBOSE_REQUEST_LOGS = isEnvFlagEnabled(process.env.OMPCHAMBER_VERBOSE_REQUEST_LOGS);

/** Plan mode 实验开关：任一实验环境变量启用即视为开启。 */
const PLAN_MODE_EXPERIMENT_ENABLED =
  isEnvFlagEnabled(process.env.OPENCODE_EXPERIMENTAL_PLAN_MODE)
  || isEnvFlagEnabled(process.env.OPENCODE_EXPERIMENTAL);

/** fs.promises 别名：供各 runtime 做异步文件读写。 */
const fsPromises = fs.promises;

/**
 * 设置归一化 runtime：承载路径/TTL/隧道主机名/项目与模型引用等字段的
 * 清洗与归一化逻辑，注入 os/path/fs.realpathSync 与隧道 TTL 边界常量。
 */
const settingsNormalizationRuntime = createSettingsNormalizationRuntime({
  os,
  path,
  processLike: process,
  realpathSync: fs.realpathSync,
  tunnelBootstrapTtlDefaultMs: TUNNEL_BOOTSTRAP_TTL_DEFAULT_MS,
  tunnelBootstrapTtlMinMs: TUNNEL_BOOTSTRAP_TTL_MIN_MS,
  tunnelBootstrapTtlMaxMs: TUNNEL_BOOTSTRAP_TTL_MAX_MS,
  tunnelSessionTtlDefaultMs: TUNNEL_SESSION_TTL_DEFAULT_MS,
  tunnelSessionTtlMinMs: TUNNEL_SESSION_TTL_MIN_MS,
  tunnelSessionTtlMaxMs: TUNNEL_SESSION_TTL_MAX_MS,
});

/** 归一化目录路径（realpath + 稳定分隔符），转发到归一化 runtime。 */
const normalizeDirectoryPath = (...args) => settingsNormalizationRuntime.normalizeDirectoryPath(...args);
/** 归一化需要持久化的路径（保持用户可读形式），转发到归一化 runtime。 */
const normalizePathForPersistence = (...args) => settingsNormalizationRuntime.normalizePathForPersistence(...args);
/** 归一化 settings 中所有路径字段，转发到归一化 runtime。 */
const normalizeSettingsPaths = (...args) => settingsNormalizationRuntime.normalizeSettingsPaths(...args);
/** 归一化隧道 bootstrap TTL（毫秒，夹在 min/max 之间）。 */
const normalizeTunnelBootstrapTtlMs = (...args) => settingsNormalizationRuntime.normalizeTunnelBootstrapTtlMs(...args);
/** 归一化隧道会话 TTL（毫秒，夹在 min/max 之间）。 */
const normalizeTunnelSessionTtlMs = (...args) => settingsNormalizationRuntime.normalizeTunnelSessionTtlMs(...args);
/** 归一化 managed-remote 隧道的主机名（小写、去协议前缀等）。 */
const normalizeManagedRemoteTunnelHostname = (...args) =>
  settingsNormalizationRuntime.normalizeManagedRemoteTunnelHostname(...args);
/** 归一化 managed-remote 隧道 preset 列表。 */
const normalizeManagedRemoteTunnelPresets = (...args) =>
  settingsNormalizationRuntime.normalizeManagedRemoteTunnelPresets(...args);
/** 归一化 managed-remote 隧道 preset 中的 token 字段。 */
const normalizeManagedRemoteTunnelPresetTokens = (...args) =>
  settingsNormalizationRuntime.normalizeManagedRemoteTunnelPresetTokens(...args);
/** 判断 skill 相对路径是否越权（逃出 skill 目录），不安全则拒绝。 */
const isUnsafeSkillRelativePath = (...args) => settingsNormalizationRuntime.isUnsafeSkillRelativePath(...args);
/** 清洗排版字号设置的部分字段（夹取合法范围）。 */
const sanitizeTypographySizesPartial = (...args) =>
  settingsNormalizationRuntime.sanitizeTypographySizesPartial(...args);
/** 把任意输入归一化为字符串数组（剔除非字符串项）。 */
const normalizeStringArray = (...args) => settingsNormalizationRuntime.normalizeStringArray(...args);
/** 清洗 settings 中的模型引用（provider/model ID）。 */
const sanitizeModelRefs = (...args) => settingsNormalizationRuntime.sanitizeModelRefs(...args);
/** 清洗 skill catalog 配置列表。 */
const sanitizeSkillCatalogs = (...args) => settingsNormalizationRuntime.sanitizeSkillCatalogs(...args);
/** 清洗 settings 中的项目列表（路径、时间戳等字段）。 */
const sanitizeProjects = (...args) => settingsNormalizationRuntime.sanitizeProjects(...args);

/** 用户级配置根目录 ~/.config/ompchamber。 */
const OMPCHAMBER_USER_CONFIG_ROOT = path.join(os.homedir(), '.config', 'ompchamber');
/** 用户自定义主题目录（JSON 主题文件）。 */
const OMPCHAMBER_USER_THEMES_DIR = path.join(OMPCHAMBER_USER_CONFIG_ROOT, 'themes');
/** 项目级配置目录（projects/<id>.json）。 */
const OMPCHAMBER_PROJECTS_CONFIG_DIR = path.join(OMPCHAMBER_USER_CONFIG_ROOT, 'projects');

/** 单个主题 JSON 文件的大小上限（512KB），防止异常大文件拖垮解析。 */
const MAX_THEME_JSON_BYTES = 512 * 1024;


/**
 * 主题 runtime：从用户主题目录读取自定义主题（受 MAX_THEME_JSON_BYTES
 * 限制），注入 fs、路径与日志器。
 */
const themeRuntime = createThemeRuntime({
  fsPromises,
  path,
  themesDir: OMPCHAMBER_USER_THEMES_DIR,
  maxThemeJsonBytes: MAX_THEME_JSON_BYTES,
  logger: console,
});

/** 从磁盘读取自定义主题列表，转发到主题 runtime。 */
const readCustomThemesFromDisk = (...args) => themeRuntime.readCustomThemesFromDisk(...args);

/** 通知模板 runtime：在 readSettingsFromDisk 可用后延迟创建（见下方赋值）。 */
let notificationTemplateRuntime = null;
/** agent-tool runtime：main() 启动时创建（需要 server 端口等信息）。 */
let agentToolRuntime = null;

/** 创建带超时的 AbortSignal，转发到通知模板 runtime。 */
const createTimeoutSignal = (...args) => notificationTemplateRuntime.createTimeoutSignal(...args);
/** 把项目目录格式化为通知里展示的项目标签。 */
const formatProjectLabel = (...args) => notificationTemplateRuntime.formatProjectLabel(...args);
/** 解析命中的通知模板（用户自定义或默认）。 */
const resolveNotificationTemplate = (...args) => notificationTemplateRuntime.resolveNotificationTemplate(...args);
/** 判断解析出的模板 message 是否应覆盖默认文案。 */
const shouldApplyResolvedTemplateMessage = (...args) => notificationTemplateRuntime.shouldApplyResolvedTemplateMessage(...args);
/** 拉取 models.dev 的免费 Zen 模型列表。 */
const fetchFreeZenModels = (...args) => notificationTemplateRuntime.fetchFreeZenModels(...args);
/** 从消息 parts 中抽取纯文本内容。 */
const extractTextFromParts = (...args) => notificationTemplateRuntime.extractTextFromParts(...args);
/** 从消息列表抽取最后一条消息文本。 */
const extractLastMessageText = (...args) => notificationTemplateRuntime.extractLastMessageText(...args);
/** 调 OpenCode API 取会话最后一条 assistant 消息文本。 */
const fetchLastAssistantMessageText = (...args) => notificationTemplateRuntime.fetchLastAssistantMessageText(...args);
/** 从 SSE 事件中缓存会话信息（标题等）供通知使用。 */
const maybeCacheSessionInfoFromEvent = (...args) => notificationTemplateRuntime.maybeCacheSessionInfoFromEvent(...args);
/** 组装通知模板变量（项目、会话、模型等）。 */
const buildTemplateVariables = (...args) => notificationTemplateRuntime.buildTemplateVariables(...args);
/** 读取缓存的 Zen 模型列表（带 TTL）。 */
const getCachedZenModels = (...args) => notificationTemplateRuntime.getCachedZenModels(...args);

/**
 * OMPChamber 数据目录：OMPCHAMBER_DATA_DIR 指定时取其绝对路径，
 * 否则默认 ~/.config/ompchamber。settings/push/APNs/隧道等持久化文件都在此。
 */
const OMPCHAMBER_DATA_DIR = process.env.OMPCHAMBER_DATA_DIR
  ? path.resolve(process.env.OMPCHAMBER_DATA_DIR)
  : path.join(os.homedir(), '.config', 'ompchamber');
/** settings.json 的绝对路径（用户设置持久化）。 */
const SETTINGS_FILE_PATH = path.join(OMPCHAMBER_DATA_DIR, 'settings.json');
/** Web Push 订阅列表的持久化文件路径。 */
const PUSH_SUBSCRIPTIONS_FILE_PATH = path.join(OMPCHAMBER_DATA_DIR, 'push-subscriptions.json');
/** APNs device token 列表的持久化文件路径。 */
const APNS_TOKENS_FILE_PATH = path.join(OMPCHAMBER_DATA_DIR, 'apns-tokens.json');
/** 已配对远程客户端信息的持久化文件路径。 */
const REMOTE_CLIENTS_FILE_PATH = path.join(OMPCHAMBER_DATA_DIR, 'remote-clients.json');
/** 配对会话（pending pairing sessions）的持久化文件路径。 */
const CLIENT_PAIRING_SESSIONS_FILE_PATH = path.join(OMPCHAMBER_DATA_DIR, 'client-pairing-sessions.json');
/** Cloudflare managed-remote 隧道配置文件路径。 */
const CLOUDFLARE_MANAGED_REMOTE_TUNNELS_FILE_PATH = path.join(OMPCHAMBER_DATA_DIR, 'cloudflare-managed-remote-tunnels.json');
/** 旧版 Cloudflare named 隧道配置文件路径（迁移源）。 */
const CLOUDFLARE_LEGACY_NAMED_TUNNELS_FILE_PATH = path.join(OMPCHAMBER_DATA_DIR, 'cloudflare-named-tunnels.json');
/** managed-remote 隧道配置文件的 schema 版本号。 */
const CLOUDFLARE_MANAGED_REMOTE_TUNNELS_VERSION = 1;

/**
 * managed 隧道配置 runtime：读写 cloudflare-managed-remote-tunnels.json，
 * 与 preset 同步、维护 token；注入归一化函数与新旧两个文件路径常量。
 */
const managedTunnelConfigRuntime = createManagedTunnelConfigRuntime({
  fsPromises,
  path,
  normalizeManagedRemoteTunnelHostname,
  normalizeManagedRemoteTunnelPresets,
  constants: {
    CLOUDFLARE_MANAGED_REMOTE_TUNNELS_FILE_PATH,
    CLOUDFLARE_LEGACY_NAMED_TUNNELS_FILE_PATH,
    CLOUDFLARE_MANAGED_REMOTE_TUNNELS_VERSION,
  },
});

/** 从磁盘读取 managed-remote 隧道配置（含旧版文件迁移）。 */
const readManagedRemoteTunnelConfigFromDisk = (...args) => managedTunnelConfigRuntime.readManagedRemoteTunnelConfigFromDisk(...args);
/** 让磁盘上的隧道配置与 settings 中的 preset 保持同步。 */
const syncManagedRemoteTunnelConfigWithPresets = (...args) => managedTunnelConfigRuntime.syncManagedRemoteTunnelConfigWithPresets(...args);
/** upsert 隧道 token（写入磁盘配置）。 */
const upsertManagedRemoteTunnelToken = (...args) => managedTunnelConfigRuntime.upsertManagedRemoteTunnelToken(...args);
/** 解析当前可用的 managed-remote 隧道 token。 */
const resolveManagedRemoteTunnelToken = (...args) => managedTunnelConfigRuntime.resolveManagedRemoteTunnelToken(...args);

/**
 * settings 辅助函数集合：PWA 字段归一化、settings 更新清洗、持久化设置
 * 合并与对外响应格式化，依赖上面各归一化/清洗函数。
 */
const settingsHelpers = createSettingsHelpers({
  normalizePathForPersistence,
  normalizeDirectoryPath,
  normalizeTunnelBootstrapTtlMs,
  normalizeTunnelSessionTtlMs,
  normalizeTunnelProvider,
  normalizeTunnelMode,
  normalizeOptionalPath,
  normalizeManagedRemoteTunnelHostname,
  normalizeManagedRemoteTunnelPresets,
  normalizeManagedRemoteTunnelPresetTokens,
  sanitizeTypographySizesPartial,
  normalizeStringArray,
  sanitizeModelRefs,
  sanitizeSkillCatalogs,
  sanitizeProjects,
});

/** 归一化 PWA 应用名。 */
const normalizePwaAppName = (...args) => settingsHelpers.normalizePwaAppName(...args);
/** 归一化 PWA 方向（portrait/landscape 等）。 */
const normalizePwaOrientation = (...args) => settingsHelpers.normalizePwaOrientation(...args);
/** 清洗一次 settings 更新的部分字段（剔除非法值）。 */
const sanitizeSettingsUpdate = (...args) => settingsHelpers.sanitizeSettingsUpdate(...args);
/** 合并磁盘上的持久化设置与内存默认值。 */
const mergePersistedSettings = (...args) => settingsHelpers.mergePersistedSettings(...args);
/** 组装对外返回的 settings 响应对象。 */
const formatSettingsResponse = (...args) => settingsHelpers.formatSettingsResponse(...args);

/**
 * 项目目录 runtime：候选目录解析与校验（存在性、路径归一化），
 * 校验时惰性读取最新 settings。
 */
const projectDirectoryRuntime = createProjectDirectoryRuntime({
  fsPromises,
  path,
  normalizeDirectoryPath,
  getReadSettingsFromDiskMigrated: () => readSettingsFromDiskMigrated,
  sanitizeProjects,
});

/** 解析请求中的目录候选值（query/header/body 多来源）。 */
const resolveDirectoryCandidate = (...args) => projectDirectoryRuntime.resolveDirectoryCandidate(...args);
/** 校验目录路径，非法时抛错。 */
const validateDirectoryPath = (...args) => projectDirectoryRuntime.validateDirectoryPath(...args);
/** 解析并返回必填的项目目录（不存在则报错）。 */
const resolveProjectDirectory = (...args) => projectDirectoryRuntime.resolveProjectDirectory(...args);
/** 解析可选的项目目录，缺省时返回 undefined/null。 */
const resolveOptionalProjectDirectory = (...args) => projectDirectoryRuntime.resolveOptionalProjectDirectory(...args);

/**
 * settings 读写 runtime：settings.json 的读取（带迁移）、严格读取、
 * 写入与持久化（先写临时文件再原子替换），注入清洗与隧道配置同步。
 */
const settingsRuntime = createSettingsRuntime({
  fsPromises,
  path,
  crypto,
  SETTINGS_FILE_PATH,
  sanitizeProjects,
  sanitizeSettingsUpdate,
  mergePersistedSettings,
  normalizeSettingsPaths,
  normalizeStringArray,
  formatSettingsResponse,
  resolveDirectoryCandidate,
  normalizeManagedRemoteTunnelHostname,
  normalizeManagedRemoteTunnelPresets,
  normalizeManagedRemoteTunnelPresetTokens,
  syncManagedRemoteTunnelConfigWithPresets,
  upsertManagedRemoteTunnelToken,
});

/** 读取 settings 并按需执行旧版本迁移。 */
const readSettingsFromDiskMigrated = (...args) => settingsRuntime.readSettingsFromDiskMigrated(...args);
/** 宽松读取 settings：失败时返回默认值而非抛错。 */
const readSettingsFromDisk = (...args) => settingsRuntime.readSettingsFromDisk(...args);
/** 严格读取 settings：文件损坏/缺失时抛错（APNs 等强一致路径用）。 */
const readSettingsFromDiskStrict = (...args) => settingsRuntime.readSettingsFromDiskStrict(...args);
/** 把 settings 对象写入磁盘文件。 */
const writeSettingsToDisk = (...args) => settingsRuntime.writeSettingsToDisk(...args);
/** 持久化 settings：合并变更并落盘。 */
const persistSettings = (...args) => settingsRuntime.persistSettings(...args);

/**
 * 请求安全 runtime：从请求提取 UI 会话 token、WebSocket 升级拒绝、
 * Origin 校验等安全策略（读取最新 settings 判定）。
 */
const requestSecurityRuntime = createRequestSecurityRuntime({
  readSettingsFromDiskMigrated,
});

/** 从请求（header/cookie）提取 UI 会话 token。 */
const getUiSessionTokenFromRequest = (...args) => requestSecurityRuntime.getUiSessionTokenFromRequest(...args);

/**
 * Web Push runtime：VAPID 密钥生成与持久化、订阅的增删、按 UI 会话
 * 批量推送；订阅列表落盘到 push-subscriptions.json。
 */
const pushRuntime = createPushRuntime({
  fsPromises,
  path,
  webPush,
  PUSH_SUBSCRIPTIONS_FILE_PATH,
  readSettingsFromDiskMigrated,
  writeSettingsToDisk,
});

/** 读取或生成 VAPID 密钥对（持久化到 settings）。 */
const getOrCreateVapidKeys = (...args) => pushRuntime.getOrCreateVapidKeys(...args);
/** 新增或更新一条 push 订阅。 */
const addOrUpdatePushSubscription = (...args) => pushRuntime.addOrUpdatePushSubscription(...args);
/** 移除一条失效的 push 订阅。 */
const removePushSubscription = (...args) => pushRuntime.removePushSubscription(...args);
/** 向所有 UI 会话发送 Web Push。 */
const sendPushToAllUiSessions = (...args) => pushRuntime.sendPushToAllUiSessions(...args);
// 中文补充：clearPendingPushBadge 在通知触发 runtime 创建后才会被赋真值；
// UI 客户端报告自己变为可见时清空原生推送角标，与设备侧 becomeActive
// 清零角标保持同步（英文原注释见下）。
// Set once the notification trigger runtime exists (declared later). When a UI
// client reports it became visible, reset the native push badge set — the same
// moment the device zeroes its icon badge on becomeActive, keeping them in sync.
let clearPendingPushBadge = () => {};
/**
 * 更新某 UI 会话的可见性：变为可见时同步清空待处理推送角标，
 * 再把状态转发给 push runtime（用于「可见就不推送」的抑制判断）。
 * @param {string} token UI 会话 token
 * @param {boolean} visible 是否可见
 * @param {string} platform 客户端平台标识
 */
const updateUiVisibility = (token, visible, platform) => {
  if (visible === true) clearPendingPushBadge();
  return pushRuntime.updateUiVisibility(token, visible, platform);
};
/** 是否任一 UI 客户端当前可见。 */
const isAnyUiVisible = (...args) => pushRuntime.isAnyUiVisible(...args);
/** 是否任一可交互客户端（UI/桌面）当前可见。 */
const isAnyInteractiveClientVisible = (...args) => pushRuntime.isAnyInteractiveClientVisible(...args);
/** 指定 token 的 UI 客户端是否可见。 */
const isUiVisible = (...args) => pushRuntime.isUiVisible(...args);
/** 确保 push（VAPID/订阅）已初始化（幂等）。 */
const ensurePushInitialized = (...args) => pushRuntime.ensurePushInitialized(...args);
/** 标记 push 初始化完成（初始化流程内部使用）。 */
const setPushInitialized = (...args) => pushRuntime.setPushInitialized(...args);

/**
 * APNs runtime：iOS device token 的增删与按会话推送；基于 node:http2
 * 直连 APNs，token 落盘到 apns-tokens.json，读取设置用严格模式。
 */
const apnsRuntime = createApnsRuntime({
  fsPromises,
  path,
  crypto,
  http2,
  APNS_TOKENS_FILE_PATH,
  readSettingsFromDiskMigrated,
  writeSettingsToDisk,
  readSettingsStrict: readSettingsFromDiskStrict,
});

/** 新增或更新一条 APNs device token。 */
const addOrUpdateApnsToken = (...args) => apnsRuntime.addOrUpdateApnsToken(...args);
/** 移除一条 APNs device token。 */
const removeApnsToken = (...args) => apnsRuntime.removeApnsToken(...args);
/** 向所有 UI 会话发送 APNs 推送。 */
const sendApnsToAllUiSessions = (...args) => apnsRuntime.sendApnsToAllUiSessions(...args);

/** 终端输入 WebSocket 单窗口时间内的最大重绑次数（防泄漏上限）。 */
const TERMINAL_INPUT_WS_MAX_REBINDS_PER_WINDOW = 128;
/** 终端重绑计数的时间窗口（60 秒）。 */
const TERMINAL_INPUT_WS_REBIND_WINDOW_MS = 60 * 1000;
/** 终端输入 WebSocket 心跳间隔（15 秒）。 */
const TERMINAL_INPUT_WS_HEARTBEAT_INTERVAL_MS = 15 * 1000;

/** 拒绝一次 WebSocket 升级请求（安全策略判定不通过时）。 */
const rejectWebSocketUpgrade = (...args) => requestSecurityRuntime.rejectWebSocketUpgrade(...args);


/** 判断请求 Origin 是否被允许（防跨源 WebSocket/请求）。 */
const isRequestOriginAllowed = (...args) => requestSecurityRuntime.isRequestOriginAllowed(...args);

/**
 * 通知发射器 runtime：向 SSE 客户端写事件、发桌面通知（前缀标记行）、
 * 广播 UI 通知；桌面通知开关与前缀由 ENV_DESKTOP_NOTIFY 等注入。
 */
const notificationEmitterRuntime = createNotificationEmitterRuntime({
  process,
  getDesktopNotifyEnabled: () => ENV_DESKTOP_NOTIFY,
  desktopNotifyPrefix: DESKTOP_NOTIFY_PREFIX,
  getUiNotificationClients: () => uiNotificationClients,
  getBroadcastGlobalUiEvent: () => broadcastGlobalUiEvent,
});

/** 向单个 SSE 客户端写一条事件（含断连清理由调用方处理）。 */
const writeSseEvent = (...args) => notificationEmitterRuntime.writeSseEvent(...args);
/** 发送桌面通知（Electron 宿主捕获 stdout 前缀行转系统通知）。 */
const emitDesktopNotification = (...args) => notificationEmitterRuntime.emitDesktopNotification(...args);
/**
 * 全局 UI 事件广播器：把事件同时推给 SSE 客户端集合与 WS 客户端集合，
 * 由 event-stream 模块创建。
 */
const broadcastGlobalUiEvent = createGlobalUiEventBroadcaster({
  sseClients: uiNotificationClients,
  wsClients: uiNotificationWsClients,
  writeSseEvent,
});
/** 广播一条 UI 通知（SSE + WS + 桌面通知的组合出口）。 */
const broadcastUiNotification = (...args) => notificationEmitterRuntime.broadcastUiNotification(...args);

/**
 * 会话 runtime：维护活跃会话索引与数量，消费 OpenCode SSE payload
 * 更新会话状态；事件同时写入 SSE 客户端。
 */
const sessionRuntime = createSessionRuntime({
  writeSseEvent,
  getNotificationClients: () => uiNotificationClients,
  broadcastEvent: broadcastGlobalUiEvent,
});

/** 当前活跃（有事件流）的会话数量。 */
const getActiveSessionCount = () => sessionRuntime.getActiveSessionCount();

/**
 * 上游停滞超时：并发会话多于 1 个时用更长的 CONCURRENT 档，避免多会话
 * 抢占导致误杀慢流；单会话用默认档。
 * @returns {number} 停滞超时毫秒数
 */
const getUpstreamStallTimeoutMs = () => (
  getActiveSessionCount() > 1
    ? UPSTREAM_STALL_TIMEOUT_CONCURRENT_MS
    : DEFAULT_UPSTREAM_STALL_TIMEOUT_MS
);

/**
 * 项目配置 runtime：读写 projects/<id>.json 的项目级配置文件。
 */
const projectConfigRuntime = createProjectConfigRuntime({
  fsPromises,
  path,
  projectsDirPath: OMPCHAMBER_PROJECTS_CONFIG_DIR,
});

/**
 * 项目上下文 runtime：维护每个项目的上下文（CONTEXT.md 等知识文件）。
 */
const projectContextRuntime = createProjectContextRuntime({
  fsPromises,
  path,
  projectsDirPath: OMPCHAMBER_PROJECTS_CONFIG_DIR,
});

/**
 * agent memory runtime：按项目存取 agent 记忆（projects 目录 +
 * 用户配置根下的存储布局）。
 */
const agentMemoryRuntime = createAgentMemoryRuntime({
  fsPromises,
  path,
  projectsDirPath: OMPCHAMBER_PROJECTS_CONFIG_DIR,
  userConfigRoot: OMPCHAMBER_USER_CONFIG_ROOT,
});

// 中文补充：memory 的总开关——同时控制工具、路由与会话索引三处入口，
// 关闭后没有任何残留路径仍读写记忆存储（英文原注释见下）。
/**
 * One switch for everything memory-related. It gates the tool, these routes,
 * and the session index alike, so turning memory off leaves nothing behind
 * that still reads or writes the store.
 */
const isAgentMemoryEnabled = async () => {
  // The feature gate comes first: unreleased means absent, not merely switched
  // off, so no stored setting can bring it back.
  if (!isAgentMemoryFeatureAvailable()) {
    return false;
  }
  const settings = await readSettingsFromDiskMigrated().catch(() => null);
  return settings?.agentMemoryToolEnabled === true;
};

// 中文补充：以下状态放在 globalThis 上以在 dev-server HMR 重载后存活，
// 防止产生孤儿 OpenCode 进程（英文原注释见下）。
// HMR-persistent state via globalThis
// These values survive dev-server HMR reloads to prevent zombie OpenCode processes
const hmrStateRuntime = createHmrStateRuntime({
  globalThisLike: globalThis,
  os,
  processLike: process,
  stateKey: '__ompchamberHmrState',
});
// 取出（或首次创建）全局 HMR 状态对象，并确保用户提供的密码已初始化。
const hmrState = hmrStateRuntime.getOrCreateHmrState();
hmrStateRuntime.ensureUserProvidedOpenCodePassword(hmrState);

// 中文补充：以下均为「非 HMR」可变状态——模块重载后可以安全重置。
// Non-HMR state (safe to reset on reload)
/** 健康检查轮询定时器句柄。 */
let healthCheckInterval = null;
/** 主 HTTP server 实例（main 中创建）。 */
let server = null;
/** 主 express 应用实例（main 中创建）。 */
let expressApp = null;
/** 进行中的 OpenCode 重启 Promise（防并发重启）。 */
let currentRestartPromise = null;
/** 是否正处于 OpenCode 重启流程。 */
let isRestartingOpenCode = false;
/** 探测到的 OpenCode API prefix（当前保持空串，API 在根路径）。 */
let openCodeApiPrefix = '';
/** API prefix 是否已探测完成。 */
let openCodeApiPrefixDetected = true;
/** API prefix 探测定时器句柄。 */
let openCodeApiDetectionTimer = null;
/** 最近一次 OpenCode 启动/运行错误（健康快照展示）。 */
let lastOpenCodeError = null;
/** 最近一次启动诊断信息。 */
let lastOpenCodeLaunchDiagnostics = null;
/** 最近一次健康检查失败信息。 */
let lastOpenCodeHealthFailure = null;
/** 最近一次受管 OpenCode 进程信息。 */
let lastManagedOpenCodeProcess = null;
/** 最近一次重启诊断信息。 */
let lastOpenCodeRestartDiagnostics = null;
/** OpenCode 是否已就绪。 */
let isOpenCodeReady = false;
/** OpenCode 变为未就绪的时间戳（0 表示从未）。 */
let openCodeNotReadySince = 0;
/** 是否连接的是外部（非受管）OpenCode 实例。 */
let isExternalOpenCode = false;
/** 停机完成后是否退出进程（Electron 嵌入时可关）。 */
let exitOnShutdown = true;
/** UI 鉴权控制器实例（main 中创建）。 */
let uiAuthController = null;
/** 当前活跃的隧道控制器（启动隧道后赋值）。 */
let activeTunnelController = null;
/** 全局事件 watcher 启动 Promise（防重复启动）。 */
let globalWatcherStartPromise = null;
/**
 * 隧道提供方注册表：Cloudflare 与 ngrok 两个 provider，seal 后不可再增，
 * 供隧道配线按 provider 名分发。
 */
const tunnelProviderRegistry = createTunnelProviderRegistry([
  createCloudflareTunnelProvider(),
  createNgrokTunnelProvider(),
]);
// 封闭注册表：之后任何模块都不能再注册新的隧道 provider。
tunnelProviderRegistry.seal();
/** 隧道鉴权控制器：隧道请求的 token 校验与签发。 */
const tunnelAuthController = createTunnelAuth();
/** 运行期解析出的 managed-remote 隧道 token（空串表示未启用）。 */
let runtimeManagedRemoteTunnelToken = '';
/** 运行期解析出的 managed-remote 隧道主机名。 */
let runtimeManagedRemoteTunnelHostname = '';
/** 终端 runtime 实例（启动管线创建后赋值）。 */
let terminalRuntime = null;
/** 听写 runtime 实例（启动管线创建后赋值）。 */
let dictationRuntime = null;
/** 消息流 runtime 实例（启动管线创建后赋值，重启 OpenCode 后重绑上游）。 */
let messageStreamRuntime = null;
/** 用户在 CLI/设置中显式提供的 OpenCode 密码（HMR 存活）。 */
const userProvidedOpenCodePassword = hmrStateRuntime.getUserProvidedOpenCodePassword(hmrState);
/** 从 HMR 状态推导的初始 OpenCode 鉴权状态（密码 + 来源）。 */
const initialOpenCodeAuthState = hmrStateRuntime.resolveOpenCodeAuthFromState({
  hmrState,
  userProvidedOpenCodePassword,
});
/** 当前生效的 OpenCode 鉴权密码。 */
let openCodeAuthPassword = initialOpenCodeAuthState.openCodeAuthPassword;
/** 当前密码的来源（user/env/managed 等，健康快照展示）。 */
let openCodeAuthSource = initialOpenCodeAuthState.openCodeAuthSource;

// 中文补充：把模块级可变状态写回 HMR 状态——修改任何 HMR 变量后调用
//（英文原注释见下）。
// Sync helper - call after modifying any HMR state variable
const syncToHmrState = () => {
  hmrStateRuntime.syncStateFromRuntime(hmrState, {
    openCodeProcess,
    openCodePort,
    openCodeBaseUrl,
    isShuttingDown,
    signalsAttached,
    openCodeWorkingDirectory,
    openCodeAuthPassword,
    openCodeAuthSource,
  });
};

// 中文补充：从 HMR 状态恢复模块级变量（模块重载时调用，英文原注释见下）。
// Sync helper - call to restore state from HMR (e.g., on module reload)
const syncFromHmrState = () => {
  const restored = hmrStateRuntime.restoreRuntimeFromState({
    hmrState,
    userProvidedOpenCodePassword,
  });
  openCodeProcess = restored.openCodeProcess;
  openCodePort = restored.openCodePort;
  openCodeBaseUrl = restored.openCodeBaseUrl;
  isShuttingDown = restored.isShuttingDown;
  signalsAttached = restored.signalsAttached;
  openCodeWorkingDirectory = restored.openCodeWorkingDirectory;
  openCodeAuthPassword = restored.openCodeAuthPassword;
  openCodeAuthSource = restored.openCodeAuthSource;
};

// 中文补充：以下模块级变量是 HMR 状态的影子，通过上方两个助手在
// HMR 重载前后双向同步以存活（英文原注释见下）。
// Module-level variables that shadow HMR state
// These are synced to/from hmrState to survive HMR reloads
/** 受管 OpenCode 子进程句柄（HMR 影子变量）。 */
let openCodeProcess = hmrState.openCodeProcess;
/** 受管 OpenCode 监听端口。 */
let openCodePort = hmrState.openCodePort;
/** OpenCode 的 base URL（外部实例或探测结果；null 表示未知）。 */
let openCodeBaseUrl = hmrState.openCodeBaseUrl ?? null;
/** 是否正在停机（防重入）。 */
let isShuttingDown = hmrState.isShuttingDown;
/** 信号处理器是否已挂载（防重复注册）。 */
let signalsAttached = hmrState.signalsAttached;
/** 受管 OpenCode 的工作目录。 */
let openCodeWorkingDirectory = hmrState.openCodeWorkingDirectory;

/**
 * 从环境变量解析 OpenCode 的端口/主机/主机名配置：
 * configured* 为用户显式配置，effectivePort 为考虑默认值后的有效端口。
 */
const {
  configuredOpenCodePort: ENV_CONFIGURED_OPENCODE_PORT,
  configuredOpenCodeHost: ENV_CONFIGURED_OPENCODE_HOST,
  effectivePort: ENV_EFFECTIVE_PORT,
  configuredOpenCodeHostname: ENV_CONFIGURED_OPENCODE_HOSTNAME,
} = resolveOpenCodeEnvConfig({
  env: process.env,
  logger: console,
});

/** 是否跳过受管 OpenCode 启动（外部实例或纯静态模式）。 */
const ENV_SKIP_OPENCODE_START = process.env.OPENCODE_SKIP_START === 'true' ||
                                    process.env.OMPCHAMBER_SKIP_OPENCODE_START === 'true';
/**
 * 桌面通知开关：OMPCHAMBER_DESKTOP_NOTIFY=true、desktop 运行时、或
 * argv 中出现 ompchamber-server（打包的桌面二进制）任一命中即开启。
 */
const ENV_DESKTOP_NOTIFY = (() => {
  if (process.env.OMPCHAMBER_DESKTOP_NOTIFY === 'true') {
    return true;
  }

  if (process.env.OMPCHAMBER_RUNTIME === 'desktop') {
    return true;
  }

  const argv0 = typeof process.argv?.[0] === 'string' ? process.argv[0] : '';
  const argv1 = typeof process.argv?.[1] === 'string' ? process.argv[1] : '';
  return /ompchamber-server/i.test(argv0) || /ompchamber-server/i.test(argv1);
})();
/**
 * OpenCode 鉴权状态 runtime：生成/维护受管 OpenCode 的访问密码与
 * Authorization header，判定连接是否安全；密码与来源通过 getter/setter
 * 挂到上面的模块级变量并同步 HMR 状态。
 */
const openCodeAuthStateRuntime = createOpenCodeAuthStateRuntime({
  crypto,
  process,
  getAuthPassword: () => openCodeAuthPassword,
  setAuthPassword: (value) => {
    openCodeAuthPassword = value;
  },
  getAuthSource: () => openCodeAuthSource,
  setAuthSource: (value) => {
    openCodeAuthSource = value;
  },
  getUserProvidedPassword: () => userProvidedOpenCodePassword,
  syncToHmrState,
});

/** 构造携带受管鉴权的请求 header（Authorization bearer）。 */
const getOpenCodeAuthHeaders = (...args) => openCodeAuthStateRuntime.getOpenCodeAuthHeaders(...args);
/** 当前到 OpenCode 的连接是否安全（本机或已启用鉴权）。 */
const isOpenCodeConnectionSecure = (...args) => openCodeAuthStateRuntime.isOpenCodeConnectionSecure(...args);
/** 确保本地 OpenCode server 已设置访问密码（幂等，缺失则生成）。 */
const ensureLocalOpenCodeServerPassword = (...args) => openCodeAuthStateRuntime.ensureLocalOpenCodeServerPassword(...args);

/**
 * OpenCode 网络状态对象：以 getter/setter 暴露上面的可变网络状态
 * （端口、base URL、API prefix 及其探测定时器），供 network runtime 读写。
 */
const openCodeNetworkState = {};
Object.defineProperties(openCodeNetworkState, {
  openCodePort: { get: () => openCodePort, set: (value) => { openCodePort = value; } },
  openCodeBaseUrl: { get: () => openCodeBaseUrl, set: (value) => { openCodeBaseUrl = value; } },
  openCodeApiPrefix: { get: () => openCodeApiPrefix, set: (value) => { openCodeApiPrefix = value; } },
  openCodeApiPrefixDetected: { get: () => openCodeApiPrefixDetected, set: (value) => { openCodeApiPrefixDetected = value; } },
  openCodeApiDetectionTimer: { get: () => openCodeApiDetectionTimer, set: (value) => { openCodeApiDetectionTimer = value; } },
});

/**
 * OpenCode 网络 runtime：等待就绪、归一化/维护 API prefix、拼接上游
 * URL、调度 prefix 探测；状态通过 openCodeNetworkState 间接读写。
 */
const openCodeNetworkRuntime = createOpenCodeNetworkRuntime({
  state: openCodeNetworkState,
  getOpenCodeAuthHeaders,
  configuredOpenCodeHostname: ENV_CONFIGURED_OPENCODE_HOSTNAME,
});

/** 等待 OpenCode 网络就绪（端口已知且可访问）。 */
const waitForReady = (...args) => openCodeNetworkRuntime.waitForReady(...args);
/** 归一化 API prefix（去斜杠、小写等）。 */
const normalizeApiPrefix = (...args) => openCodeNetworkRuntime.normalizeApiPrefix(...args);
/** 写入探测到的 API prefix。 */
const setDetectedOpenCodeApiPrefix = (...args) => openCodeNetworkRuntime.setDetectedOpenCodeApiPrefix(...args);
/** 拼接 OpenCode 上游完整 URL（host + prefix + path）。 */
const buildOpenCodeUrl = (...args) => openCodeNetworkRuntime.buildOpenCodeUrl(...args);
/** 确保 API prefix 已知（必要时触发探测）。 */
const ensureOpenCodeApiPrefix = (...args) => openCodeNetworkRuntime.ensureOpenCodeApiPrefix(...args);
/** 调度一次延迟的 API prefix 探测。 */
const scheduleOpenCodeApiDetection = (...args) => openCodeNetworkRuntime.scheduleOpenCodeApiDetection(...args);

// 中文补充：插件注册的 provider 只存在于运行中的 OpenCode 进程内；
// small-model 调用方必须经这条连接解析 provider，否则只能走基于文件的
// 解析、插件模型不可达（英文原注释见下）。
// Plugin-registered providers exist only inside the running OpenCode process.
// Small-model callers resolve them through this connection; without it they
// stay on the file-based resolution and plugin models remain unreachable.
configureOpenCodeRuntimeProviders({ buildOpenCodeUrl, getOpenCodeAuthHeaders });

/** 环境变量配置的 API prefix（当前实现固定为根路径，见下方告警）。 */
const ENV_CONFIGURED_API_PREFIX = normalizeApiPrefix(
  process.env.OPENCODE_API_PREFIX || process.env.OMPCHAMBER_API_PREFIX || ''
);

  // 已配置 prefix 但当前 API 固定运行在根路径：忽略并打印告警。
  if (ENV_CONFIGURED_API_PREFIX && ENV_CONFIGURED_API_PREFIX !== '') {
  console.warn('Ignoring configured OpenCode API prefix; API runs at root.');
}

/** login shell 环境快照缓存（PATH 等，spawn 子进程时复用）。 */
let cachedLoginShellEnvSnapshot;
/** 已解析的 opencode 二进制路径。 */
let resolvedOpencodeBinary = null;
/** opencode 二进制路径的解析来源（which/内置/自定义）。 */
let resolvedOpencodeBinarySource = null;
/** 已解析的 node 二进制路径（包装脚本用）。 */
let resolvedNodeBinary = null;
/** 已解析的 bun 二进制路径（opencode 可能以 bun 运行）。 */
let resolvedBunBinary = null;
/** 已解析的 git 二进制路径（spawn git 时用）。 */
let resolvedGitBinary = null;

/**
 * OpenCode 环境状态对象：以 getter/setter 暴露 login shell 快照与
 * 各已解析二进制路径，供 env runtime 读写。
 */
const openCodeEnvState = {};
Object.defineProperties(openCodeEnvState, {
  cachedLoginShellEnvSnapshot: { get: () => cachedLoginShellEnvSnapshot, set: (value) => { cachedLoginShellEnvSnapshot = value; } },
  resolvedOpencodeBinary: { get: () => resolvedOpencodeBinary, set: (value) => { resolvedOpencodeBinary = value; } },
  resolvedOpencodeBinarySource: { get: () => resolvedOpencodeBinarySource, set: (value) => { resolvedOpencodeBinarySource = value; } },
  resolvedNodeBinary: { get: () => resolvedNodeBinary, set: (value) => { resolvedNodeBinary = value; } },
  resolvedBunBinary: { get: () => resolvedBunBinary, set: (value) => { resolvedBunBinary = value; } },
  resolvedGitBinary: { get: () => resolvedGitBinary, set: (value) => { resolvedGitBinary = value; } },
});

/**
 * OpenCode 环境 runtime：login shell 环境快照、CLI 路径解析（PATH 搜索、
 * 可执行判定）、git 二进制定位与受管启动命令规格推导。
 */
const openCodeEnvRuntime = createOpenCodeEnvRuntime({
  state: openCodeEnvState,
  normalizeDirectoryPath,
  readSettingsFromDiskMigrated,
});

/** 应用 login shell 环境快照（补齐 GUI 启动时缺失的 PATH 等）。 */
const applyLoginShellEnvSnapshot = (...args) => openCodeEnvRuntime.applyLoginShellEnvSnapshot(...args);
/** 读取 login shell 环境快照（惰性采集并缓存）。 */
const getLoginShellEnvSnapshot = (...args) => openCodeEnvRuntime.getLoginShellEnvSnapshot(...args);
/** 确保已具备 opencode CLI 所需环境。 */
const ensureOpencodeCliEnv = (...args) => openCodeEnvRuntime.ensureOpencodeCliEnv(...args);
/** 解析 opencode CLI 的可执行路径（含包装脚本展开）。 */
const resolveOpencodeCliPath = (...args) => openCodeEnvRuntime.resolveOpencodeCliPath(...args);
/** 判断路径是否为可执行文件。 */
const isExecutable = (...args) => openCodeEnvRuntime.isExecutable(...args);
/** 在给定 PATH（数组或分隔字符串）中搜索可执行文件。 */
const searchPathFor = (...args) => openCodeEnvRuntime.searchPathFor(...args);
/** 解析 spawn git 时应使用的二进制路径。 */
const resolveGitBinaryForSpawn = (...args) => openCodeEnvRuntime.resolveGitBinaryForSpawn(...args);
/** 推导受管 OpenCode 的启动规格（binary/args/wrapper）。 */
const resolveManagedOpenCodeLaunchSpec = (...args) => openCodeEnvRuntime.resolveManagedOpenCodeLaunchSpec(...args);
/** 清除已缓存的 opencode 二进制解析结果（配置变化后重解析）。 */
const clearResolvedOpenCodeBinary = (...args) => openCodeEnvRuntime.clearResolvedOpenCodeBinary(...args);
/**
 * OpenCode 解析 runtime：对外提供二进制解析快照，组合 CLI 路径解析、
 * 环境准备与启动规格推导，解析结果经 getter/setter 落到上面的变量。
 */
const openCodeResolutionRuntime = createOpenCodeResolutionRuntime({
  path,
  resolveOpencodeCliPath,
  ensureOpencodeCliEnv,
  resolveManagedOpenCodeLaunchSpec,
  getResolvedState: () => ({
    resolvedOpencodeBinary,
    resolvedOpencodeBinarySource,
    resolvedNodeBinary,
    resolvedBunBinary,
  }),
  setResolvedOpencodeBinarySource: (value) => {
    resolvedOpencodeBinarySource = value;
  },
});
/** 获取当前 OpenCode 二进制解析快照（路径与来源）。 */
const getOpenCodeResolutionSnapshot = (...args) =>
  openCodeResolutionRuntime.getOpenCodeResolutionSnapshot(...args);

// 模块加载即应用 login shell 环境快照，保证后续 spawn 能找到依赖。
applyLoginShellEnvSnapshot();

/**
 * 通知模板 runtime 的正式创建（此前只有转发声明）：依赖 settings 读写、
 * OpenCode URL 构造与鉴权 header、git 二进制定位。
 */
notificationTemplateRuntime = createNotificationTemplateRuntime({
  readSettingsFromDisk,
  persistSettings,
  buildOpenCodeUrl,
  getOpenCodeAuthHeaders,
  resolveGitBinaryForSpawn,
});

/**
 * 通知触发 runtime：监听会话事件，按模板与可见性抑制规则决定是否发
 * 桌面通知 / Web Push / APNs；组合模板解析、文案抽取与三类发送出口。
 */
const notificationTriggerRuntime = createNotificationTriggerRuntime({
  readSettingsFromDisk,
  prepareNotificationLastMessage,
  buildTemplateVariables,
  extractLastMessageText,
  fetchLastAssistantMessageText,
  resolveNotificationTemplate,
  shouldApplyResolvedTemplateMessage,
  emitDesktopNotification,
  broadcastUiNotification,
  sendPushToAllUiSessions,
  sendApnsToAllUiSessions,
  isAnyInteractiveClientVisible,
  buildOpenCodeUrl,
  getOpenCodeAuthHeaders,
});

/** 处理一条会话事件并按需触发推送。 */
const maybeSendPushForTrigger = (...args) => notificationTriggerRuntime.maybeSendPushForTrigger(...args);
/** 设置某会话的权限自动接受策略（agent 会话用）。 */
const setAutoAcceptSession = (sessionId, enabled) => permissionAutoAcceptRuntime.setSessionPolicy(sessionId, enabled);
// 通知触发 runtime 已就绪：把角标清空函数指向真实实现。
clearPendingPushBadge = () => notificationTriggerRuntime.clearPendingPushBadge();

/**
 * session-assist runtime：订阅会话事件流做辅助处理（上下文补全等），
 * OpenCode 调用按事件携带的 directory 路由到正确实例；small-model 服务
 * 惰性 import 以缩短启动时间。
 */
const sessionAssistRuntime = createSessionAssistRuntime({
  buildOpenCodeUrl,
  getOpenCodeAuthHeaders,
  getSmallModelService: async () => import('./lib/small-model/index.js'),
});

/**
 * session-goal runtime：跟踪会话目标（objective/预算/完成状态），
 * 目标收敛时经 emitGoalNotification 发出通知。
 */
const sessionGoalRuntime = createSessionGoalRuntime({
  buildOpenCodeUrl,
  getOpenCodeAuthHeaders,
  getSmallModelService: async () => import('./lib/small-model/index.js'),
  // 中文补充：目标收敛通知替代被抑制的逐轮就绪通知，因此遵守同一个
  // notifyOnCompletion 开关（英文原注释见下）。
  emitGoalNotification: async ({ sessionId, directory, status, goal }) => {
    // The goal settle notification replaces the per-turn ready notifications
    // (suppressed while the goal is active) — so it obeys the same toggle.
    const settings = await readSettingsFromDisk();
    if (settings.notifyOnCompletion === false) {
      return;
    }
    const title = status === 'complete'
      ? 'Goal complete'
      : (status === 'budgetLimited' ? 'Goal reached its token budget' : 'Goal blocked');
    const detail = goal?.statusReason && goal.statusReason !== 'verified by audit' && goal.statusReason !== 'reported by agent'
      ? goal.statusReason
      : (goal?.note || '');
    const objective = typeof goal?.objective === 'string' ? goal.objective.slice(0, 140) : '';
    const notificationPayload = {
      title,
      body: [objective, detail].filter(Boolean).join(' — ').slice(0, 240),
      tag: `goal-${sessionId}`,
      kind: 'goal',
      sessionId,
      directory,
    };
    const desktopNotificationDelivered = emitDesktopNotification(notificationPayload);
    broadcastUiNotification(notificationPayload, { desktopNotificationDelivered });
    void notificationTriggerRuntime.sendGoalSettlePush({
      sessionId,
      directory,
      status,
      title,
      body: notificationPayload.body,
    }).catch((error) => {
      console.warn('[session-goal] push fanout failed:', error?.message || error);
    });
  },
});
// 中文补充：会话应被告知的「项目知识」统一出口——UI 的 HTTP 请求、定时
// 任务与进程内派发的 agent 会话都来问它，保证答案一致（英文原注释见下）。
/**
 * Owns what a session must be told about the project's knowledge. Every sender
 * asks it — the UI over HTTP, scheduled tasks and agent-dispatched sessions in
 * process — so the answer cannot differ between them.
 */
/**
 * 会话知识 runtime：组装注入会话的项目知识（项目上下文 + agent memory），
 * 内含直连 OpenCode 的 fetch 封装（15 秒超时、目录参数、鉴权 header）。
 */
const sessionKnowledgeRuntime = createSessionKnowledgeRuntime({
  projectContextRuntime,
  agentMemoryRuntime,
  // Called, not captured: the resolver is declared further down, and taking a
  // reference here would read it before it exists.
  resolveProjectId: (directory) => resolveMemoryProjectId(directory),
  isAgentMemoryEnabled,
  openCodeFetch: async (fetchPath, { directory, method = 'GET', body } = {}) => {
    const params = new URLSearchParams();
    if (directory) params.set('directory', directory);
    const search = params.toString();
    const response = await fetch(`${buildOpenCodeUrl(fetchPath, '')}${search ? `?${search}` : ''}`, {
      method,
      headers: {
        Accept: 'application/json',
        ...(body ? { 'Content-Type': 'application/json' } : {}),
        ...getOpenCodeAuthHeaders(),
      },
      ...(body ? { body: JSON.stringify(body) } : {}),
      signal: AbortSignal.timeout(15_000),
    });
    if (!response.ok) throw new Error(`OpenCode ${method} ${fetchPath} failed with ${response.status}`);
    return response.json().catch(() => null);
  },
});

/**
 * context-obligatory runtime：消费会话事件，维护「必读上下文」相关的
 * 会话状态（依赖会话知识 runtime 的结论）。
 */
const contextObligatoryRuntime = createContextObligatoryRuntime({
  buildOpenCodeUrl,
  getOpenCodeAuthHeaders,
  sessionKnowledgeRuntime,
});

/** Linear 集成的会话状态 runtime（issue 与会话的关联跟踪）。 */
const linearSessionStatusRuntime = createLinearSessionStatusRuntime();

/**
 * 全局消息流 hub：统一订阅 OpenCode 事件流并分发给所有下游（UI WS、
 * session runtime、辅助 runtime 等），带上游停滞超时判定。
 */
const globalMessageStreamHub = createGlobalMessageStreamHub({
  buildOpenCodeUrl,
  getOpenCodeAuthHeaders,
  upstreamStallTimeoutMs: getUpstreamStallTimeoutMs,
});

/**
 * 权限自动接受 runtime：按会话/目录策略自动应答 OpenCode 的权限请求；
 * 订阅全局事件 hub，策略变化广播给 UI。
 */
const permissionAutoAcceptRuntime = createPermissionAutoAcceptRuntime({
  globalEventHub: globalMessageStreamHub,
  buildOpenCodeUrl,
  getOpenCodeAuthHeaders,
  readSettingsFromDiskMigrated,
  persistSettings,
  broadcastGlobalUiEvent,
});
// 启动权限自动接受监听，并让通知触发 runtime 感知自动接受状态（抑制无谓推送）。
permissionAutoAcceptRuntime.start();
notificationTriggerRuntime.setGetIsSessionAutoAccepting(
  (sessionId, directory) => permissionAutoAcceptRuntime.isSessionAutoAccepting(sessionId, directory),
);

/**
 * OpenCode watcher runtime：维护到 OpenCode 事件流的 SSE 订阅，
 * payload 到达时更新会话信息缓存、触发推送并驱动会话 runtime。
 */
const openCodeWatcherRuntime = createOpenCodeWatcherRuntime({
  waitForOpenCodePort: (...args) => waitForOpenCodePort(...args),
  buildOpenCodeUrl,
  getOpenCodeAuthHeaders,
  parseSseDataPayload: (...args) => parseSseDataPayload(...args),
  globalEventHub: globalMessageStreamHub,
  onPayload: (payload) => {
    maybeCacheSessionInfoFromEvent(payload);
    void maybeSendPushForTrigger(payload);
    sessionRuntime.processOpenCodeSsePayload(payload);
  },
});

// 中文补充：session-assist 直接订阅全局 hub——它需要信封上的 directory
// 才能把自己的 OpenCode 调用路由到正确实例（英文原注释见下）。
// Session-assist subscribes to the hub directly: it needs the envelope's
// directory to route its own OpenCode calls to the right instance.
console.log('[session-assist] listening for session events');
globalMessageStreamHub.subscribeEvent((event) => {
  const raw = event?.payload;
  const payload = raw?.payload && typeof raw.payload === 'object' ? raw.payload : raw;
  if (!payload || typeof payload !== 'object') return;
  const directory = typeof event?.directory === 'string' && event.directory && event.directory !== 'global'
    ? event.directory
    : '';
  sessionAssistRuntime.processPayload(payload, directory);
  sessionGoalRuntime.processPayload(payload, directory);
  contextObligatoryRuntime.processPayload(payload, directory);
  linearSessionStatusRuntime.processPayload(payload);
});

/**
 * 处理经转发到达的 OpenCode 事件 payload：缓存会话信息；对
 * session.status 事件额外合成两条 OMPChamber 事件——
 * ompchamber:session-status（含重试 attempt/message/next 元数据）与
 * ompchamber:session-activity（busy/idle 相位）——供未直连事件流的客户端
 * 感知会话状态。
 * @param {object} payload OpenCode 事件 payload
 * @param {(event: object) => void} emitSyntheticEvent 合成事件的发射函数
 */
const processForwardedEventPayload = (payload, emitSyntheticEvent) => {
  if (!payload || typeof payload !== 'object' || typeof emitSyntheticEvent !== 'function') {
    return;
  }

  // 先把会话信息（标题等）写进通知缓存。
  maybeCacheSessionInfoFromEvent(payload);

  // 仅 session.status 事件需要合成派生事件。
  if (payload.type !== 'session.status') {
    return;
  }

  const properties = payload.properties && typeof payload.properties === 'object' ? payload.properties : {};
  const statusInfo = properties.status && typeof properties.status === 'object' ? properties.status : {};
  const info = properties.info && typeof properties.info === 'object' ? properties.info : {};
  const sessionId = typeof properties.sessionID === 'string' ? properties.sessionID.trim() : '';
  const status = typeof statusInfo.type === 'string'
    ? statusInfo.type.trim()
    : (typeof info.type === 'string' ? info.type.trim() : '');

  // 缺少 sessionID 或 status 的事件无法定位会话，直接放弃。
  if (!sessionId || !status) {
    return;
  }

  // 合成状态事件：statusInfo 与 info 两种上游形态都兼容。
  emitSyntheticEvent({
    type: 'ompchamber:session-status',
    properties: {
      sessionID: sessionId,
      status,
      timestamp: Date.now(),
      metadata: {
        attempt: typeof statusInfo.attempt === 'number'
          ? statusInfo.attempt
          : (typeof info.attempt === 'number' ? info.attempt : undefined),
        message: typeof statusInfo.message === 'string'
          ? statusInfo.message
          : (typeof info.message === 'string' ? info.message : undefined),
        next: typeof statusInfo.next === 'number'
          ? statusInfo.next
          : (typeof info.next === 'number' ? info.next : undefined),
      },
      needsAttention: false,
    },
  });

  // 合成活动相位事件：busy/retry 视为忙碌，其余视为空闲。
  emitSyntheticEvent({
    type: 'ompchamber:session-activity',
    properties: {
      sessionId,
      phase: status === 'busy' || status === 'retry' ? 'busy' : 'idle',
    },
  });
};


/**
 * server 工具 runtime：OpenCode 端口设置与等待、augment/managed 路径
 * 构造、SSE payload 解析、agents/providers/models 快照拉取与反向代理
 * （setupProxy）等核心工具能力。
 */
const serverUtilsRuntime = createServerUtilsRuntime({
  fs,
  os,
  path,
  process,
  openCodeReadyGraceMs: OPEN_CODE_READY_GRACE_MS,
  longRequestTimeoutMs: LONG_REQUEST_TIMEOUT_MS,
  getRuntime: () => ({
    openCodePort,
    openCodeBaseUrl,
    openCodeNotReadySince,
    isOpenCodeReady,
    isRestartingOpenCode,
  }),
  getOpenCodeAuthHeaders,
  buildOpenCodeUrl,
  ensureOpenCodeApiPrefix,
  getUpstreamStallTimeoutMs,
  getUiNotificationClients: () => uiNotificationClients,
  getOpenCodePort: () => openCodePort,
  setOpenCodePortState: (value) => {
    openCodePort = value;
  },
  syncToHmrState,
  markOpenCodeNotReady: () => {
    isOpenCodeReady = false;
  },
  setOpenCodeNotReadySince: (value) => {
    openCodeNotReadySince = value;
  },
  clearLastOpenCodeError: () => {
    lastOpenCodeError = null;
  },
  getLoginShellPath: () => {
    const snapshot = getLoginShellEnvSnapshot();
    if (!snapshot || typeof snapshot.PATH !== 'string' || snapshot.PATH.length === 0) {
      return null;
    }
    return snapshot.PATH;
  },
});

/** 设置 OpenCode 端口（同步 HMR 状态）。 */
const setOpenCodePort = (...args) => serverUtilsRuntime.setOpenCodePort(...args);
/** 等待 OpenCode 端口就绪（启动期间轮询）。 */
const waitForOpenCodePort = (...args) => serverUtilsRuntime.waitForOpenCodePort(...args);
/** 构造 augmented（叠加内置资源）路径。 */
const buildAugmentedPath = (...args) => serverUtilsRuntime.buildAugmentedPath(...args);
/** 构造 managed OpenCode 资源路径。 */
const buildManagedOpenCodePath = (...args) => serverUtilsRuntime.buildManagedOpenCodePath(...args);
/** 解析 SSE data: 行的 JSON payload（容错）。 */
const parseSseDataPayload = (...args) => serverUtilsRuntime.parseSseDataPayload(...args);
/**
 * 静态资源路由 runtime：静态目录服务、PWA manifest（名称/方向）、
 * 项目目录解析与 OpenCode 联动。
 */
const staticRoutesRuntime = createStaticRoutesRuntime({
  fs,
  path,
  process,
  __dirname,
  express,
  resolveProjectDirectory,
  buildOpenCodeUrl,
  getOpenCodeAuthHeaders,
  readSettingsFromDiskMigrated,
  normalizePwaAppName,
  normalizePwaOrientation,
});
/**
 * 远程客户端鉴权 runtime：已配对设备（remote-clients.json）的注册、
 * 校验与吊销。
 */
const remoteClientAuthRuntime = createRemoteClientAuthRuntime({
  fsPromises,
  path,
  crypto,
  storePath: REMOTE_CLIENTS_FILE_PATH,
});
/**
 * 客户端配对 runtime：配对会话（client-pairing-sessions.json）的创建、
 * 确认与过期，落成后写入远程客户端存储。
 */
const clientPairingRuntime = createClientPairingRuntime({
  fsPromises,
  path,
  crypto,
  storePath: CLIENT_PAIRING_SESSIONS_FILE_PATH,
  remoteClientAuthRuntime,
});
/**
 * feature 路由 runtime：注册设置/主题/项目/agent memory/定时任务等大块
 * feature API 路由；clientReloadDelayMs 为构建变化后的客户端刷新延迟。
 */
const featureRoutesRuntime = createFeatureRoutesRuntime({
  clientReloadDelayMs: CLIENT_RELOAD_DELAY_MS,
});
/**
 * bootstrap runtime：装配基础路由——公共请求中间件、服务器状态/健康、
 * 鉴权与访问控制、TTS、通知、OMPChamber 主路由与 agent-tool 路由。
 */
const bootstrapRuntime = createBootstrapRuntime({
  createUiAuth,
  registerServerStatusRoutes,
  registerCommonRequestMiddleware,
  registerAuthAndAccessRoutes,
  registerTtsRoutes,
  registerNotificationRoutes,
  registerOMPChamberRoutes,
  registerAgentToolRoutes: (app, options) => options.agentToolRuntime.registerRoutes(app, options.express),
  express,
});
/**
 * 隧道配线 runtime：初始化隧道体系（注册管理路由、维护活跃隧道控制器
 * 与 managed-remote token/hostname 的运行期状态）。
 */
const tunnelWiringRuntime = createTunnelWiringRuntime({
  crypto,
  URL,
  tunnelProviderRegistry,
  tunnelAuthController,
  readSettingsFromDiskMigrated,
  readManagedRemoteTunnelConfigFromDisk,
  normalizeTunnelProvider,
  normalizeTunnelMode,
  normalizeOptionalPath,
  normalizeManagedRemoteTunnelHostname,
  normalizeTunnelBootstrapTtlMs,
  normalizeTunnelSessionTtlMs,
  isSupportedTunnelMode,
  upsertManagedRemoteTunnelToken,
  resolveManagedRemoteTunnelToken,
  TUNNEL_MODE_QUICK,
  TUNNEL_MODE_MANAGED_LOCAL,
  TUNNEL_MODE_MANAGED_REMOTE,
  TUNNEL_PROVIDER_CLOUDFLARE,
  TunnelServiceError,
  getActiveTunnelController: () => activeTunnelController,
  setActiveTunnelController: (value) => {
    activeTunnelController = value;
  },
  getRuntimeManagedRemoteTunnelHostname: () => runtimeManagedRemoteTunnelHostname,
  setRuntimeManagedRemoteTunnelHostname: (value) => {
    runtimeManagedRemoteTunnelHostname = value;
  },
  getRuntimeManagedRemoteTunnelToken: () => runtimeManagedRemoteTunnelToken,
  setRuntimeManagedRemoteTunnelToken: (value) => {
    runtimeManagedRemoteTunnelToken = value;
  },
});
/**
 * 启动管线 runtime：按序创建终端/听写/消息流 WS 等运行时并执行 server
 * 启动流程（静态服务、受管 OpenCode 启动、信号挂载等）。
 */
const startupPipelineRuntime = createStartupPipelineRuntime({
  createTerminalRuntime,
  createDictationRuntime,
  createMessageStreamWsRuntime,
  createServerStartupRuntime,
});

/**
 * OpenCode 生命周期状态对象：以 getter/setter 暴露进程/端口/重启/健康等
 * 全部可变状态，供生命周期 runtime 统一读写。
 */
const openCodeLifecycleState = {};
Object.defineProperties(openCodeLifecycleState, {
  openCodeProcess: { get: () => openCodeProcess, set: (value) => { openCodeProcess = value; } },
  openCodePort: { get: () => openCodePort, set: (value) => { openCodePort = value; } },
  openCodeBaseUrl: { get: () => openCodeBaseUrl, set: (value) => { openCodeBaseUrl = value; } },
  openCodeWorkingDirectory: { get: () => openCodeWorkingDirectory, set: (value) => { openCodeWorkingDirectory = value; } },
  currentRestartPromise: { get: () => currentRestartPromise, set: (value) => { currentRestartPromise = value; } },
  isRestartingOpenCode: { get: () => isRestartingOpenCode, set: (value) => { isRestartingOpenCode = value; } },
  openCodeApiPrefix: { get: () => openCodeApiPrefix, set: (value) => { openCodeApiPrefix = value; } },
  openCodeApiPrefixDetected: { get: () => openCodeApiPrefixDetected, set: (value) => { openCodeApiPrefixDetected = value; } },
  openCodeApiDetectionTimer: { get: () => openCodeApiDetectionTimer, set: (value) => { openCodeApiDetectionTimer = value; } },
  lastOpenCodeError: { get: () => lastOpenCodeError, set: (value) => { lastOpenCodeError = value; } },
  lastOpenCodeLaunchDiagnostics: { get: () => lastOpenCodeLaunchDiagnostics, set: (value) => { lastOpenCodeLaunchDiagnostics = value; } },
  lastOpenCodeHealthFailure: { get: () => lastOpenCodeHealthFailure, set: (value) => { lastOpenCodeHealthFailure = value; } },
  isExternalOpenCode: { get: () => isExternalOpenCode, set: (value) => { isExternalOpenCode = value; } },
  isShuttingDown: { get: () => isShuttingDown, set: (value) => { isShuttingDown = value; } },
  healthCheckInterval: { get: () => healthCheckInterval, set: (value) => { healthCheckInterval = value; } },
  expressApp: { get: () => expressApp, set: (value) => { expressApp = value; } },
});

/**
 * OpenCode 生命周期 runtime：受管 OpenCode 的启动、重启、健康监控、
 * 端口占用清理与配置变更后的刷新；含启动预热目录与重启后的善后回调。
 */
const openCodeLifecycleRuntime = createOpenCodeLifecycleRuntime({
  state: openCodeLifecycleState,
  env: {
    ENV_CONFIGURED_OPENCODE_PORT,
    ENV_CONFIGURED_OPENCODE_HOST,
    ENV_EFFECTIVE_PORT,
    ENV_CONFIGURED_OPENCODE_HOSTNAME,
    ENV_SKIP_OPENCODE_START,
  },
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
  setupProxy: (...args) => setupProxy(...args),
  ensureOpenCodeApiPrefix,
  clearResolvedOpenCodeBinary,
  buildAugmentedPath,
  buildManagedOpenCodePath,
  getManagedOpenCodeShellEnvSnapshot: getLoginShellEnvSnapshot,
  getActiveSessionCount,
  // 中文补充：预热目录按最近使用优先——OpenCode 对每个目录惰性初始化
  // （大会话库要数秒），就绪后立刻预热可避免 UI 首个交互请求付这笔
  // 延迟（英文原注释见下）。
  // Most-recently-used directories first: OpenCode initializes each directory
  // lazily on first request (seconds on large session stores), so the
  // lifecycle warms these right after readiness — before the UI's first
  // interactive request would otherwise pay that cost.
  getWarmupDirectories: async () => {
    const settings = await readSettingsFromDiskMigrated().catch(() => null);
    if (!settings) return [];
    const directories = [];
    if (typeof settings.lastDirectory === 'string' && settings.lastDirectory) {
      directories.push(settings.lastDirectory);
    }
    const projects = Array.isArray(settings.projects) ? [...settings.projects] : [];
    projects.sort((a, b) => (b?.lastOpenedAt ?? 0) - (a?.lastOpenedAt ?? 0));
    for (const project of projects) {
      if (typeof project?.path === 'string' && project.path) {
        directories.push(project.path);
      }
    }
    return [...new Set(directories)];
  },
  // 中文补充：受管重启可能把 OpenCode 挪到新端口（旧端口未必及时释放），
  // 需把消息流上游 reader 重绑到当前端口，否则 UI 停留在旧进程收不到
  // 事件（#2638，英文原注释见下）。
  // A managed restart can move OpenCode to a NEW port (the old one may stay
  // occupied if killProcessOnPort/waitForPortRelease didn't free it in time,
  // on any platform). Rebind the message-stream upstream readers to the current port
  // so the UI keeps receiving events instead of staying pinned to the old
  // process (#2638). The runtime is created later by the startup pipeline;
  // by the time any restart runs, it is assigned.
  // 中文补充：重启会重载插件——provider 端口、凭证乃至 provider 列表
  // 都可能与缓存不同，必须先清缓存（英文原注释见下）。
  onOpenCodeRestarted: () => {
    // A restart reloads plugins: provider ports, credentials and the provider
    // list itself can all differ from what was cached.
    resetOpenCodeRuntimeProviders();
    try {
      messageStreamRuntime?.rebindUpstream();
    } catch (error) {
      console.warn('Failed to rebind message stream after OpenCode restart:', error?.message ?? error);
    }
    try {
      const { sessionIds } = sessionRuntime.interruptBusySessionsAfterRestart();
      if (sessionIds.length > 0) {
        const multiple = sessionIds.length > 1;
        broadcastUiNotification({
          title: multiple ? 'Chats interrupted' : 'Chat interrupted',
          body: multiple
            ? 'OpenCode restarted during running responses. Send a message in each chat to continue.'
            : 'OpenCode restarted during a running response. Send a message to continue.',
          tag: 'opencode-restart-interrupted',
          kind: 'opencode-restart-interrupted',
          sessionId: sessionIds[0],
        });
      }
    } catch (error) {
      console.warn('Failed to reconcile sessions after OpenCode restart:', error?.message ?? error);
    }
  },
  // 中文补充：omp 宿主不是 OpenCode 进程，不读取 OPENCODE_CONFIG_CONTENT，
  // 因此不再在 spawn 时注入 agent-tool 插件；该能力在 omp 引擎下视为
  // 不可用，系统提示优化器已整体移除（英文原注释见下）。
  getManagedOpenCodeEnv: async () => {
    // The omp host is not an OpenCode process: it never reads
    // OPENCODE_CONFIG_CONTENT, so the agent-tool plugin is no longer
    // injected at spawn. That capability is documented as unavailable with
    // the omp engine; the system-prompt optimizer was removed outright.
    return {};
  },
});


/** 重启受管 OpenCode（生命周期 runtime，含善后与重绑）。 */
const restartOpenCode = (...args) => openCodeLifecycleRuntime.restartOpenCode(...args);
/** 等待 OpenCode 就绪（启动或重启后）。 */
const waitForOpenCodeReady = (...args) => openCodeLifecycleRuntime.waitForOpenCodeReady(...args);
/** 等待 agent（模型提供方）就绪可用。 */
const waitForAgentPresence = (...args) => openCodeLifecycleRuntime.waitForAgentPresence(...args);
/** OpenCode 配置变化后的刷新（避免整进程重启）。 */
const refreshOpenCodeAfterConfigChange = (...args) => openCodeLifecycleRuntime.refreshOpenCodeAfterConfigChange(...args);
/** 启动周期健康监控（间隔 HEALTH_CHECK_INTERVAL）。 */
const startHealthMonitoring = () => openCodeLifecycleRuntime.startHealthMonitoring(HEALTH_CHECK_INTERVAL);
/** 立即触发一次健康检查。 */
const triggerHealthCheck = () => openCodeLifecycleRuntime.triggerHealthCheck();
/**
 * 定时任务 runtime：按项目配置调度任务的创建/触发，任务执行派发
 * OpenCode 会话（自动接受策略、知识注入），执行结果广播给 UI。
 */
const scheduledTasksRuntime = createScheduledTasksRuntime({
  projectConfigRuntime,
  // 项目列表取自最新 settings 并复用同一套清洗逻辑。
  listProjects: async () => {
    const settings = await readSettingsFromDiskMigrated();
    return sanitizeProjects(settings?.projects || []);
  },
  buildOpenCodeUrl,
  getOpenCodeAuthHeaders,
  waitForOpenCodeReady,
  sessionKnowledgeRuntime,
  setSessionAutoAccept: (sessionId, enabled, directory) => permissionAutoAcceptRuntime.setSessionPolicy(sessionId, enabled, directory),
  // 任务执行事件经 OMPChamber 事件 SSE 广播给所有已连接客户端。
  emitTaskRunEvent: (event) => {
    for (const client of uiOMPChamberEventClients) {
      try {
        writeSseEvent(client, {
          type: 'ompchamber:scheduled-task-ran',
          properties: {
            projectId: event.projectID,
            taskId: event.taskID,
            ranAt: event.ranAt,
            status: event.status,
            ...(event.sessionID ? { sessionId: event.sessionID } : {}),
          },
        });
      } catch {
        uiOMPChamberEventClients.delete(client);
      }
    }
  },
  logger: console,
});
/**
 * 向所有 OMPChamber 事件客户端广播 ompchamber:session-created 事件
 * （会话创建时间、目录、是否已派发 prompt 等）；写失败的客户端就地移除。
 * @param {{ sessionID: string, directory: string, createdAt: number, promptDispatched?: boolean, dispatchedAsCommand?: boolean, projectID?: string, title?: string }} event 会话创建事件
 */
const emitSessionCreatedEvent = (event) => {
  for (const client of uiOMPChamberEventClients) {
    try {
      writeSseEvent(client, {
        type: 'ompchamber:session-created',
        properties: {
          sessionId: event.sessionID,
          directory: event.directory,
          createdAt: event.createdAt,
          promptDispatched: event.promptDispatched === true,
          dispatchedAsCommand: event.dispatchedAsCommand === true,
          ...(event.projectID ? { projectId: event.projectID } : {}),
          ...(event.title ? { title: event.title } : {}),
        },
      });
    } catch {
      uiOMPChamberEventClients.delete(client);
    }
  }
};

// 中文补充：注册项目的 linked-worktree 拓扑变化——通过 .git/worktrees
// 元数据观测（无论哪个客户端增删 worktree），广播给 OMPChamber 客户端
// 让其权威地重新列表（英文原注释见下）。
// Linked-worktree topology changes for registered projects, observed via
// `.git/worktrees` metadata regardless of which client created or removed
// the worktree. Emitted to OMPChamber clients so they re-list authoritatively.
const emitWorktreesChangedEvent = (directories) => {
  if (!Array.isArray(directories) || directories.length === 0) return;
  for (const client of uiOMPChamberEventClients) {
    try {
      writeSseEvent(client, {
        type: 'ompchamber:worktrees-changed',
        properties: {
          directories,
        },
      });
    } catch {
      uiOMPChamberEventClients.delete(client);
    }
  }
};

// 中文补充：把会话目录映射到其记忆所属的项目，让运行在 worktree 里的
// 会话写到面板所展示的那个项目（英文原注释见下）。
/**
 * Maps a session directory onto the project whose memory it belongs to, so a
 * session running in a worktree writes to the project the panel shows.
 */
const resolveMemoryProjectId = createMemoryProjectResolver({
  listProjectPaths: async () => {
    const settings = await readSettingsFromDiskMigrated().catch(() => null);
    return sanitizeProjects(settings?.projects || []).map((project) => project.path);
  },
  resolvePrimaryWorktreeRoot,
  managedProjectRoots: [path.join(OMPCHAMBER_USER_CONFIG_ROOT, 'chats')],
});

// 中文补充：通知已打开的面板「agent 改变了它记住的内容」，让刚写入的
// 记忆无需重开即可见（英文原注释见下）。
/**
 * Tells open panels that the agent changed what it remembers, so what it just
 * stored is visible without reopening anything.
 */
const emitAgentMemoryChangedEvent = (event) => {
  for (const client of uiOMPChamberEventClients) {
    try {
      writeSseEvent(client, {
        type: 'ompchamber:agent-memory-changed',
        properties: {
          scope: event.scope,
          ...(event.projectId ? { projectId: event.projectId } : {}),
        },
      });
    } catch {
      uiOMPChamberEventClients.delete(client);
    }
  }
};
/**
 * worktree watcher：监听注册项目（来自 settings）与 settings 文件本身
 * 的 worktree 变化，变化时回调 emitWorktreesChangedEvent。
 */
const worktreeWatcherRuntime = createWorktreeWatcher({
  listProjects: async () => {
    const settings = await readSettingsFromDiskMigrated().catch(() => null);
    return sanitizeProjects(settings?.projects || []);
  },
  settingsFilePath: SETTINGS_FILE_PATH,
  onWorktreesChanged: emitWorktreesChangedEvent,
  logger: console,
});
/**
 * 定时任务服务：面向路由的定时任务操作入口（创建/更新/删除/查询），
 * 组合 settings 读写、项目配置与 runtime 调度。
 */
const scheduledTaskService = createScheduledTaskService({
  readSettingsFromDiskMigrated,
  sanitizeProjects,
  projectConfigRuntime,
  scheduledTasksRuntime,
});
/**
 * OMPChamber 会话服务：创建/派发 OMPChamber 会话（校验目录、等待
 * OpenCode 就绪、注入会话知识），并广播 session-created 事件。
 */
const openChamberSessionService = createOMPChamberSessionService({
  readSettingsFromDiskMigrated,
  sanitizeProjects,
  validateDirectoryPath,
  buildOpenCodeUrl,
  getOpenCodeAuthHeaders,
  waitForOpenCodeReady,
  emitSessionCreatedEvent,
  sessionKnowledgeRuntime,
});
// 中文补充：浏览器操作广播给所有已连接的 OMPChamber 客户端，由持有
// 浏览器面板的那个应答；emitRequest 返回送达的客户端数，让 broker 能在
// 无人监听时快速失败（英文原注释见下）。
// Browser actions are published to whichever OMPChamber clients are connected;
// the one owning the browser panel answers. `emitRequest` returns the number of
// clients reached so the broker can fail fast when nobody is listening.
const browserControlBroker = createBrowserControlBroker({
  createId: () => `browser-${crypto.randomUUID()}`,
  emitRequest: (request) => {
    // Opening a page only needs a panel to open it in; everything else needs a
    // client that can actually drive one. Counting the right clients is what
    // lets the broker say "not here" instead of timing out.
    const needsBrowserView = request.action !== 'browser.open';
    let delivered = 0;
    for (const client of uiOMPChamberEventClients) {
      if (needsBrowserView && client.ompchamberBrowserCapable !== true) continue;
      try {
        writeSseEvent(client, {
          type: 'ompchamber:browser-control-request',
          properties: {
            requestId: request.requestId,
            action: request.action,
            parameters: request.parameters,
          },
        });
        delivered += 1;
      } catch {
        uiOMPChamberEventClients.delete(client);
      }
    }
    return delivered;
  },
});

/**
 * OMPChamber 控制服务：面向 agent-tool 的统一命令执行入口，聚合会话、
 * 定时任务、浏览器控制与 agent memory 动作（含错误映射与事件广播）。
 */
const openChamberControlService = createOMPChamberControlService({
  readSettingsFromDiskMigrated,
  sanitizeProjects,
  buildOpenCodeUrl,
  getOpenCodeAuthHeaders,
  waitForOpenCodeReady,
  sessionService: openChamberSessionService,
  scheduledTaskService,
  browserControl: browserControlBroker,
  agentMemoryActions: createAgentMemoryActions({
    agentMemoryRuntime,
    createError: (message, status) => new OMPChamberControlError(message, status),
    onMemoryChanged: emitAgentMemoryChangedEvent,
    isAgentMemoryEnabled,
    resolveProjectId: resolveMemoryProjectId,
  }),
});

/**
 * 确保 OpenCode 全局事件 watcher 已启动（幂等：并发调用复用同一个
 * Promise；启动失败会清除缓存以便重试）。
 * @returns {Promise<void>} watcher 启动完成
 */
const ensureGlobalWatcherStarted = async () => {
  if (globalWatcherStartPromise) {
    return globalWatcherStartPromise;
  }

  globalWatcherStartPromise = openCodeWatcherRuntime.start().catch((error) => {
    globalWatcherStartPromise = null;
    throw error;
  });

  return globalWatcherStartPromise;
};
/**
 * 启动期引导受管 OpenCode：执行生命周期 bootstrap、调度 API prefix
 * 探测、按需启动健康监控，并无条件启动全局 watcher（session-assist
 * 也依赖它的事件 hub）。
 */
const bootstrapOpenCodeAtStartup = async (...args) => {
  await openCodeLifecycleRuntime.bootstrapOpenCodeAtStartup(...args);
  scheduleOpenCodeApiDetection();
  if (openCodeLifecycleState.openCodeProcess && !openCodeLifecycleState.isExternalOpenCode) {
    startHealthMonitoring();
  }
  // The global watcher used to start only for desktop notifications; the
  // session-assist runtime also rides its event hub, so it now starts
  // unconditionally once OpenCode is up.
  void ensureGlobalWatcherStarted().catch((error) => {
    console.warn(`Global event watcher startup failed: ${error?.message || error}`);
  });
};
/** 清理占用指定端口的进程（重启前腾出端口）。 */
const killProcessOnPort = (...args) => openCodeLifecycleRuntime.killProcessOnPort(...args);
/** 等待端口真正释放（轮询直到可绑定）。 */
const waitForPortRelease = (...args) => openCodeLifecycleRuntime.waitForPortRelease(...args);

/** 拉取 OpenCode agents 快照（agent 列表）。 */
const fetchAgentsSnapshot = (...args) => serverUtilsRuntime.fetchAgentsSnapshot(...args);
/** 拉取 OpenCode providers 快照（提供方列表）。 */
const fetchProvidersSnapshot = (...args) => serverUtilsRuntime.fetchProvidersSnapshot(...args);
/** 拉取 OpenCode models 快照（模型列表）。 */
const fetchModelsSnapshot = (...args) => serverUtilsRuntime.fetchModelsSnapshot(...args);
/** 在 express app 上挂载 OpenCode 反向代理（含 SSE 与超时策略）。 */
const setupProxy = (...args) => serverUtilsRuntime.setupProxy(...args);
/**
 * 优雅停机 runtime：按序停止各 runtime、关闭 server 与受管 OpenCode、
 * 清理隧道/定时任务，超时（SHUTDOWN_TIMEOUT）强制退出。
 */
const gracefulShutdownRuntime = createGracefulShutdownRuntime({
  process,
  shutdownTimeoutMs: SHUTDOWN_TIMEOUT,
  getExitOnShutdown: () => exitOnShutdown,
  getIsShuttingDown: () => isShuttingDown,
  setIsShuttingDown: (value) => {
    isShuttingDown = value;
  },
  syncToHmrState,
  openCodeWatcherRuntime,
  sessionAssistRuntime,
  sessionGoalRuntime,
  contextObligatoryRuntime,
  sessionRuntime,
  getHealthCheckInterval: () => healthCheckInterval,
  clearHealthCheckInterval: (value) => clearInterval(value),
  getTerminalRuntime: () => terminalRuntime,
  setTerminalRuntime: (value) => {
    terminalRuntime = value;
  },
  getMessageStreamRuntime: () => messageStreamRuntime,
  setMessageStreamRuntime: (value) => {
    messageStreamRuntime = value;
  },
  shouldSkipOpenCodeStop: () => ENV_SKIP_OPENCODE_START || isExternalOpenCode,
  getOpenCodePort: () => openCodePort,
  getOpenCodeProcess: () => openCodeProcess,
  setOpenCodeProcess: (value) => {
    openCodeProcess = value;
  },
  killProcessOnPort,
  waitForPortRelease,
  getServer: () => server,
  getUiAuthController: () => uiAuthController,
  setUiAuthController: (value) => {
    uiAuthController = value;
  },
  getActiveTunnelController: () => activeTunnelController,
  setActiveTunnelController: (value) => {
    activeTunnelController = value;
  },
  tunnelAuthController,
  scheduledTasksRuntime,
});

/**
 * 优雅停机入口（转发到 gracefulShutdownRuntime）。
 * 供 CLI 信号处理器与嵌入宿主（Electron）共用。
 */
const gracefulShutdown = (...args) => gracefulShutdownRuntime.gracefulShutdown(...args);

/**
 * web server 主入口：装配整个 OMPChamber Node 服务并启动。
 *
 * 步骤概览：
 * 1. 归一化端口/host（options → 环境变量 → 127.0.0.1 兜底），并对暴露到
 *    LAN 且无密码的绑定做安全拦截（直接抛错）；
 * 2. 创建 agent-tool runtime 与配对（pairing）传输探测逻辑；
 * 3. 组装 express：robots/CORS/compression（SSE 路径跳过压缩）、基础
 *    状态与鉴权路由（bootstrapRuntime.setupBaseRoutes）、realtime proxy、
 *    隧道配线（tunnelWiringRuntime）、私有 relay 服务、浏览器控制路由、
 *    dev-server 隧道、feature 路由；
 * 4. 运行启动管线（startupPipelineRuntime）：静态资源、消息流 WebSocket、
 *    终端/听写 runtime、受管 OpenCode 启动与信号处理；
 * 5. 启动定时任务与 worktree watcher，按需 reconcile relay 并每分钟轮询；
 * 6. 返回 WebUiServerController（端口/隧道 URL/就绪状态/重启/停机）。
 *
 * 多数配置错误向上抛出由调用方决定退出方式；后台任务的启动失败只 warn 不阻断。
 * @param {object} [options] 启动选项（port/host/uiPassword/tunnel 等选项）
 * @returns {Promise<WebUiServerController>} 服务控制器
 */
async function main(options = {}) {
  const port = Number.isFinite(options.port) && options.port >= 0 ? Math.trunc(options.port) : DEFAULT_PORT;
  const host = typeof options.host === 'string' && options.host.length > 0 ? options.host : undefined;
  const effectiveBindHost = host
    || (typeof process.env.OMPCHAMBER_HOST === 'string' && process.env.OMPCHAMBER_HOST.trim().length > 0
      ? process.env.OMPCHAMBER_HOST.trim()
      : '127.0.0.1');
  // agent-tool runtime：为 omp 宿主内的 agent 提供工具执行通道，
  // 动作执行统一交给 openChamberControlService，端口取自当前 server。
  agentToolRuntime = createAgentToolRuntime({
    crypto,
    fsPromises,
    path,
    dataDir: OMPCHAMBER_DATA_DIR,
    env: process.env,
    executeAction: (...args) => openChamberControlService.execute(...args),
    getActivePort: () => {
      const address = server?.address?.();
      return typeof address === 'object' && address ? address.port : null;
    },
  });

  // 中文补充：配对传输的 LAN 可达性依据 server 实际绑定推导——通配绑定取
  // 机器 LAN IP、特定非回环绑定取该地址，而非 UI 的打开方式；仅回环绑定时
  // 不提供 LAN 链接（连上也连不通），英文原注释见下。
  // Pairing transports advertised to the create-device dialog. LAN reachability is
  // derived from the SERVER's actual bind (a wildcard bind → the machine's LAN IP;
  // a specific non-loopback host → that host), NOT from how the UI was opened — so
  // "Local network" works even when the UI is opened on localhost, and is absent
  // when the server is only bound to loopback (a LAN link would not connect).
  // The IPv4 the requesting client actually reached this server on (if any).
  // Strips the IPv6-mapped prefix; loopback means "not a LAN path".
  /** 取客户端实际连到本 server 的 IPv4 地址；剥离 IPv6-mapped 前缀，
   *  回环地址视为「非 LAN 路径」返回 null。 */
  const requestReachedLanAddress = (req) => {
    const raw = typeof req?.socket?.localAddress === 'string' ? req.socket.localAddress : '';
    const address = raw.startsWith('::ffff:') ? raw.slice(7) : raw;
    if (!/^\d+\.\d+\.\d+\.\d+$/.test(address)) return null;
    if (address.startsWith('127.')) return null;
    return address;
  };
  /**
   * 计算配对对话框展示的传输方式：本机回环 URL、LAN URL（优先取客户端
   * 已在实际通信的地址，网卡扫描仅作兜底）、以及 relay 是否可用。
   * @param {import('express').Request} req 请求
   * @returns {{ local: string, lan: string | null, relayAvailable: boolean }}
   */
  const resolvePairingTransports = (req) => {
    const activePort = tunnelRuntimeContext.getActivePort() || port;
    const local = `http://127.0.0.1:${activePort}`;
    let lanHost = null;
    if (isNetworkExposedBindHost(effectiveBindHost)) {
      // Prefer the address the client is ALREADY talking to us on — it is the
      // one interface guaranteed to be routable from that client's network.
      // Interface scanning is only a fallback: on servers with virtual bridges
      // (docker0 etc.) the first non-internal IPv4 can be an address no other
      // machine can reach, which produced pairing links whose LAN candidate
      // silently failed and forced devices onto the relay.
      lanHost = requestReachedLanAddress(req);
      try {
        if (!lanHost) {
          for (const list of Object.values(os.networkInterfaces())) {
            for (const entry of (list || [])) {
              if (entry.family === 'IPv4' && !entry.internal) { lanHost = entry.address; break; }
            }
            if (lanHost) break;
          }
        }
      } catch {
        lanHost = null;
      }
    } else {
      const h = String(effectiveBindHost || '').toLowerCase();
      if (h && h !== '127.0.0.1' && h !== 'localhost' && h !== '::1') lanHost = effectiveBindHost;
    }
    const lan = lanHost ? `http://${lanHost.includes(':') ? `[${lanHost}]` : lanHost}:${activePort}` : null;
    return { local, lan, relayAvailable: true };
  };
  // 中文补充：列出本 server 当前全部可直接访问的 LAN URL（客户端已用的
  // 地址优先——经 relay 隧道时是回环、拿不到任何东西——再加所有非内部
  // IPv4 网卡），供候选刷新端点替换客户端过期的 DHCP 地址（英文原注释见下）。
  // ALL direct LAN URLs this server is currently reachable on, for the
  // candidates-refresh endpoint: the address the requesting client already
  // reached us on first (guaranteed routable from its network — over the relay
  // tunnel this is loopback and yields nothing), then every non-internal IPv4
  // interface. A client that paired while the machine had a different DHCP
  // lease uses this to replace its stale LAN candidate.
  /**
   * 汇总所有直接可达的 LAN URL（去重），绑定仅回环时为空数组。
   * @param {import('express').Request} req 请求
   * @returns {string[]} http://host:port 形式的 URL 列表
   */
  const resolveDirectLanUrls = (req) => {
    const activePort = tunnelRuntimeContext.getActivePort() || port;
    const urls = [];
    const push = (host) => {
      if (typeof host !== 'string' || !host) return;
      const url = `http://${host.includes(':') ? `[${host}]` : host}:${activePort}`;
      if (!urls.includes(url)) urls.push(url);
    };
    if (isNetworkExposedBindHost(effectiveBindHost)) {
      push(requestReachedLanAddress(req));
      try {
        for (const list of Object.values(os.networkInterfaces())) {
          for (const entry of (list || [])) {
            if (entry.family === 'IPv4' && !entry.internal) push(entry.address);
          }
        }
      } catch {
        // interface scan failure → whatever we already collected
      }
    } else {
      const h = String(effectiveBindHost || '').toLowerCase();
      if (h && h !== '127.0.0.1' && h !== 'localhost' && h !== '::1') push(effectiveBindHost);
    }
    return urls;
  };
  // UI 密码：options 优先，其次 OMPCHAMBER_UI_PASSWORD 环境变量，
  // 都没有则为 null（沿用受管密码）。
  const uiPassword = typeof options.uiPassword === 'string'
    ? options.uiPassword
    : (typeof process.env.OMPCHAMBER_UI_PASSWORD === 'string' ? process.env.OMPCHAMBER_UI_PASSWORD : null);
  // 安全闸：绑定到非回滚地址（LAN 可达）、非 dev server、未设密码且未
  // 显式允许裸奔时，直接抛错拒绝启动，避免无鉴权暴露。
  if (
    isNetworkExposedBindHost(effectiveBindHost)
    && !isDevelopmentServer(process.env)
    && !(typeof uiPassword === 'string' && uiPassword.trim().length > 0)
    && !isUnsafeUnauthenticatedLanAllowed(process.env)
  ) {
    throw new Error(getUnauthenticatedLanErrorMessage(effectiveBindHost));
  }
  // 旧版 --cf-tunnel 兼容开关：请求 quick 模式的 Cloudflare 隧道。
  const tryCfTunnel = options.tryCfTunnel === true;
  // 是否以 API-only 模式运行（不服务静态 UI，只提供 API）。
  const apiOnly = options.apiOnly === true || isEnvFlagEnabled(process.env.OMPCHAMBER_API_ONLY);
  // 是否显式提供了任一 canonical 隧道配置项：只要有一项就走规范化的
  // 隧道启动请求，否则退回旧开关（--cf-tunnel）或不开隧道。
  const shouldUseCanonicalTunnelConfig = typeof options.tunnelMode === 'string'
    || typeof options.tunnelProvider === 'string'
    || options.tunnelConfigPath === null
    || typeof options.tunnelConfigPath === 'string'
    || typeof options.tunnelToken === 'string'
    || typeof options.tunnelHostname === 'string';
  // 归一化后的启动期隧道请求：canonical 配置优先，其次 --cf-tunnel，
  // 都没有则为 null（不启动隧道）。
  const startupTunnelRequest = shouldUseCanonicalTunnelConfig
    ? normalizeTunnelStartRequest({
        provider: normalizeTunnelProvider(options.tunnelProvider),
        mode: options.tunnelMode,
        configPath: normalizeOptionalPath(options.tunnelConfigPath),
        token: typeof options.tunnelToken === 'string' ? options.tunnelToken.trim() : '',
        hostname: normalizeManagedRemoteTunnelHostname(options.tunnelHostname),
      })
    : (tryCfTunnel
      ? {
          provider: TUNNEL_PROVIDER_CLOUDFLARE,
          mode: TUNNEL_MODE_QUICK,
          configPath: undefined,
          token: '',
          hostname: undefined,
        }
      : null);
  // 默认注册 SIGINT/SIGTERM 处理器；嵌入宿主（Electron）传 false 自行管理。
  const attachSignals = options.attachSignals !== false;
  // 隧道就绪后的回调（宿主可借此拿到公网 URL），未提供时为 null。
  const onTunnelReady = typeof options.onTunnelReady === 'function' ? options.onTunnelReady : null;
  // 宿主可覆盖停机后是否退出进程（默认 true）。
  if (typeof options.exitOnShutdown === 'boolean') {
    exitOnShutdown = options.exitOnShutdown;
  }
  // 桌面宿主回调注入：onDesktopNotification 转发桌面通知，
  // getIsWindowFocused 用于通知的窗口聚焦抑制。
  if (typeof options.onDesktopNotification === 'function') {
    notificationEmitterRuntime.setOnDesktopNotification(options.onDesktopNotification);
  }
  if (typeof options.getIsWindowFocused === 'function') {
    notificationTriggerRuntime.setGetIsWindowFocused(options.getIsWindowFocused);
  }
  // 桌面运行时配置读取器（Electron 注入），供 realtime proxy 使用；可为空。
  const getDesktopRuntimeConfig = typeof options.getDesktopRuntimeConfig === 'function'
    ? options.getDesktopRuntimeConfig
    : null;

  // 启动横幅：port=0 表示由内核自动分配端口。
  console.log(`Starting OMPChamber on port ${port === 0 ? 'auto' : port}`);

  // Voice enumeration is independent from route registration. Start it now,
  // but do not hold server listen or managed OpenCode startup on `say -v "?"`.
  // 语音能力（say -v ?）枚举与路由注册解耦：立即启动但不阻塞监听与 OpenCode 启动。
  const sayTTSCapability = detectSayTtsCapability(process);

  // 主 express 应用；后续所有中间件与路由都注册在它上面。
  const app = express();
  // server 启动时间戳（ISO 格式），状态接口展示 uptime 起点。
  const serverStartedAt = new Date().toISOString();
  // 打包客户端（自定义 scheme/Capacitor）与本地页面的合法 Origin 集合。
  const packagedClientOrigins = new Set([
    'ompchamber-ui://app',
    'capacitor://localhost',
    'http://localhost',
    'https://localhost',
  ]);
  // 本地开发服务器（vite 等 http://localhost:端口）的 Origin 判定。
  const isLocalDevClientOrigin = (origin) => /^https?:\/\/(localhost|127\.0\.0\.1):\d+$/.test(origin);
  // 信任代理头（X-Forwarded-For 等）：隧道/反代后仍能取到真实客户端信息。
  app.set('trust proxy', true);
  // 中文补充：所有响应都带 X-Robots-Tag noindex——应用壳是公开服务的
  //（密码提示页之前就会加载），不加则自托管实例会被搜索引擎收录；
  // robots.txt 路由再把意图显式告诉爬虫（英文原注释见下）。
  // Keep self-hosted instances out of search engines. The app shell is served
  // publicly (it loads before prompting for the UI password), so without this
  // even a password-protected instance gets crawled and indexed. Applies to
  // every response; the robots.txt route makes the intent explicit for crawlers.
  app.use((_req, res, next) => {
    res.setHeader('X-Robots-Tag', 'noindex, nofollow');
    next();
  });
  // robots.txt：显式禁止所有爬虫抓取。
  app.get('/robots.txt', (_req, res) => {
    res.type('text/plain').send('User-agent: *\nDisallow: /\n');
  });
  // CORS：仅放行打包客户端与本地 dev Origin 且携带凭证；OPTIONS 预检直接 204。
  app.use((req, res, next) => {
    const origin = typeof req.headers.origin === 'string' ? req.headers.origin : '';
    if (packagedClientOrigins.has(origin) || isLocalDevClientOrigin(origin)) {
      res.setHeader('Access-Control-Allow-Origin', origin);
      res.setHeader('Access-Control-Allow-Credentials', 'true');
      res.setHeader('Access-Control-Allow-Methods', 'GET,POST,PUT,PATCH,DELETE,OPTIONS');
      res.setHeader('Access-Control-Allow-Headers', 'Content-Type,Authorization,Accept,X-Requested-With,Cache-Control,X-OpenCode-Directory,X-OpenCode-Directory-Encoding,X-Omp-Epoch,Last-Event-ID,Ngrok-Skip-Browser-Warning');
      res.setHeader('Access-Control-Expose-Headers', 'x-next-cursor');
      res.setHeader('Vary', 'Origin');
      if (req.method === 'OPTIONS') {
        res.status(204).end();
        return;
      }
    }
    next();
  });
  // 压缩中间件：SSE（shouldSkipCompression）永不压缩，其余按默认过滤 + 1KB 阈值。
  app.use(compression({
    filter: (req, res) => {
      if (shouldSkipCompression(req, res)) return false;
      return compression.filter(req, res);
    },
    threshold: 1024,
  }));
  // 记录 express 应用实例（生命周期/停机路径读取）。
  expressApp = app;
  // HTTP server：express 之上承载 WebSocket 升级（realtime proxy、消息流等）。
  server = http.createServer(app);
  // realtime proxy runtime 占位；真正的实例在下方 attachRealtimeProxy 时替换。
  let realtimeProxyRuntime = { stop: () => {} };

  // 中文补充：relay 服务在更下方构造（依赖隧道 runtime 的活跃端口），
  // 这里注册的配对路由只在请求时惰性读取 relay 候选，晚绑定即可
  //（英文原注释见下）。
  // The relay service is constructed further below (it depends on the tunnel
  // runtime's active port). The pairing routes registered here only read the
  // relay candidate lazily at request time, so a late-bound holder is enough.
  let relayServiceInstance = null;

  // 中文补充：隧道 runtime 同样晚创建——基础路由先注册，
  // /api/system/info 在请求时才解析端口与隧道 URL（英文原注释见下）。
  // Same pattern for the tunnel runtime: created after the base routes so
  // /api/system/info resolves port + tunnel URL lazily at request time.
  let tunnelRuntimeContextHolder = null;

  // 注册基础路由（健康/状态/鉴权/通知订阅等），返回 UI 鉴权控制器等装配结果。
  const bootstrapResult = bootstrapRuntime.setupBaseRoutes(app, {
    process,
    ompchamberVersion: OMPCHAMBER_VERSION,
    runtimeName: process.env.OMPCHAMBER_RUNTIME || 'web',
    serverStartedAt,
    gracefulShutdown,
    // /api 健康快照：OpenCode 端口/就绪状态、诊断信息、二进制解析结果等。
    getHealthSnapshot: () => {
      const launchSpec = resolvedOpencodeBinary
        ? resolveManagedOpenCodeLaunchSpec(resolvedOpencodeBinary)
        : null;
      return {
        openCodePort,
        openCodeRunning: Boolean(openCodePort && isOpenCodeReady && !isRestartingOpenCode),
        openCodeSecureConnection: isOpenCodeConnectionSecure(),
        openCodeAuthSource: openCodeAuthSource || null,
        openCodeApiPrefix: '',
        openCodeApiPrefixDetected: true,
        isOpenCodeReady,
        lastOpenCodeError,
        lastOpenCodeLaunchDiagnostics,
        lastOpenCodeHealthFailure,
        lastManagedOpenCodeProcess,
        lastOpenCodeRestartDiagnostics,
        opencodeBinaryResolved: resolvedOpencodeBinary || null,
        opencodeBinarySource: resolvedOpencodeBinarySource || null,
        opencodeLaunchBinary: launchSpec?.binary || null,
        opencodeLaunchArgs: launchSpec?.args || [],
        opencodeLaunchWrapperType: launchSpec?.wrapperType || null,
        nodeBinaryResolved: resolvedNodeBinary || null,
        bunBinaryResolved: resolvedBunBinary || null,
        desktopNotifyEnabled: ENV_DESKTOP_NOTIFY,
        planModeExperimentalEnabled: PLAN_MODE_EXPERIMENT_ENABLED,
        apiOnly,
      };
    },
    // 中文补充：本实例的服务端口与活跃隧道公网 URL（如有），供
    // /api/system/info；因隧道 runtime 在这些路由之后创建，故惰性解析
    //（英文原注释见下）。
    // Port this instance serves on and the active tunnel's public URL (if
    // any), for /api/system/info. Resolved lazily because the tunnel runtime
    // is created after these base routes are registered.
    getServerPort: () => {
      const activePort = tunnelRuntimeContextHolder?.getActivePort?.();
      if (Number.isFinite(activePort) && activePort > 0) return activePort;
      return Number.isFinite(port) && port > 0 ? port : null;
    },
    getTunnelUrl: () => tunnelRuntimeContextHolder?.tunnelService?.getPublicUrl?.() ?? null,
    verboseRequestLogs: OMPCHAMBER_VERBOSE_REQUEST_LOGS,
    uiPassword,
    tunnelAuthController,
    remoteClientAuthRuntime,
    clientPairingRuntime,
    // relay 配对候选：配对链接按需启用 relay（ensureEnabled），
    // 普通链接只在 relay 已开启时才广告它。
    getRelayPairingCandidate: (options) => {
      if (!relayServiceInstance) return null;
      // A relay pairing link enables the relay on demand; a plain link only
      // advertises relay when it is already on.
      return options?.ensureEnabled
        ? relayServiceInstance.ensureEnabledForPairing()
        : relayServiceInstance.getPairingCandidate();
    },
    // 配对/设备变化后重新评估 relay 生命周期（吊销或兑换会翻转 relay 需求）。
    // Re-evaluate the relay lifecycle after pairing/device changes (a revoked or
    // redeemed device can flip relay demand on or off).
    reconcileRelay: () => (relayServiceInstance ? relayServiceInstance.reconcile() : Promise.resolve()),
    getPairingTransports: resolvePairingTransports,
    getDirectCandidateUrls: resolveDirectLanUrls,
    // 稳定的 server 标识：客户端用它校验学习到的地址是否仍属于本机；惰性解析。
    // Stable server identity for client-side verification of learned addresses.
    // Lazily resolved: the relay service is constructed after these routes.
    getServerId: () => (relayServiceInstance ? relayServiceInstance.getServerId() : Promise.resolve(null)),
    // 配对设备展示的本 server 名称：取机器主机名而非运营者输入的配对标签。
    // The display name a paired device shows for THIS server. Devices name the
    // connection by the issuing machine's hostname, not the per-device pairing
    // label typed by the operator.
    getServerLabel: () => {
      try {
        const name = os.hostname();
        return typeof name === 'string' && name.trim().length > 0 ? name.trim() : 'OMPChamber';
      } catch {
        return 'OMPChamber';
      }
    },
    readSettingsFromDiskMigrated,
    normalizeTunnelSessionTtlMs,
    sayTTSCapability,
    ensurePushInitialized,
    ensureGlobalWatcherStarted,
    getOrCreateVapidKeys,
    getUiSessionTokenFromRequest,
    writeSettingsToDisk,
    addOrUpdatePushSubscription,
    removePushSubscription,
    addOrUpdateApnsToken,
    removeApnsToken,
    updateUiVisibility,
    clearPendingPushBadge: () => clearPendingPushBadge(),
    isUiVisible,
    getUiNotificationClients: () => uiNotificationClients,
    writeSseEvent,
    sessionRuntime,
    setPushInitialized,
    fs,
    os,
    path,
    server,
    __dirname,
    ompchamberDataDir: OMPCHAMBER_DATA_DIR,
    modelsDevApiUrl: MODELS_DEV_API_URL,
    modelsMetadataCacheTtl: MODELS_METADATA_CACHE_TTL,
    fetchFreeZenModels,
    getCachedZenModels,
    setAutoAcceptSession,
    agentToolRuntime,
  });
  // UI 鉴权控制器（会话 token 签发与校验），供后续各组件共享。
  uiAuthController = bootstrapResult.uiAuthController;
  // 挂载 realtime proxy（WebSocket/SSE 通道）并取得可停止的 runtime 实例。
  realtimeProxyRuntime = attachRealtimeProxy({
    app,
    server,
    getDesktopRuntimeConfig,
    getUiAuthController: () => uiAuthController,
    isRequestOriginAllowed,
  });

  // 初始化隧道配线：注册隧道管理路由，返回含 activePort/tunnelService 的上下文。
  const tunnelRuntimeContext = tunnelWiringRuntime.initialize(app, port);
  // 解构出隧道服务与「以归一化请求启动隧道」的入口。
  const { tunnelService, startTunnelWithNormalizedRequest } = tunnelRuntimeContext;
  tunnelRuntimeContextHolder = tunnelRuntimeContext;

  // 中文补充：私有 relay 宿主服务——配置、管理路由与宿主客户端生命周期；
  // 回环端口与隧道同源，保证经 relay 隧道的请求仍打到本地 express
  //（英文原注释见下）。
  // Private relay host service: config + management routes + host client
  // lifecycle. Loopback port comes from the same source the tunnel uses so
  // relay-tunneled requests hit the local Express app on 127.0.0.1.
  const relayService = createRelayService({
    crypto,
    os,
    readSettingsFromDiskMigrated,
    writeSettingsToDisk,
    readSettingsStrict: readSettingsFromDiskStrict,
    remoteClientAuthRuntime,
    getLocalPort: () => tunnelRuntimeContext.getActivePort(),
    // One relay host per machine: every instance sharing this data dir shares
    // the relay identity (serverId), so concurrent hosts evict each other at
    // the relay worker and devices land on a random local instance.
    hostLock: createRelayHostLock({
      lockFilePath: path.join(OMPCHAMBER_DATA_DIR, 'relay-host.lock'),
      fs,
      process,
    }),
    // Dev/debug instances share the data dir (and thus the relay identity) with
    // the production instance, so they must not host the relay on their own —
    // paired devices would land on them. OMPCHAMBER_RELAY_HOST=off disables
    // passive hosting explicitly (dev scripts set it); the Electron dev shell is
    // covered via OMPCHAMBER_ELECTRON_DEV. OMPCHAMBER_RELAY_HOST=on overrides
    // both. Explicit enable/pairing on the instance still hosts regardless.
    allowPassiveHost: process.env.OMPCHAMBER_RELAY_HOST === 'on'
      || (process.env.OMPCHAMBER_RELAY_HOST !== 'off' && process.env.OMPCHAMBER_ELECTRON_DEV !== '1'),
    // Relay demand = any paired device or pending pairing session that uses the
    // relay transport. Drives the auto on/off lifecycle.
    hasRelayDemand: async () => {
      // A store read failure must NOT masquerade as "no demand": reconcile
      // persists enabled=false and severs paired devices. Any affirmative
      // answer wins; otherwise a failed check aborts reconcile (throw) so the
      // relay keeps its current state until a trustworthy read succeeds.
      const [pendingRelay, deviceRelay] = await Promise.allSettled([
        clientPairingRuntime.hasActiveRelaySession(),
        remoteClientAuthRuntime.hasActiveRelayClients(),
      ]);
      if (pendingRelay.status === 'fulfilled' && pendingRelay.value) return true;
      if (deviceRelay.status === 'fulfilled' && deviceRelay.value) return true;
      if (pendingRelay.status === 'rejected') throw pendingRelay.reason;
      if (deviceRelay.status === 'rejected') throw deviceRelay.reason;
      return false;
    },
  });
  relayServiceInstance = relayService;
  relayService.registerRoutes(app);

  // 浏览器控制路由：把浏览器操作请求交给 broker 广播给持有浏览器面板的客户端。
  registerBrowserControlRoutes(app, { express, broker: browserControlBroker });

  // 中文补充：同一个扫描器同时支撑发现列表与隧道 allowlist——用户能
  // 看到的端口恰好就是隧道会拨号的端口（英文原注释见下）。
  // One scanner backs both discovery and the tunnel allowlist, so a port the
  // user can see is exactly a port the tunnel will dial.
  const devServerScanner = createDevServerScanner({ spawn, platform: process.platform });
  // 发现本机 dev server 时排除 OMPChamber 自己监听的两个端口。
  const listDevServers = () => devServerScanner.discover({
    ownPorts: [port, openCodePort].filter((value) => Number.isInteger(value) && value > 0),
  });

  // dev 隧道 runtime：为发现的 dev server 建立隧道并做升级鉴权。
  createDevTunnelRuntime({
    server,
    discoverDevServers: listDevServers,
    uiAuthController,
    isRequestOriginAllowed,
    rejectWebSocketUpgrade,
    logger: console,
  });

  // 注册 feature 路由（设置、主题、项目、agent memory、定时任务等大块 API）。
  await featureRoutesRuntime.registerRoutes(app, {
    crypto,
    fs,
    os,
    path,
    fsPromises,
    spawn,
    resolveGitBinaryForSpawn,
    createFsSearchRuntime: createFsSearchRuntimeFactory,
    ompchamberDataDir: OMPCHAMBER_DATA_DIR,
    ompchamberUserConfigRoot: OMPCHAMBER_USER_CONFIG_ROOT,
    normalizeDirectoryPath,
    resolveProjectDirectory,
    resolveOptionalProjectDirectory,
    validateDirectoryPath,
    readCustomThemesFromDisk,
    refreshOpenCodeAfterConfigChange,
    getOpenCodeResolutionSnapshot,
    formatSettingsResponse,
    readSettingsFromDisk,
    readSettingsFromDiskMigrated,
    persistSettings,
    sanitizeProjects,
    sanitizeSkillCatalogs,
    isUnsafeSkillRelativePath,
    buildOpenCodeUrl,
    getOpenCodeAuthHeaders,
    // 受管 OpenCode 的端口（未就绪时可能为 null）。
    getOpenCodePort: () => openCodePort,
    // 中文补充：dev-server 发现不得把 OMPChamber 自己的监听端口当作
    // 可预览的 dev server 提供给用户（英文原注释见下）。
    // Dev-server discovery must not offer OMPChamber's own listeners back to
    // the user as something to preview.
    getOwnPorts: () => [port, openCodePort].filter((value) => Number.isInteger(value) && value > 0),
    devServerScanner,
    buildAugmentedPath,
    projectConfigRuntime,
    projectContextRuntime,
    agentMemoryRuntime,
    isAgentMemoryEnabled,
    sessionKnowledgeRuntime,
    scheduledTasksRuntime,
    scheduledTaskService,
    openChamberSessionService,
    openChamberControlService,
    waitForOpenCodeReady,
    emitSessionCreatedEvent,
    getOMPChamberEventClients: () => uiOMPChamberEventClients,
    writeSseEvent,
    permissionAutoAcceptRuntime,
  });

  // 启动管线：静态资源、消息流 WS、终端/听写 runtime、受管 OpenCode
  // 启动、信号挂载，返回各 runtime 实例。
  const startupPipelineResult = await startupPipelineRuntime.run({
    app,
    server,
    express,
    fs,
    path,
    uiAuthController,
    buildAugmentedPath,
    searchPathFor,
    isExecutable,
    isRequestOriginAllowed,
    rejectWebSocketUpgrade,
    buildOpenCodeUrl,
    getOpenCodeAuthHeaders,
    globalEventHub: globalMessageStreamHub,
    processForwardedEventPayload,
    messageStreamWsClients: uiNotificationWsClients,
    upstreamStallTimeoutMs: getUpstreamStallTimeoutMs,
    terminalHeartbeatIntervalMs: TERMINAL_INPUT_WS_HEARTBEAT_INTERVAL_MS,
    terminalRebindWindowMs: TERMINAL_INPUT_WS_REBIND_WINDOW_MS,
    terminalMaxRebindsPerWindow: TERMINAL_INPUT_WS_MAX_REBINDS_PER_WINDOW,
    setupProxy,
    scheduleOpenCodeApiDetection,
    bootstrapOpenCodeAtStartup,
    triggerHealthCheck,
    staticRoutesRuntime,
    process,
    crypto,
    normalizeTunnelBootstrapTtlMs,
    readSettingsFromDiskMigrated,
    tunnelAuthController,
    startTunnelWithNormalizedRequest,
    gracefulShutdown,
    getSignalsAttached: () => signalsAttached,
    setSignalsAttached: (value) => {
      signalsAttached = value;
    },
    syncToHmrState,
    TUNNEL_MODE_QUICK,
    TUNNEL_MODE_MANAGED_LOCAL,
    TUNNEL_MODE_MANAGED_REMOTE,
    host,
    port,
    startupTunnelRequest,
    onTunnelReady,
    tunnelRuntimeContext,
    attachSignals,
    apiOnly,
    dictationModelsDir: path.join(OMPCHAMBER_USER_CONFIG_ROOT, 'speech-models'),
  });
  // 取出启动管线创建的终端/听写/消息流 runtime 供停机与重启路径使用。
  terminalRuntime = startupPipelineResult.terminalRuntime;
  dictationRuntime = startupPipelineResult.dictationRuntime;
  messageStreamRuntime = startupPipelineResult.messageStreamRuntime;

  // 定时任务 runtime 启动失败不阻断 server（只 warn）。
  try {
    await scheduledTasksRuntime.start();
  } catch (error) {
    console.warn('[ScheduledTasks] Failed to start runtime:', error?.message || error);
  }

  // worktree watcher 启动失败同样不阻断 server。
  try {
    worktreeWatcherRuntime.start();
  } catch (error) {
    console.warn('[WorktreeWatcher] Failed to start:', error?.message || error);
  }

  // 中文补充：仅在用户开启配置时才建立 relay 控制连接；启动时按需求
  // reconcile——存在 relay 设备/会话则运行，否则停用并清理过期的启用
  // 标记（英文原注释见下）。
  // Only opens a relay control socket when the user opted in (config enabled).
  // Reconcile the relay lifecycle from demand on startup: run it if any relay
  // device/session exists, stop it (and clear a stale enabled flag) otherwise.
  void relayService.reconcile();

  // 中文补充：relay 需求可能在没有任何请求到达时变化——`ompchamber
  // connect-url --relay` 直接向磁盘写入待处理会话，且待处理会话会自行
  // 过期；每分钟轮询 reconcile 让 headless 实例及时跟进（英文原注释见下）。
  // Relay demand can change outside our routes: `ompchamber connect-url
  // --relay` writes a pending relay session straight to the on-disk store, and
  // pending sessions expire without any request hitting us. Poll reconcile so a
  // headless instance picks the relay up (or drops it) within a minute.
  const relayReconcileTimer = setInterval(() => {
    void relayService.reconcile();
  }, 60_000);
  relayReconcileTimer.unref?.();

  // 返回给调用方的控制器：端口/隧道查询、就绪状态、重启与停机。
  return {
    expressApp: app,
    httpServer: server,
    // 实际监听端口（隧道活跃时为隧道端口）。
    getPort: () => tunnelRuntimeContext.getActivePort(),
    getOpenCodePort: () => openCodePort,
    // 活跃隧道的公网 URL；无隧道时为 null。
    getTunnelUrl: () => tunnelService.getPublicUrl(),
    // 退出风险状态：隧道是否活跃、定时任务是否在跑（UI 据此提示确认）。
    getQuitRiskStatus: () => ({
      tunnel: {
        active: Boolean(tunnelService.getPublicUrl()),
      },
      scheduledTasks: scheduledTasksRuntime.getStatus(),
    }),
    // OpenCode 是否就绪。
    isReady: () => isOpenCodeReady,
    // 重启受管 OpenCode 并等待就绪。
    restartOpenCode: () => restartOpenCode(),
    // 受管 OpenCode 进程信息：仅当进程确属本 server 管理时才暴露
    // pid/port，防止 Electron 侧按端口清理时误杀用户自己的外部实例。
    getOpenCodeProcessInfo: () => {
      const managed = Boolean((openCodeProcess || openCodePort) && !ENV_SKIP_OPENCODE_START && !isExternalOpenCode);
      // Only ever expose pid/port for a server WE manage. The Electron-side
      // killer kills by port (lsof + kill -KILL), so returning a port we don't
      // own — e.g. an external/desktop OpenCode on 4096 we attached to — would
      // let a single miscomputed `managed` flag take down the user's separate
      // server. Structurally withhold what isn't ours so the killer has no
      // target, instead of relying on the flag check alone.
      return {
        managed,
        pid: managed && typeof openCodeProcess?.pid === 'number' ? openCodeProcess.pid : null,
        port: managed ? openCodePort : null,
      };
    },
    // 停止 server：各 runtime 尽力而为地清理，最终走优雅停机。
    stop: (shutdownOptions = {}) => {
      realtimeProxyRuntime.stop();
      clearInterval(relayReconcileTimer);
      try {
        worktreeWatcherRuntime.stop();
      } catch {
        // best-effort teardown of fs watchers
      }
      try {
        relayService.stop();
      } catch {
        // best-effort teardown of the relay host client
      }
      try {
        dictationRuntime?.stop?.();
      } catch {
        // best-effort shutdown of the dictation worker
      }
      return gracefulShutdown({ exitProcess: shutdownOptions.exitProcess ?? false });
    }
  };
}

/**
 * CLI 入口判定：本模块作为主程序运行（node index.js / ompchamber-server）
 * 时解析 `serve` 命令行参数并调用 main 启动 server；被 import 时不执行。
 */
runCliEntryIfMain({
  process,
  currentFilename: __filename,
  parseServeCliOptions,
  defaultPort: DEFAULT_PORT,
  cloudflareProvider: TUNNEL_PROVIDER_CLOUDFLARE,
  managedLocalMode: TUNNEL_MODE_MANAGED_LOCAL,
  setExitOnShutdown: (value) => {
    exitOnShutdown = value;
  },
  startServer: main,
});

/**
 * 对外公共 API：桌面（Electron）宿主与集成测试通过这些导出驱动 server。
 */
export {
  gracefulShutdown,
  setupProxy,
  restartOpenCode,
  main as startWebUiServer,
  parseServeCliOptions as parseArgs,
};
