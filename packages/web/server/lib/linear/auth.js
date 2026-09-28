/**
 * Linear OAuth 凭据与配置的本地持久化模块。
 *
 * 管理 linear-auth.json（多 workspace 授权列表）与 settings.json（client id/secret、
 * scopes、broker 地址、会话评论开关等）的读写，并向路由层提供授权查询、写入、
 * 激活、清除以及对外暴露的安全状态摘要。
 */
import fs from 'fs';
import path from 'path';
import os from 'os';
import { isPlainObject, readEnv, readFiniteNumber, readTrimmedString } from './parse.js';

/** 内置默认的 Linear OAuth client id（环境变量与 settings 均未配置时使用）。 */
const DEFAULT_LINEAR_CLIENT_ID = '91bbe26a69a2c8568d3683f1e01e776c';
/** 默认向 Linear 申请的 OAuth scope 列表（读取、写入、创建评论）。 */
const DEFAULT_LINEAR_SCOPES = 'read,write,comments:create';
/** 默认的 OAuth broker（云端中转授权）服务地址。 */
const DEFAULT_LINEAR_BROKER_URL = 'https://api.openchamber.dev/v1/oauth/linear';
/** access token 过期判定的提前量（2 分钟）：距过期不足该窗口即视为过期，便于提前刷新。 */
const ACCESS_TOKEN_REFRESH_SKEW_MS = 2 * 60_000;
/** 无法从组织或用户信息推导出 workspace 时使用的兜底 workspace 标识（兼容旧版单账号数据）。 */
const LEGACY_WORKSPACE_ID = 'legacy';
/** settings.json 中"会话状态评论"开关对应的键名。 */
const SESSION_COMMENTS_SETTING_KEY = 'linearSessionComments';

/**
 * 解析数据目录：优先使用 OPENCHAMBER_DATA_DIR 环境变量（解析为绝对路径），
 * 否则回退到 ~/.config/openchamber。
 * @returns {string} 数据目录绝对路径
 */
function resolveDataDir() {
  const fromEnv = readEnv('OPENCHAMBER_DATA_DIR');
  if (fromEnv) {
    return path.resolve(fromEnv);
  }
  return path.join(os.homedir(), '.config', 'openchamber');
}

/** 返回 Linear 授权凭据存储文件 linear-auth.json 的绝对路径。 */
function storageFile() {
  return path.join(resolveDataDir(), 'linear-auth.json');
}

/** 返回通用设置文件 settings.json 的绝对路径。 */
function settingsFile() {
  return path.join(resolveDataDir(), 'settings.json');
}

/** 确保数据目录存在，不存在时递归创建（供写文件前调用）。 */
function ensureStorageDir() {
  const dir = resolveDataDir();
  if (!fs.existsSync(dir)) {
    fs.mkdirSync(dir, { recursive: true });
  }
}

/**
 * 读取并解析指定路径的 JSON 文件。
 * 文件不存在、内容为空、解析结果不是普通对象或解析失败时返回 null；
 * 解析异常会通过 console.error 记录但不抛出。
 * @param {string} filePath JSON 文件绝对路径
 * @returns {object | null} 解析后的普通对象，失败返回 null
 */
function readJsonFile(filePath) {
  if (!fs.existsSync(filePath)) {
    return null;
  }
  try {
    const raw = fs.readFileSync(filePath, 'utf8');
    const trimmed = raw.trim();
    if (!trimmed) {
      return null;
    }
    const parsed = JSON.parse(trimmed);
    if (!isPlainObject(parsed)) {
      return null;
    }
    return parsed;
  } catch (error) {
    console.error('Failed to read Linear auth file:', error);
    return null;
  }
}

/**
 * 以接近原子的方式写入 JSON：先写入带 pid 与时间戳后缀的临时文件并设置 0600 权限，
 * 再 rename 覆盖目标文件，避免读到写了一半的内容；权限设置失败仅尽力而为。
 * @param {string} filePath 目标 JSON 文件路径
 * @param {object} payload 待序列化写入的对象
 */
function writeJsonFile(filePath, payload) {
  ensureStorageDir();
  const tmpFile = `${filePath}.${process.pid}.${Date.now()}.tmp`;
  fs.writeFileSync(tmpFile, JSON.stringify(payload, null, 2), 'utf8');
  try {
    fs.chmodSync(tmpFile, 0o600);
  } catch {
    // best-effort
  }
  fs.renameSync(tmpFile, filePath);
  try {
    fs.chmodSync(filePath, 0o600);
  } catch {
    // best-effort
  }
}

/**
 * 将 Linear 用户信息规范为固定字段结构（id/name/displayName/email/avatarUrl）。
 * 入参不是普通对象或缺少有效 id 时返回 null；其余字段缺失时为 null。
 */
function normalizeUser(user) {
  if (!isPlainObject(user)) {
    return null;
  }
  const id = readTrimmedString(user.id);
  if (!id) {
    return null;
  }
  return {
    id,
    name: readTrimmedString(user.name) || null,
    displayName: readTrimmedString(user.displayName) || null,
    email: readTrimmedString(user.email) || null,
    avatarUrl: readTrimmedString(user.avatarUrl) || null,
  };
}

/**
 * 将 Linear 组织信息规范为固定字段结构（id/name/urlKey）。
 * 入参不是普通对象或缺少 id、name 时返回 null。
 */
function normalizeOrganization(organization) {
  if (!isPlainObject(organization)) {
    return null;
  }
  const id = readTrimmedString(organization.id);
  const name = readTrimmedString(organization.name);
  if (!id || !name) {
    return null;
  }
  return {
    id,
    name,
    urlKey: readTrimmedString(organization.urlKey) || null,
  };
}

/**
 * 确定 workspace 标识：优先使用显式传入的 workspaceId，其次组织 id，
 * 再次以 user:<userId> 标识，最终兜底为 legacy（兼容旧数据）。
 */
function resolveLinearWorkspaceId({ organization, user, workspaceId } = {}) {
  const explicit = readTrimmedString(workspaceId);
  if (explicit) return explicit;
  const organizationId = organization ? readTrimmedString(organization.id) : '';
  if (organizationId) return organizationId;
  const userId = user ? readTrimmedString(user.id) : '';
  if (userId) return `user:${userId}`;
  return LEGACY_WORKSPACE_ID;
}

/**
 * 将单条原始授权记录规范为标准结构，并推导 workspaceId。
 * 入参不是普通对象或缺少 accessToken 时返回 null；数值字段经 readFiniteNumber 归一。
 */
function normalizeAuthEntry(raw) {
  if (!isPlainObject(raw)) {
    return null;
  }
  const accessToken = readTrimmedString(raw.accessToken);
  if (!accessToken) {
    return null;
  }
  const user = normalizeUser(raw.user);
  const organization = normalizeOrganization(raw.organization);
  return {
    accessToken,
    refreshToken: readTrimmedString(raw.refreshToken) || null,
    tokenType: readTrimmedString(raw.tokenType) || 'bearer',
    expiresAt: readFiniteNumber(raw.expiresAt),
    scope: readTrimmedString(raw.scope),
    createdAt: readFiniteNumber(raw.createdAt),
    authorizedAt: readFiniteNumber(raw.authorizedAt) || readFiniteNumber(raw.createdAt),
    user,
    organization,
    current: Boolean(raw.current),
    workspaceId: resolveLinearWorkspaceId({
      organization,
      user,
      workspaceId: raw.workspaceId,
    }),
  };
}

/**
 * 规范化整个授权列表，兼容旧版单账号格式（顶层 accessToken）。
 * 按 workspaceId 去重；保证恰好存在一个 current（多余的重置、缺失时激活第一条）。
 * @param {object | null} raw 磁盘上读到的原始数据
 * @returns {{ list: object[], changed: boolean }} changed 表示相对原始数据是否发生变化（需回写磁盘）
 */
function normalizeAuthList(raw) {
  const source = Array.isArray(raw?.workspaces)
    ? raw.workspaces
    : (raw?.accessToken ? [raw] : []);
  const list = source.map((entry) => normalizeAuthEntry(entry)).filter(Boolean);

  if (!list.length) {
    return { list: [], changed: Boolean(raw && (raw.accessToken || Array.isArray(raw.workspaces))) };
  }

  let changed = Array.isArray(raw?.workspaces) === false && Boolean(raw?.accessToken);
  const seen = new Set();
  const deduped = [];
  for (const entry of list) {
    if (seen.has(entry.workspaceId)) {
      changed = true;
      continue;
    }
    seen.add(entry.workspaceId);
    deduped.push(entry);
  }

  let currentFound = false;
  deduped.forEach((entry) => {
    if (entry.current && !currentFound) {
      currentFound = true;
    } else if (entry.current && currentFound) {
      entry.current = false;
      changed = true;
    }
  });

  if (!currentFound && deduped[0]) {
    deduped[0].current = true;
    changed = true;
  }

  return { list: deduped, changed };
}

/**
 * 读取授权列表；若规范化过程中数据发生变更（去重、修正 current 等）则立即回写磁盘。
 * @returns {object[]} 规范化后的授权条目数组（可能为空）
 */
function readAuthList() {
  const data = readJsonFile(storageFile());
  if (!data) {
    return [];
  }
  const { list, changed } = normalizeAuthList(data);
  if (changed) {
    writeAuthList(list);
  }
  return list;
}

/**
 * 持久化授权列表：列表为空时直接删除存储文件，否则以 { workspaces: list } 结构写入。
 */
function writeAuthList(list) {
  if (!list.length) {
    const filePath = storageFile();
    if (fs.existsSync(filePath)) {
      fs.unlinkSync(filePath);
    }
    return;
  }
  writeJsonFile(storageFile(), { workspaces: list });
}

/** 读取 settings.json 并返回普通对象，读取失败或文件不存在时返回空对象。 */
function readSettings() {
  return readJsonFile(settingsFile()) || {};
}

/** 将设置对象整体写入 settings.json（覆盖式写入）。 */
function writeSettings(settings) {
  writeJsonFile(settingsFile(), settings);
}

/** 读取 settings.json 中指定键的字符串值（trim 后返回，缺失或非字符串时为 null）。 */
function readSettingString(key) {
  const stored = readSettings()[key];
  return readTrimmedString(stored);
}

/**
 * 返回当前激活（current）的 Linear 授权条目；无激活项时取第一条，列表为空返回 null。
 */
export function getLinearAuth() {
  const list = readAuthList();
  if (!list.length) {
    return null;
  }
  return list.find((entry) => entry.current) || list[0];
}

/**
 * 按 workspace id 查找授权条目；未提供有效 id 时退化为返回当前激活条目，找不到返回 null。
 * @param {string} workspaceId workspace 标识
 */
export function getLinearAuthByWorkspaceId(workspaceId) {
  const id = readTrimmedString(workspaceId);
  if (!id) {
    return getLinearAuth();
  }
  return readAuthList().find((entry) => entry.workspaceId === id) || null;
}

/**
 * 返回所有已授权 workspace 的公开摘要列表（id、组织名、urlKey、是否当前、用户、授权时间）。
 */
export function getLinearAuthWorkspaces() {
  return readAuthList().map((entry) => ({
    id: entry.workspaceId,
    name: entry.organization?.name || null,
    urlKey: entry.organization?.urlKey || null,
    current: Boolean(entry.current),
    user: entry.user || null,
    authorizedAt: entry.authorizedAt || entry.createdAt || null,
  }));
}

/**
 * 写入或更新某个 workspace 的 Linear 授权凭据并持久化。
 *
 * accessToken 必填（缺失时抛错）；未显式传入的字段（user、organization、refreshToken、
 * expiresAt 等）会沿用同 workspace 旧条目或当前条目的值。默认（options.activate !== false）
 * 将该 workspace 设为唯一 current；若列表中原本没有 current 也会自动激活它。
 * @param {object} input 授权字段，accessToken 必填，其余可选
 * @param {{ activate?: boolean }} options activate 为 false 时不改变现有激活状态
 * @returns {object} 写入后的授权条目
 */
export function setLinearAuth(input, options = {}) {
  const accessToken = readTrimmedString(input?.accessToken);
  if (!accessToken) {
    throw new Error('accessToken is required');
  }
  const activate = options.activate !== false;
  const list = readAuthList();
  const current = list.find((entry) => entry.current) || list[0] || null;

  const nextUser = Object.prototype.hasOwnProperty.call(input, 'user')
    ? normalizeUser(input.user)
    : current?.user || null;
  const nextOrganization = Object.prototype.hasOwnProperty.call(input, 'organization')
    ? normalizeOrganization(input.organization)
    : current?.organization || null;
  const workspaceId = resolveLinearWorkspaceId({
    organization: nextOrganization,
    user: nextUser,
    workspaceId: input?.workspaceId || (nextOrganization || nextUser ? '' : current?.workspaceId),
  });

  const existingIndex = list.findIndex((entry) => entry.workspaceId === workspaceId);
  const previous = existingIndex >= 0 ? list[existingIndex] : (
    nextOrganization || nextUser ? null : current
  );
  const targetIndex = existingIndex >= 0
    ? existingIndex
    : (previous && !nextOrganization && !nextUser ? list.indexOf(previous) : -1);
  const wasCurrent = previous?.current === true;

  const next = {
    accessToken,
    refreshToken: Object.prototype.hasOwnProperty.call(input, 'refreshToken')
      ? (readTrimmedString(input.refreshToken) || null)
      : previous?.refreshToken || null,
    tokenType: readTrimmedString(input?.tokenType) || previous?.tokenType || 'bearer',
    expiresAt: readFiniteNumber(input?.expiresAt) ?? previous?.expiresAt ?? null,
    scope: readTrimmedString(input?.scope) || previous?.scope || '',
    createdAt: previous?.createdAt || Date.now(),
    authorizedAt: Object.prototype.hasOwnProperty.call(input, 'authorizedAt')
      ? (readFiniteNumber(input.authorizedAt) || Date.now())
      : (activate ? Date.now() : (previous?.authorizedAt || previous?.createdAt || Date.now())),
    user: nextUser,
    organization: nextOrganization,
    current: false,
    workspaceId,
  };

  if (targetIndex >= 0) {
    list[targetIndex] = next;
  } else {
    list.push(next);
  }

  const writtenIndex = targetIndex >= 0 ? targetIndex : list.length - 1;
  if (activate || !list.some((entry) => entry.current)) {
    list.forEach((entry, index) => {
      entry.current = index === writtenIndex;
    });
  } else {
    list[writtenIndex].current = wasCurrent;
  }

  writeAuthList(list);
  return list[writtenIndex];
}

/**
 * 将指定 workspace 设为当前激活项（其余条目取消激活）并持久化。
 * @param {string} workspaceId workspace 标识
 * @returns {boolean} 找到并激活返回 true，id 无效或不存在返回 false
 */
export function activateLinearAuth(workspaceId) {
  const id = readTrimmedString(workspaceId);
  if (!id) {
    return false;
  }
  const list = readAuthList();
  const index = list.findIndex((entry) => entry.workspaceId === id);
  if (index === -1) {
    return false;
  }
  list.forEach((entry, idx) => {
    entry.current = idx === index;
  });
  writeAuthList(list);
  return true;
}

/**
 * 删除 Linear 授权：传入 workspaceId 时仅删除该 workspace，否则删除当前激活条目。
 * 删除后若列表清空则移除存储文件；若删掉的是 current，则将剩余第一条设为 current。
 * 任何异常都会被捕获并记录，返回 false 表示失败。
 * @param {string} [workspaceId] 待删除的 workspace 标识
 * @returns {boolean} 是否成功
 */
export function clearLinearAuth(workspaceId) {
  try {
    const list = readAuthList();
    if (!list.length) {
      return true;
    }
    const id = readTrimmedString(workspaceId);
    const remaining = id
      ? list.filter((entry) => entry.workspaceId !== id)
      : list.filter((entry) => !entry.current);
    if (!remaining.length) {
      writeAuthList([]);
      return true;
    }
    if (!remaining.some((entry) => entry.current)) {
      remaining[0].current = true;
    }
    writeAuthList(remaining);
    return true;
  } catch (error) {
    console.error('Failed to clear Linear auth file:', error);
    return false;
  }
}

/**
 * 判断 access token 是否过期（或距过期不足 2 分钟的刷新提前量）。
 * expiresAt 缺失或非法时一律视为已过期，返回 true。
 * @param {number | null} expiresAt token 过期时间戳（毫秒）
 * @param {number} [now] 当前时间戳，默认 Date.now()
 * @returns {boolean} 是否需要刷新
 */
export function isLinearAccessTokenStale(expiresAt, now = Date.now()) {
  const expiry = readFiniteNumber(expiresAt);
  if (expiry == null) {
    return true;
  }
  return expiry - ACCESS_TOKEN_REFRESH_SKEW_MS <= now;
}

/**
 * 将授权条目转换为可对外（API/前端）返回的连接状态对象：
 * 未连接时仅含 connected: false；已连接时包含用户、组织、scope 与 workspace 摘要，
 * 绝不包含 access/refresh token 等敏感字段。
 */
export function toLinearPublicStatus(auth, workspaces = getLinearAuthWorkspaces()) {
  if (!auth?.accessToken) {
    return { connected: false };
  }
  return {
    connected: true,
    user: auth.user || null,
    organization: auth.organization || null,
    scope: auth.scope || undefined,
    workspaces,
  };
}

/** 读取 Linear OAuth client id：环境变量 OPENCHAMBER_LINEAR_CLIENT_ID > settings > 内置默认值。 */
export function getLinearClientId() {
  const fromEnv = readEnv('OPENCHAMBER_LINEAR_CLIENT_ID');
  if (fromEnv) return fromEnv;
  const stored = readSettingString('linearClientId');
  if (stored) return stored;
  return DEFAULT_LINEAR_CLIENT_ID;
}

/**
 * 读取 Linear OAuth client secret：环境变量 OPENCHAMBER_LINEAR_CLIENT_SECRET > settings。
 * 无配置时返回 null（表示走 broker 中转授权，本机不持有 secret）。
 */
export function getLinearClientSecret() {
  const fromEnv = readEnv('OPENCHAMBER_LINEAR_CLIENT_SECRET');
  if (fromEnv) return fromEnv;
  return readSettingString('linearClientSecret');
}

/** 读取 OAuth scope 列表：环境变量 OPENCHAMBER_LINEAR_SCOPES > settings > 默认值。 */
export function getLinearScopes() {
  const fromEnv = readEnv('OPENCHAMBER_LINEAR_SCOPES');
  if (fromEnv) return fromEnv;
  const stored = readSettingString('linearScopes');
  if (stored) return stored;
  return DEFAULT_LINEAR_SCOPES;
}

/** 读取 OAuth broker 地址（去除尾部斜杠）：环境变量 OPENCHAMBER_LINEAR_BROKER_URL > settings > 默认值。 */
export function getLinearBrokerUrl() {
  const fromEnv = readEnv('OPENCHAMBER_LINEAR_BROKER_URL');
  if (fromEnv) return fromEnv.replace(/\/+$/, '');
  const stored = readSettingString('linearBrokerUrl');
  if (stored) return stored.replace(/\/+$/, '');
  return DEFAULT_LINEAR_BROKER_URL;
}

/** 读取 OAuth 回调地址：环境变量 OPENCHAMBER_LINEAR_REDIRECT_URI > settings > broker 地址 + /callback。 */
export function getLinearRedirectUri() {
  const fromEnv = readEnv('OPENCHAMBER_LINEAR_REDIRECT_URI');
  if (fromEnv) return fromEnv;
  const stored = readSettingString('linearRedirectUri');
  if (stored) return stored;
  return `${getLinearBrokerUrl()}/callback`;
}

/** 读取"会话状态评论"开关：仅在 settings 中显式为 true 时开启，默认关闭。 */
/**
 * Status comments are opt-in: they are written into a Linear workspace other
 * people read, so nothing is posted until the user turns them on.
 */
export function getLinearSessionCommentsEnabled() {
  return readSettings()[SESSION_COMMENTS_SETTING_KEY] === true;
}

/**
 * 设置"会话状态评论"开关并持久化到 settings.json（仅 true 视为开启）。
 * @param {boolean} enabled 是否开启
 * @returns {boolean} 持久化后的最终值
 */
export function setLinearSessionCommentsEnabled(enabled) {
  const next = enabled === true;
  const settings = readSettings();
  settings[SESSION_COMMENTS_SETTING_KEY] = next;
  writeSettings(settings);
  return next;
}

/** 返回凭据存储文件路径（供诊断与测试使用）。 */
export function getLinearAuthFilePath() {
  return storageFile();
}
/** 导出内置默认 client id 常量（供测试与外部引用）。 */
export const DEFAULT_LINEAR_CLIENT_ID_VALUE = DEFAULT_LINEAR_CLIENT_ID;
