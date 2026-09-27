/**
 * Cursor 配额 provider。
 *
 * 依次从环境变量（CURSOR_*_TOKEN）、token 文件、受管凭据存储取得
 * access/refresh token，过期时用 refresh token 换新并按来源回写；
 * 调用 Cursor DashboardService 的 connect 协议接口，组装计费周期、
 * auto/API、on-demand 与积分余额等用量窗口。
 */

import { existsSync, readFileSync } from 'fs';
import { homedir } from 'os';
import { join } from 'path';
import { execFileSync } from 'child_process';
import { readManagedCredential, writeManagedCredential } from '../credentials/providers.js';
import {
  buildResult,
  formatMoney,
  toNumber,
  toTimestamp,
  toUsageWindow
} from '../utils/index.js';

/** Cursor 后端 API 基地址。 */
const BASE_URL = 'https://api2.cursor.sh';
/** 当前计费周期用量端点。 */
const USAGE_URL = `${BASE_URL}/aiserver.v1.DashboardService/GetCurrentPeriodUsage`;
/** 订阅计划信息端点。 */
const PLAN_URL = `${BASE_URL}/aiserver.v1.DashboardService/GetPlanInfo`;
/** 积分（credit grants）余额端点。 */
const CREDITS_URL = `${BASE_URL}/aiserver.v1.DashboardService/GetCreditGrantsBalance`;
/** OAuth token 刷新端点。 */
const REFRESH_URL = `${BASE_URL}/oauth/token`;
/** 刷新 token 所用的公开 client_id。 */
const CLIENT_ID = 'KbZUR41cY7W6zRSdpSUJ7I7mLYBKOCmB';
/** 刷新提前量：距过期不足 5 分钟即刷新 access token。 */
const REFRESH_BUFFER_MS = 5 * 60 * 1000;
/** macOS 上 Cursor 内置 SQLite 状态库路径，导入凭据时从中读取 token。 */
const STATE_DB = join(homedir(), 'Library', 'Application Support', 'Cursor', 'User', 'globalStorage', 'state.vscdb');

/** provider 标识（quota 路由与受管凭据存储使用）。 */
export const providerId = 'cursor';
/** 展示用名称；拿到订阅名后会展示为 `Cursor <planName>`。 */
export const providerName = 'Cursor';
/** provider 别名列表（供配置匹配）。 */
const aliases = ['cursor'];

/** 解码 JWT payload（base64url → JSON），缺失或解析失败返回 null。 */
const readJwtPayload = (token) => {
  try {
    const [, payload] = String(token).split('.');
    if (!payload) return null;
    return JSON.parse(Buffer.from(payload, 'base64url').toString('utf8'));
  } catch {
    return null;
  }
};

/**
 * 用 sqlite3 CLI 从 Cursor 的 state.vscdb 查询 ItemTable 中指定 key 的
 * 字符串值（key 中的单引号加倍转义防注入，stdio 静默避免污染输出）。
 * 库不存在、命令失败或值为空均返回 null。
 */
const readStateValue = (key) => {
  if (!existsSync(STATE_DB)) return null;
  try {
    const escapedKey = String(key).replace(/'/g, "''");
    const rows = execFileSync('sqlite3', [
      '-json',
      STATE_DB,
      `SELECT value FROM ItemTable WHERE key = '${escapedKey}' LIMIT 1;`
    ], {
      encoding: 'utf8',
      windowsHide: true,
      stdio: ['ignore', 'pipe', 'ignore']
    });
    const parsed = JSON.parse(rows || '[]');
    const value = parsed?.[0]?.value;
    return typeof value === 'string' && value.trim() ? value.trim() : null;
  } catch {
    return null;
  }
};

/** 读取 token 文件内容并 trim；路径为空、文件不存在或读取失败返回 null。 */
const readFileToken = (path) => {
  try {
    if (!path || !existsSync(path)) return null;
    const content = readFileSync(path, 'utf8').trim();
    return content || null;
  } catch {
    return null;
  }
};

/**
 * 按优先级解析认证来源：环境变量 → CURSOR_*_TOKEN_FILE 指向的文件 →
 * 受管凭据存储。返回带 source 标记的 token 对（source 决定刷新后的
 * access token 能否回写：仅 managed 来源会落盘）。
 */
const loadAuthState = () => {
  const envAccessToken = process.env.CURSOR_TOKEN || process.env.CURSOR_ACCESS_TOKEN || null;
  const envRefreshToken = process.env.CURSOR_REFRESH_TOKEN || null;
  const accessTokenPath = process.env.CURSOR_TOKEN_FILE || null;
  const refreshTokenPath = process.env.CURSOR_REFRESH_TOKEN_FILE || null;
  const fileAccessToken = readFileToken(accessTokenPath);
  const fileRefreshToken = readFileToken(refreshTokenPath);

  if (envAccessToken || envRefreshToken) {
    return {
      accessToken: envAccessToken,
      refreshToken: envRefreshToken,
      source: 'env'
    };
  }

  if (fileAccessToken || fileRefreshToken) {
    return {
      accessToken: fileAccessToken,
      refreshToken: fileRefreshToken,
      source: 'file'
    };
  }

  const managed = readManagedCredential(providerId);
  return {
    accessToken: managed?.accessToken || null,
    refreshToken: managed?.refreshToken || null,
    source: 'managed'
  };
};

/** access token 是否需要刷新：缺失、payload 无 exp、或距过期不足 REFRESH_BUFFER_MS。 */
const tokenNeedsRefresh = (token) => {
  if (!token) return true;
  const payload = readJwtPayload(token);
  const expiresAt = typeof payload?.exp === 'number' ? payload.exp * 1000 : null;
  return !expiresAt || expiresAt - Date.now() <= REFRESH_BUFFER_MS;
};

/** 刷新成功后回写 access token；仅当来源为受管存储时落盘，env/file 来源不回写。 */
const persistAccessToken = (auth, accessToken) => {
  if (auth.source === 'managed') writeManagedCredential(providerId, { accessToken, refreshToken: auth.refreshToken || '' });
};

/**
 * 从本机 Cursor 的 state.vscdb 导入凭据：读取 access/refresh token，
 * 立即刷新验证可用性（不可用则抛错），然后写入受管存储并返回脱敏状态。
 */
export const importCursorCredential = async () => {
  const credential = {
    accessToken: readStateValue('cursorAuth/accessToken') || '',
    refreshToken: readStateValue('cursorAuth/refreshToken') || '',
  };
  if (!credential.accessToken && !credential.refreshToken) throw new Error('Cursor credentials are unavailable');
  const accessToken = await resolveCredentialAccessToken({ ...credential, source: 'import' });
  if (!accessToken) throw new Error('Cursor credentials are invalid');
  return writeManagedCredential(providerId, { ...credential, accessToken });
};

/**
 * 用 refresh token 换新 access token；无 refresh token 时原样返回旧值。
 * 服务端要求重新登录（shouldLogout）或 401 时抛出面向用户的会话过期
 * 错误，其余非 2xx 与响应缺 token 也抛错；成功则按来源回写并返回新 token。
 */
const refreshAccessToken = async (auth) => {
  if (!auth.refreshToken) return auth.accessToken;

  const response = await fetch(REFRESH_URL, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({
      grant_type: 'refresh_token',
      client_id: CLIENT_ID,
      refresh_token: auth.refreshToken
    })
  });

  const body = await response.json().catch(() => null);
  if (body?.shouldLogout === true) {
    throw new Error('Session expired - please sign in to Cursor again');
  }
  if (!response.ok) {
    throw new Error(response.status === 401 ? 'Cursor session expired' : `API error: ${response.status}`);
  }
  if (typeof body?.access_token !== 'string' || !body.access_token) {
    throw new Error('Cursor refresh response did not include an access token');
  }

  persistAccessToken(auth, body.access_token);
  return body.access_token;
};

/** 解析出可用的 access token：token 对为空返回 null，仍新鲜直接返回，否则刷新。 */
const resolveCredentialAccessToken = async (auth) => {
  if (!auth.accessToken && !auth.refreshToken) return null;
  if (!tokenNeedsRefresh(auth.accessToken)) return auth.accessToken;
  return refreshAccessToken(auth);
};

/** 按默认来源链（loadAuthState）解析当前可用的 access token。 */
const resolveAccessToken = async () => resolveCredentialAccessToken(loadAuthState());

/**
 * 校验候选凭据：刷新/取得 access token 后真实调用一次用量接口，
 * 任何失败抛错。供凭据保存前的预检与手动校验路由复用。
 */
export const validateCursorCredential = async (credential) => {
  const accessToken = await resolveCredentialAccessToken({ ...credential, source: 'validation' });
  if (!accessToken) throw new Error('Cursor credentials are invalid');
  await connectPost(USAGE_URL, accessToken);
};

/**
 * 发起 Cursor connect 协议 POST（Bearer token + 空 JSON body），
 * 401 视为会话过期、其余非 2xx 报 API 错误，成功返回解析后的 JSON。
 */
const connectPost = async (url, accessToken) => {
  const response = await fetch(url, {
    method: 'POST',
    headers: {
      Authorization: `Bearer ${accessToken}`,
      'Content-Type': 'application/json',
      'Connect-Protocol-Version': '1'
    },
    body: '{}'
  });

  if (!response.ok) {
    throw new Error(response.status === 401 ? 'Cursor session expired' : `API error: ${response.status}`);
  }

  return response.json();
};

/** 把美分值格式化为 `$xx.xx` 字符串；非法输入返回 null。 */
const centsLabel = (cents) => {
  const value = toNumber(cents);
  return value === null ? null : `$${formatMoney(value / 100)}`;
};

/** 计算用量百分比：优先接口显式给出的 totalPercentUsed，否则由 limit/remaining 推算并夹在 0-100。 */
const percentFromSpend = (planUsage) => {
  const explicit = toNumber(planUsage?.totalPercentUsed);
  if (explicit !== null) return explicit;
  const limit = toNumber(planUsage?.limit);
  const remaining = toNumber(planUsage?.remaining);
  if (!limit || remaining === null) return null;
  return Math.min(100, Math.max(0, ((limit - remaining) / limit) * 100));
};

/**
 * 由用量与计划响应组装用量窗口：billing_cycle（总消耗 + 计划额度）、
 * auto、api（有显式百分比才生成）、plan_limit 与 on-demand（个人/共享
 * 上限取先存在者）。所有窗口共享计费周期重置时间，数据缺失则跳过对应窗口。
 */
const buildWindows = (usage, plan) => {
  const planUsage = usage?.planUsage ?? {};
  const spendLimitUsage = usage?.spendLimitUsage ?? {};
  const resetAt = toTimestamp(usage?.billingCycleEnd ?? plan?.planInfo?.billingCycleEnd);
  const windowSeconds = resetAt ? Math.max(0, Math.floor((resetAt - Date.now()) / 1000)) : null;
  const windows = {};

  windows.billing_cycle = toUsageWindow({
    usedPercent: percentFromSpend(planUsage),
    windowSeconds,
    resetAt,
    valueLabel: centsLabel(planUsage.totalSpend)
  });

  const autoPercent = toNumber(planUsage.autoPercentUsed);
  if (autoPercent !== null) {
    windows.auto = toUsageWindow({ usedPercent: autoPercent, windowSeconds, resetAt });
  }

  const apiPercent = toNumber(planUsage.apiPercentUsed);
  if (apiPercent !== null) {
    windows.api = toUsageWindow({ usedPercent: apiPercent, windowSeconds, resetAt });
  }

  const planLimit = centsLabel(planUsage.limit);
  if (planLimit) {
    const limit = toNumber(planUsage.limit);
    const remaining = toNumber(planUsage.remaining);
    windows.plan_limit = toUsageWindow({
      usedPercent: limit && remaining !== null
        ? Math.min(100, Math.max(0, ((limit - remaining) / limit) * 100))
        : null,
      windowSeconds,
      resetAt,
      valueLabel: `${centsLabel(planUsage.remaining) ?? '$0.00'} remaining of ${planLimit}`
    });
  }

  const onDemandLimit = toNumber(spendLimitUsage.individualLimit) ?? toNumber(spendLimitUsage.pooledLimit);
  if (onDemandLimit && onDemandLimit > 0) {
    const remaining = toNumber(spendLimitUsage.individualRemaining) ?? toNumber(spendLimitUsage.pooledRemaining) ?? 0;
    windows.on_demand = toUsageWindow({
      usedPercent: Math.min(100, Math.max(0, ((onDemandLimit - remaining) / onDemandLimit) * 100)),
      windowSeconds,
      resetAt,
      valueLabel: `${centsLabel(remaining) ?? '$0.00'} remaining of ${centsLabel(onDemandLimit)}`
    });
  }

  return windows;
};

/** 能取到积分余额（美分）时追加一个仅展示余额、无百分比/重置时间的 credits 窗口。 */
const appendCreditsWindow = (windows, credits) => {
  const balance = toNumber(credits?.balanceCents ?? credits?.totalBalanceCents ?? credits?.amountCents);
  if (balance === null) return;
  windows.credits = toUsageWindow({
    usedPercent: null,
    windowSeconds: null,
    resetAt: null,
    valueLabel: centsLabel(balance)
  });
};

/** 任一来源存在 access 或 refresh token 即视为已配置（不发起网络请求）。 */
export const isConfigured = () => {
  const auth = loadAuthState();
  return Boolean(auth.accessToken || auth.refreshToken);
};

/**
 * 配额查询入口：解析 access token（必要时刷新），并发拉取用量/计划/
 * 积分（计划与积分失败不致命）。无有效 token 返回未配置，无订阅返回
 * 错误结果；成功组装窗口，其余异常一律转为 ok:false 的结果对象。
 */
export const fetchQuota = async () => {
  const accessToken = await resolveAccessToken();
  if (!accessToken) {
    return buildResult({
      providerId,
      providerName,
      ok: false,
      configured: false,
      error: 'Not configured'
    });
  }

  try {
    const [usage, plan, credits] = await Promise.all([
      connectPost(USAGE_URL, accessToken),
      connectPost(PLAN_URL, accessToken).catch(() => null),
      connectPost(CREDITS_URL, accessToken).catch(() => null)
    ]);

    if (usage?.enabled === false || !usage?.planUsage) {
      return buildResult({
        providerId,
        providerName,
        ok: false,
        configured: true,
        error: 'No active Cursor subscription'
      });
    }

    const windows = buildWindows(usage, plan);
    appendCreditsWindow(windows, credits);

    return buildResult({
      providerId,
      providerName: plan?.planInfo?.planName ? `Cursor ${plan.planInfo.planName}` : providerName,
      ok: true,
      configured: true,
      usage: { windows }
    });
  } catch (error) {
    return buildResult({
      providerId,
      providerName,
      ok: false,
      configured: true,
      error: error instanceof Error ? error.message : 'Request failed'
    });
  }
};
