/**
 * Linear 会话状态评论模块。
 *
 * 在 Linear issue 上以评论形式同步 OpenChamber 会话的 started/completed/failure
 * 状态。通过 linear-session-status.json 去重（同一会话同一状态只发一次），
 * 且仅当会话 origin 为公网可达地址时才发布链接，避免把内网/回环地址写进团队空间。
 */
import fs from 'fs';
import path from 'path';
import { getLinearAuth, getLinearAuthFilePath, getLinearSessionCommentsEnabled } from './auth.js';
import { createLinearIssueComment } from './issues.js';
import { isPlainObject, readTrimmedString } from './parse.js';

/** 允许的会话状态类型（started/completed/failure）。 */
const LINEAR_SESSION_STATUS_KINDS = ['started', 'completed', 'failure'];
/** 去重记录文件保留的最大条目数（超出时裁掉最旧的）。 */
const MAX_SESSION_STATUS_RECORDS = 500;

/**
 * 会话状态评论流程统一错误类型：code 为 INVALID 表示调用方输入问题（路由层映射 400），
 * MALFORMED 表示本地去重记录文件损坏（映射 500）。
 */
export class LinearSessionStatusError extends Error {
  /**
   * @param {string} message 人类可读的错误信息
   * @param {string} code 机器可读错误码（INVALID 或 MALFORMED）
   */
  constructor(message, code) {
    super(message);
    this.name = 'LinearSessionStatusError';
    this.code = code;
  }
}

/** 以 "sessionId:kind" 为键的进行中发布 promise 表，用于合并并发重复请求。 */
const inflight = new Map();

/** 返回去重记录文件 linear-session-status.json 的路径（与授权文件同目录）。 */
function statusFile() {
  return path.join(path.dirname(getLinearAuthFilePath()), 'linear-session-status.json');
}

/**
 * 以接近原子的方式写入 JSON：先写带 pid/时间戳后缀的临时文件（权限 0600），
 * 再 rename 覆盖目标文件，避免读到半写内容；目录不存在时递归创建，
 * 权限设置失败仅尽力而为。
 */
function writeJsonFile(filePath, payload) {
  const dir = path.dirname(filePath);
  if (!fs.existsSync(dir)) {
    fs.mkdirSync(dir, { recursive: true });
  }
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

/** 视为内网机器名的 hostname 后缀列表（mDNS/内部域名等，均非公网可达）。 */
const PRIVATE_HOST_SUFFIXES = ['.local', '.localhost', '.internal', '.lan', '.home.arpa'];

/**
 * 判断点分十进制 IPv4 是否为私有/非公网地址：覆盖 0/8、10/8、127/8、169.254/16、
 * 172.16/12、192.168/16 与 100.64/10（运营商级 NAT，Tailscale 等组网常用）；
 * 格式不是合法 IPv4 时返回 false。
 */
function isPrivateIpv4(hostname) {
  const parts = hostname.split('.');
  if (parts.length !== 4) return false;
  const octets = parts.map((part) => (/^\d{1,3}$/.test(part) ? Number(part) : -1));
  if (octets.some((octet) => octet < 0 || octet > 255)) return false;
  const [a, b] = octets;
  if (a === 0 || a === 10 || a === 127) return true;
  if (a === 169 && b === 254) return true;
  if (a === 172 && b >= 16 && b <= 31) return true;
  if (a === 192 && b === 168) return true;
  // 100.64.0.0/10 is carrier-grade NAT, which Tailscale and similar overlays use.
  if (a === 100 && b >= 64 && b <= 127) return true;
  return false;
}

/**
 * 判断 IPv6 地址（可能带方括号）是否为非公网地址：::1 与 :: 全零地址、
 * fc00::/7（唯一本地）与 fe80::/10（链路本地）。
 */
function isPrivateIpv6(hostname) {
  const address = hostname.replace(/^\[/, '').replace(/\]$/, '').toLowerCase();
  if (address === '::1' || address === '::') return true;
  // fc00::/7 (unique local) and fe80::/10 (link local).
  return /^f[cd]/.test(address) || /^fe[89ab]/.test(address);
}

/**
 * 判断会话 origin 是否公网可达：依次排除回环/localhost、内网后缀域名、
 * 私有 IPv4/IPv6 以及单标签主机名（局域网机器名），只有其余情况才算公网。
 */
/**
 * A session link is only worth writing into Linear when somebody other than the
 * person who started the session can open it. Loopback, private LAN and
 * overlay-network addresses reach nobody else, so they do not qualify.
 */
export function isPublicSessionOrigin(value) {
  const origin = readSessionOrigin(value);
  if (!origin) return false;
  let hostname;
  try {
    hostname = new URL(origin).hostname.toLowerCase();
  } catch {
    return false;
  }
  if (!hostname || hostname === 'localhost') return false;
  if (PRIVATE_HOST_SUFFIXES.some((suffix) => hostname.endsWith(suffix))) return false;
  if (hostname.includes(':') || hostname.startsWith('[')) return !isPrivateIpv6(hostname);
  if (/^[\d.]+$/.test(hostname)) return !isPrivateIpv4(hostname);
  // A bare single-label host is a LAN machine name, not a routable address.
  return hostname.includes('.');
}

/**
 * 校验并规范化会话 origin：仅接受不带用户名密码、query、hash 与路径的
 * http/https 地址，返回其 url.origin；任何不合法情况一律返回空字符串。
 */
export function readSessionOrigin(value) {
  const trimmed = readTrimmedString(value);
  if (!trimmed) return '';
  try {
    const url = new URL(trimmed);
    if (url.protocol !== 'http:' && url.protocol !== 'https:') return '';
    if (url.username || url.password) return '';
    if (url.search || url.hash) return '';
    if (url.pathname && url.pathname !== '/') return '';
    return url.origin;
  } catch {
    return '';
  }
}

/**
 * 构造会话打开链接：<origin>/?session=<sessionId>（sessionId 经 URL 编码）；
 * origin 无效时返回空字符串。
 */
export function buildLinearSessionOpenUrl(sessionId, sessionOrigin) {
  const id = readTrimmedString(sessionId);
  const origin = readSessionOrigin(sessionOrigin);
  if (!origin) return '';
  return `${origin}/?session=${encodeURIComponent(id)}`;
}

/** 将状态 kind 映射为评论文案中的动词（未知 kind 一律按 failed 处理）。 */
function statusWord(kind) {
  if (kind === 'started') return 'started';
  if (kind === 'completed') return 'completed';
  return 'failed';
}

/**
 * 构造发布到 Linear issue 的评论正文：无 url 时为 "OpenChamber session <word>"，
 * 有 url 时输出 markdown 链接形式。
 */
export function buildLinearSessionStatusComment({ kind, sessionUrl }) {
  const url = readTrimmedString(sessionUrl);
  const label = `OpenChamber session ${statusWord(kind)}`;
  if (!url) return label;
  // The comment already lives on the issue, so it says only what happened and
  // links to the session. Issue titles routinely contain brackets ("[Bug] …"),
  // which would break this markdown link if they were repeated in the label.
  return `[${label}](${url})`;
}

/** 仅当值严格等于 true 时返回 true，其余任何值一律视为 false。 */
function readBooleanFlag(value) {
  return value === true;
}

/**
 * 将单条原始去重记录规范为标准结构：issueIdentifier 必填，sessionOrigin 经
 * readSessionOrigin 校验，三个状态位布尔严格归一；不合法时返回 null。
 */
function readRecord(value) {
  if (!isPlainObject(value)) return null;
  const issueIdentifier = readTrimmedString(value.issueIdentifier);
  if (!issueIdentifier) return null;
  return {
    issueIdentifier,
    sessionOrigin: readSessionOrigin(value.sessionOrigin) || null,
    organizationId: readTrimmedString(value.organizationId) || null,
    started: readBooleanFlag(value.started),
    completed: readBooleanFlag(value.completed),
    failure: readBooleanFlag(value.failure),
  };
}

/**
 * 读取去重记录文件并返回 { sessionId: record } 映射：文件不存在或为空返回空对象；
 * JSON 损坏或顶层不是普通对象时抛 MALFORMED 错误；逐条经 readRecord 过滤非法条目。
 */
function readRecords() {
  const filePath = statusFile();
  if (!fs.existsSync(filePath)) {
    return {};
  }
  let parsed;
  try {
    const raw = fs.readFileSync(filePath, 'utf8');
    const trimmed = raw.trim();
    if (!trimmed) {
      return {};
    }
    parsed = JSON.parse(trimmed);
  } catch {
    throw new LinearSessionStatusError('Linear session status file is malformed', 'MALFORMED');
  }
  if (!isPlainObject(parsed)) {
    throw new LinearSessionStatusError('Linear session status file is malformed', 'MALFORMED');
  }
  const next = {};
  for (const key of Object.keys(parsed)) {
    const sessionId = readTrimmedString(key);
    const record = readRecord(parsed[key]);
    if (sessionId && record) {
      next[sessionId] = record;
    }
  }
  return next;
}

/**
 * 裁剪去重记录：仅保留最新的 limit 条（按对象键的插入顺序取尾部）。
 */
/**
 * The file only exists to dedupe comments, so it does not need to remember
 * every session ever started. Keep the newest entries and drop the tail.
 */
export function pruneSessionStatusRecords(records, limit = MAX_SESSION_STATUS_RECORDS) {
  const keys = Object.keys(records);
  if (keys.length <= limit) {
    return records;
  }
  const kept = {};
  for (const key of keys.slice(keys.length - limit)) {
    kept[key] = records[key];
  }
  return kept;
}

/** 先裁剪到上限，再将去重记录原子写入本地文件。 */
function writeRecords(records) {
  writeJsonFile(statusFile(), pruneSessionStatusRecords(records));
}

/**
 * 执行一次会话状态发布（不含并发去重）：
 * 校验 kind/sessionId（非法抛 INVALID）；未连接 Linear 直接返回 connected: false；
 * 评论开关关闭、该状态已发过、会话未 started、origin 非公网可达时分别以
 * skipped 原因跳过；issueIdentifier 可沿用历史记录。通过 createLinearIssueComment
 * 发布成功后更新去重记录并返回 commentId，issue 不存在时以 issue-not-found 跳过。
 * @param {object} input 含 kind、sessionId、issueIdentifier、sessionOrigin、organizationId
 * @returns {Promise<{ connected: boolean, posted?: boolean, skipped?: string, commentId?: string }>}
 */
async function postOnce(input) {
  const kind = readTrimmedString(input?.kind);
  const sessionId = readTrimmedString(input?.sessionId);
  if (!LINEAR_SESSION_STATUS_KINDS.includes(kind) || !sessionId) {
    throw new LinearSessionStatusError('kind and sessionId are required', 'INVALID');
  }

  // Disconnected answers first so the picker and panel keep showing their
  // "connect Linear" state whatever the comment preference says.
  if (!getLinearAuth()) {
    return { connected: false };
  }
  if (!getLinearSessionCommentsEnabled()) {
    return { connected: true, posted: false, skipped: 'disabled' };
  }

  const records = readRecords();
  const existing = records[sessionId] || null;
  if (existing?.[kind] === true) {
    return { connected: true, posted: false, skipped: 'already-posted' };
  }
  if (kind !== 'started' && existing?.started !== true) {
    return { connected: true, posted: false, skipped: 'not-started' };
  }

  const issueIdentifier = readTrimmedString(input?.issueIdentifier)
    || readTrimmedString(existing?.issueIdentifier);
  if (!issueIdentifier) {
    throw new LinearSessionStatusError('issueIdentifier is required', 'INVALID');
  }

  const sessionOrigin = readSessionOrigin(input?.sessionOrigin)
    || readTrimmedString(existing?.sessionOrigin);
  // Without an origin other people can reach, the comment would carry a link
  // only its author could open. Say nothing rather than publish a dead link.
  if (!isPublicSessionOrigin(sessionOrigin)) {
    return { connected: true, posted: false, skipped: 'origin-not-public' };
  }
  const sessionUrl = buildLinearSessionOpenUrl(sessionId, sessionOrigin);
  const organizationId = readTrimmedString(input?.organizationId)
    || readTrimmedString(existing?.organizationId)
    || readTrimmedString(getLinearAuth()?.workspaceId);
  const body = buildLinearSessionStatusComment({ kind, sessionUrl });
  const commentResult = await createLinearIssueComment({
    issueId: issueIdentifier,
    body,
    organizationId,
  });
  if (commentResult.connected === false) {
    return { connected: false };
  }
  if (!commentResult.comment) {
    return { connected: true, posted: false, skipped: 'issue-not-found' };
  }

  records[sessionId] = {
    issueIdentifier,
    sessionOrigin: sessionOrigin || null,
    organizationId: organizationId || null,
    started: existing?.started === true || kind === 'started',
    completed: existing?.completed === true || kind === 'completed',
    failure: existing?.failure === true || kind === 'failure',
  };
  writeRecords(records);
  return {
    connected: true,
    posted: true,
    commentId: commentResult.comment.id,
  };
}

/**
 * 发布会话状态评论的对外入口：以 "sessionId:kind" 为键合并并发的重复请求
 * （同一键复用同一个进行中的 promise，结束后从表中移除），实际逻辑委托 postOnce。
 * @param {object} input 同 postOnce
 * @returns {Promise<object>} 发布结果
 */
export async function postLinearSessionStatus(input) {
  const kind = readTrimmedString(input?.kind);
  const sessionId = readTrimmedString(input?.sessionId);
  const key = `${sessionId}:${kind}`;
  const pending = inflight.get(key);
  if (pending) {
    return pending;
  }
  const promise = postOnce(input).finally(() => {
    inflight.delete(key);
  });
  inflight.set(key, promise);
  return promise;
}
