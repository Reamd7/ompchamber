/**
 * GitHub 账号认证的本地存储模块。
 *
 * 把 OAuth/device-flow 得到的 access token 以账号列表形式持久化到
 * OMPCHAMBER_DATA_DIR 下的 github-auth.json（0600 权限、原子写入），
 * 支持多账号与当前账号切换；gh CLI 凭据的启用/禁用与激活开关存放在
 * 同目录的 settings.json。对上层导出当前凭证读写、账号列表以及
 * gh CLI 偏好的全部接口。
 */
import fs from 'fs';
import path from 'path';
import os from 'os';

/** 数据目录：优先读 OMPCHAMBER_DATA_DIR 环境变量，否则用 ~/.config/ompchamber。 */
const OMPCHAMBER_DATA_DIR = process.env.OMPCHAMBER_DATA_DIR
  ? path.resolve(process.env.OMPCHAMBER_DATA_DIR)
  : path.join(os.homedir(), '.config', 'ompchamber');

/** 认证文件的存储目录（OMPCHAMBER_DATA_DIR 的别名，保留历史命名）。 */
const STORAGE_DIR = OMPCHAMBER_DATA_DIR;
/** GitHub 认证账号列表的持久化文件路径（github-auth.json）。 */
const STORAGE_FILE = path.join(STORAGE_DIR, 'github-auth.json');
/** 通用设置文件路径（settings.json），存 gh CLI 开关与自定义 client id/scopes。 */
const SETTINGS_FILE = path.join(OMPCHAMBER_DATA_DIR, 'settings.json');

/** 未配置任何覆盖时兜底使用的 GitHub OAuth App client id。 */
const DEFAULT_GITHUB_CLIENT_ID = 'Ov23lizomPOC3eFYo56r';
/** 默认申请的 OAuth scopes：仓库、org、workflow、用户资料与邮箱。 */
const DEFAULT_GITHUB_SCOPES = 'repo read:org workflow read:user user:email';
/** gh CLI 凭据在账号列表中使用的固定账号 id。 */
export const GH_CLI_ACCOUNT_ID = 'gh-cli';

/** 确保存储目录存在，不存在则递归创建（所有写文件前的统一前置）。 */
function ensureStorageDir() {
  if (!fs.existsSync(STORAGE_DIR)) {
    fs.mkdirSync(STORAGE_DIR, { recursive: true });
  }
}

/**
 * 读取并解析 github-auth.json；文件不存在、内容为空、JSON 非法或不是
 * 对象时统一返回 null，解析失败只记日志、绝不抛出。
 */
function readJsonFile() {
  ensureStorageDir();
  if (!fs.existsSync(STORAGE_FILE)) {
    return null;
  }
  try {
    const raw = fs.readFileSync(STORAGE_FILE, 'utf8');
    const trimmed = raw.trim();
    if (!trimmed) {
      return null;
    }
    const parsed = JSON.parse(trimmed);
    if (!parsed || typeof parsed !== 'object') {
      return null;
    }
    return parsed;
  } catch (error) {
    console.error('Failed to read GitHub auth file:', error);
    return null;
  }
}

/**
 * 原子写入 github-auth.json：先写带 pid 与时间戳的临时文件并 chmod 0600，
 * 再 rename 覆盖目标，保证多个 OMPChamber 实例共享同一文件时不会读到
 * 半截 JSON；chmod 失败按尽力而为忽略。
 */
function writeJsonFile(payload) {
  ensureStorageDir();

  // Atomic write so multiple OMPChamber instances can safely share the same file.
  const tmpFile = `${STORAGE_FILE}.${process.pid}.${Date.now()}.tmp`;
  fs.writeFileSync(tmpFile, JSON.stringify(payload, null, 2), 'utf8');
  try {
    fs.chmodSync(tmpFile, 0o600);
  } catch {
    // best-effort
  }

  fs.renameSync(tmpFile, STORAGE_FILE);
  try {
    fs.chmodSync(STORAGE_FILE, 0o600);
  } catch {
    // best-effort
  }
}

/**
 * 为账号推导稳定的 accountId：显式 accountId > user.login > user.id >
 * token 前 8 位兜底。用于跨重启识别同一账号、支撑多账号切换。
 */
function resolveAccountId({ user, accessToken, accountId }) {
  if (typeof accountId === 'string' && accountId.trim()) {
    return accountId.trim();
  }
  if (user && typeof user.login === 'string' && user.login.trim()) {
    return user.login.trim();
  }
  if (user && typeof user.id === 'number') {
    return String(user.id);
  }
  if (typeof accessToken === 'string' && accessToken.trim()) {
    return `token:${accessToken.slice(0, 8)}`;
  }
  return '';
}

/**
 * 把磁盘上的单条账号记录规整为安全结构：无 accessToken 直接判废返回
 * null；user 各字段逐项校验类型；tokenType 缺省补 'bearer'，并补齐
 * accountId。
 */
function normalizeAuthEntry(entry) {
  if (!entry || typeof entry !== 'object') return null;
  const accessToken = typeof entry.accessToken === 'string' ? entry.accessToken : '';
  if (!accessToken) return null;
  const user = entry.user && typeof entry.user === 'object'
    ? {
      login: typeof entry.user.login === 'string' ? entry.user.login : null,
      avatarUrl: typeof entry.user.avatarUrl === 'string' ? entry.user.avatarUrl : null,
      id: typeof entry.user.id === 'number' ? entry.user.id : null,
      name: typeof entry.user.name === 'string' ? entry.user.name : null,
      email: typeof entry.user.email === 'string' ? entry.user.email : null,
    }
    : null;

  const accountId = resolveAccountId({
    user,
    accessToken,
    accountId: typeof entry.accountId === 'string' ? entry.accountId : '',
  });

  return {
    accessToken,
    scope: typeof entry.scope === 'string' ? entry.scope : '',
    tokenType: typeof entry.tokenType === 'string' ? entry.tokenType : 'bearer',
    createdAt: typeof entry.createdAt === 'number' ? entry.createdAt : null,
    user,
    current: Boolean(entry.current),
    accountId,
  };
}

/**
 * 规整整份账号列表：兼容单对象与数组两种历史格式，剔除无效条目，
 * 保证恰好一个 current 账号（重复的清掉、全无则选首个），并为缺失
 * accountId 的旧数据补 id；changed 标记内容是否被迁移、需要回写磁盘。
 */
function normalizeAuthList(raw) {
  const list = (Array.isArray(raw) ? raw : [raw])
    .map((entry) => normalizeAuthEntry(entry))
    .filter(Boolean);

  if (!list.length) {
    return { list: [], changed: false };
  }

  let changed = false;
  let currentFound = false;
  list.forEach((entry) => {
    if (entry.current && !currentFound) {
      currentFound = true;
    } else if (entry.current && currentFound) {
      entry.current = false;
      changed = true;
    }
  });

  if (!currentFound && list[0]) {
    list[0].current = true;
    changed = true;
  }

  list.forEach((entry) => {
    if (!entry.accountId) {
      entry.accountId = resolveAccountId(entry);
      changed = true;
    }
  });

  return { list, changed };
}

/** 读取账号列表；若规整过程改变了内容则顺手回写（惰性迁移旧格式）。 */
function readAuthList() {
  const data = readJsonFile();
  if (!data) {
    return [];
  }
  const { list, changed } = normalizeAuthList(data);
  if (changed) {
    writeJsonFile(list);
  }
  return list;
}

/** 把账号列表整体写回 github-auth.json（内部走原子写入）。 */
function writeAuthList(list) {
  writeJsonFile(list);
}

/**
 * 读取 settings.json；文件不存在或解析失败一律返回空对象，
 * 调用方拿到的结果一定可以安全取属性。
 */
function readSettingsFile() {
  try {
    if (fs.existsSync(SETTINGS_FILE)) {
      return JSON.parse(fs.readFileSync(SETTINGS_FILE, 'utf8')) || {};
    }
  } catch {
    // ignore
  }
  return {};
}

/**
 * 原子写入 settings.json（临时文件 + rename），并尽力 chmod 0600，
 * 与认证文件同样的多实例安全策略。
 */
function writeSettingsFile(settings) {
  ensureStorageDir();
  const tmpFile = `${SETTINGS_FILE}.${process.pid}.${Date.now()}.tmp`;
  fs.writeFileSync(tmpFile, JSON.stringify(settings, null, 2), 'utf8');
  try {
    fs.chmodSync(tmpFile, 0o600);
  } catch {
    // best-effort
  }
  fs.renameSync(tmpFile, SETTINGS_FILE);
  try {
    fs.chmodSync(SETTINGS_FILE, 0o600);
  } catch {
    // best-effort
  }
}

/**
 * 返回当前账号的认证条目（含 accessToken、user、scope 等）；
 * 无任何账号或当前条目缺 token 时返回 null。
 */
export function getGitHubAuth() {
  const list = readAuthList();
  if (!list.length) {
    return null;
  }
  const current = list.find((entry) => entry.current) || list[0];
  if (!current?.accessToken) {
    return null;
  }
  return current;
}

/**
 * 列出全部可用账号（仅保留带 user 与 accountId 的条目），映射为
 * { id, user, scope, current } 供账号切换 UI 使用，不暴露 token。
 */
export function getGitHubAuthAccounts() {
  const list = readAuthList();
  return list
    .filter((entry) => entry?.user && entry.accountId)
    .map((entry) => ({
      id: entry.accountId,
      user: entry.user,
      scope: entry.scope || '',
      current: Boolean(entry.current),
    }));
}

/**
 * 新增或更新一个账号并设为 current：按 accountId 匹配已有条目则原位
 * 替换，否则追加到末尾；随后重算所有条目的 current 标记并落盘。
 * accessToken 缺失时直接抛错。返回写入的条目。
 */
export function setGitHubAuth({ accessToken, scope, tokenType, user, accountId }) {
  if (!accessToken || typeof accessToken !== 'string') {
    throw new Error('accessToken is required');
  }
  const normalizedUser = user && typeof user === 'object'
    ? {
      login: typeof user.login === 'string' ? user.login : undefined,
      avatarUrl: typeof user.avatarUrl === 'string' ? user.avatarUrl : undefined,
      id: typeof user.id === 'number' ? user.id : undefined,
      name: typeof user.name === 'string' ? user.name : undefined,
      email: typeof user.email === 'string' ? user.email : undefined,
    }
    : undefined;

  const resolvedAccountId = resolveAccountId({
    user: normalizedUser,
    accessToken,
    accountId,
  });

  const list = readAuthList();
  const existingIndex = list.findIndex((entry) => entry.accountId === resolvedAccountId);
  const nextEntry = {
    accessToken,
    scope: typeof scope === 'string' ? scope : '',
    tokenType: typeof tokenType === 'string' ? tokenType : 'bearer',
    createdAt: Date.now(),
    user: normalizedUser || null,
    current: true,
    accountId: resolvedAccountId,
  };

  if (existingIndex >= 0) {
    list[existingIndex] = nextEntry;
  } else {
    list.push(nextEntry);
  }

  list.forEach((entry, index) => {
    entry.current = index === (existingIndex >= 0 ? existingIndex : list.length - 1);
  });
  writeAuthList(list);
  return nextEntry;
}

/**
 * 按 accountId 把某账号设为 current（其余条目置否），同时关闭 gh CLI
 * 凭据的激活态以避免双凭据来源。accountId 非法或找不到时返回 false。
 */
export function activateGitHubAuth(accountId) {
  if (typeof accountId !== 'string' || !accountId.trim()) {
    return false;
  }
  const list = readAuthList();
  const index = list.findIndex((entry) => entry.accountId === accountId.trim());
  if (index === -1) {
    return false;
  }
  setGhCliActive(false);
  list.forEach((entry, idx) => {
    entry.current = idx === index;
  });
  writeAuthList(list);
  return true;
}

/**
 * 登出当前账号：仅剩当前一个账号时直接删除整个认证文件；还有其它账号
 * 则移除当前账号并把下一个设为 current。任何步骤失败只记日志并返回
 * false，成功返回 true。
 */
export function clearGitHubAuth() {
  try {
    const list = readAuthList();
    if (!list.length) {
      return true;
    }
    const remaining = list.filter((entry) => !entry.current);
    if (!remaining.length) {
      if (fs.existsSync(STORAGE_FILE)) {
        fs.unlinkSync(STORAGE_FILE);
      }
      return true;
    }
    remaining.forEach((entry, index) => {
      entry.current = index === 0;
    });
    writeAuthList(remaining);
    return true;
  } catch (error) {
    console.error('Failed to clear GitHub auth file:', error);
    return false;
  }
}

/**
 * 解析 OAuth client id，优先级：环境变量 OMPCHAMBER_GITHUB_CLIENT_ID >
 * settings.json 的 githubClientId > 内置默认值。
 */
export function getGitHubClientId() {
  const raw = process.env.OMPCHAMBER_GITHUB_CLIENT_ID;
  const clientId = typeof raw === 'string' ? raw.trim() : '';
  if (clientId) return clientId;

  try {
    if (fs.existsSync(SETTINGS_FILE)) {
      const parsed = JSON.parse(fs.readFileSync(SETTINGS_FILE, 'utf8'));
      const stored = typeof parsed?.githubClientId === 'string' ? parsed.githubClientId.trim() : '';
      if (stored) return stored;
    }
  } catch {
    // ignore
  }

  return DEFAULT_GITHUB_CLIENT_ID;
}

/**
 * 解析 OAuth scopes，优先级：环境变量 OMPCHAMBER_GITHUB_SCOPES >
 * settings.json 的 githubScopes > 内置默认值。
 */
export function getGitHubScopes() {
  const raw = process.env.OMPCHAMBER_GITHUB_SCOPES;
  const fromEnv = typeof raw === 'string' ? raw.trim() : '';
  if (fromEnv) return fromEnv;

  try {
    if (fs.existsSync(SETTINGS_FILE)) {
      const parsed = JSON.parse(fs.readFileSync(SETTINGS_FILE, 'utf8'));
      const stored = typeof parsed?.githubScopes === 'string' ? parsed.githubScopes.trim() : '';
      if (stored) return stored;
    }
  } catch {
    // ignore
  }

  return DEFAULT_GITHUB_SCOPES;
}

/** 认证文件绝对路径的具名导出，供诊断与测试直接引用。 */
export const GITHUB_AUTH_FILE = STORAGE_FILE;

/** gh CLI 凭据是否被用户显式禁用（settings.json 的 ghCliDisabled）。 */
export function isGhCliDisabled() {
  return Boolean(readSettingsFile()?.ghCliDisabled);
}

/**
 * 设置 gh CLI 凭据的禁用开关并落盘；置为禁用时同步清除其激活态，
 * 避免"已禁用但仍激活"的不一致状态。
 */
export function setGhCliDisabled(disabled) {
  const settings = readSettingsFile();
  settings.ghCliDisabled = Boolean(disabled);
  if (settings.ghCliDisabled) {
    settings.ghCliActive = false;
  }
  writeSettingsFile(settings);
}

/** gh CLI 凭据当前是否激活：未禁用且 ghCliActive 为真才返回 true。 */
export function isGhCliActive() {
  const settings = readSettingsFile();
  return !settings?.ghCliDisabled && Boolean(settings?.ghCliActive);
}

/** 设置 gh CLI 凭据的激活态并落盘；禁用状态下强制视为未激活。 */
export function setGhCliActive(active) {
  const settings = readSettingsFile();
  settings.ghCliActive = Boolean(active) && !settings.ghCliDisabled;
  writeSettingsFile(settings);
}
