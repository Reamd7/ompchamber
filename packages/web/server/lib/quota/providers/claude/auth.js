/**
 * Claude credential discovery.
 *
 * Claude Code is the primary source: on macOS it keeps its OAuth tokens in the
 * login Keychain, elsewhere in a credentials file. OpenCode's own `auth.json`
 * entry is the fallback for users who signed into Anthropic through OpenCode
 * instead of Claude Code.
 *
 * Every source is read-only. Claude rotates a Keychain/credentials entry from
 * under us whenever Claude Code refreshes, so credentials are read fresh per
 * request rather than cached; a stale cached token would outlive the record it
 * came from.
 *
 * @module quota/providers/claude/auth
 */
/**
 * Claude 凭据发现（credential discovery）。
 *
 * 凭据来源按优先级排列：macOS 登录 Keychain（仅 darwin）、Claude Code 的
 * `.credentials.json` 文件、OpenCode 自身 `auth.json` 中的 anthropic/claude
 * 条目，最后是 `CLAUDE_CODE_OAUTH_TOKEN` 环境变量。所有来源均为只读；
 * 由于 Claude Code 刷新 token 时会原地轮换 Keychain/文件条目，凭据每次
 * 请求都重新读取、不做缓存——缓存的旧 token 会活得比它的来源记录更久。
 */

import { execFileSync } from 'child_process';
import os from 'os';
import path from 'path';

import { readAuthFile } from '../../../opencode/auth.js';
import { asObject, asNonEmptyString, normalizeTimestamp, getAuthEntry, normalizeAuthEntry, readJsonFile } from '../../utils/index.js';

/** macOS 登录 Keychain 中 Claude Code OAuth 凭据的 service 名称。 */
const KEYCHAIN_SERVICE = 'Claude Code-credentials';
/** OpenCode auth.json 中可承载 Anthropic 凭据的条目别名（按序匹配）。 */
const OPENCODE_AUTH_ALIASES = ['anthropic', 'claude'];

/**
 * ClaudeCredential 类型定义的中文补充说明：从任一来源读出的 OAuth 凭据。
 * `accessToken` 为空即视为该来源无凭据（返回 null，尝试下一个来源）。
 */
/**
 * @typedef {object} ClaudeCredential
 * @property {string} accessToken
 * @property {string|null} refreshToken
 * @property {number|null} expiresAt Epoch milliseconds, when the source reports it.
 * @property {string|null} planLabel Subscription tier reported by Claude Code, e.g. `max`.
 * @property {'keychain'|'credentials-file'|'opencode-auth'|'env'} source
 */

/**
 * Claude Code 配置目录：`CLAUDE_CONFIG_DIR` 环境变量优先，否则用
 * `~/.claude`。
 * @returns {string} 配置目录绝对路径
 */
const claudeConfigDirectory = () => {
  const override = asNonEmptyString(process.env.CLAUDE_CONFIG_DIR);
  return override ? path.resolve(override) : path.join(os.homedir(), '.claude');
};

/**
 * 从 Claude Code 写入的凭据 JSON 中提取 OAuth 条目；缺失 accessToken
 * （例如 blob 里只有无关的 MCP token）时返回 null，由调用方降级到下一来源。
 */
/**
 * Claude Code writes one JSON blob holding both its own OAuth tokens
 * (`claudeAiOauth`) and unrelated MCP server tokens. Only the former is read.
 */
const parseClaudeCodeBlob = (blob, source) => {
  const oauth = asObject(asObject(blob)?.claudeAiOauth);
  const accessToken = asNonEmptyString(oauth?.accessToken);
  if (!accessToken) return null;
  return {
    accessToken,
    refreshToken: asNonEmptyString(oauth.refreshToken),
    expiresAt: normalizeTimestamp(oauth.expiresAt),
    planLabel: asNonEmptyString(oauth.subscriptionType),
    source
  };
};

/**
 * macOS 专用：用 `security find-generic-password -w` 同步读取 Keychain 中
 * 的 Claude Code 凭据 blob。非 darwin 平台、条目不存在、用户拒绝访问或
 * 内容不是合法 JSON 时都返回 null（10 秒超时防止子进程卡死）。
 */
const readKeychainCredential = () => {
  if (process.platform !== 'darwin') return null;
  let raw;
  try {
    raw = execFileSync('security', ['find-generic-password', '-s', KEYCHAIN_SERVICE, '-w'], {
      encoding: 'utf8',
      timeout: 10_000,
      stdio: ['ignore', 'pipe', 'ignore']
    });
  } catch {
    // No entry, or the user denied Keychain access. Both mean "try the next source".
    return null;
  }
  try {
    return parseClaudeCodeBlob(JSON.parse(raw.trim()), 'keychain');
  } catch {
    console.warn('Claude quota: Keychain credentials are not valid JSON');
    return null;
  }
};

/**
 * 读取 `~/.claude/.credentials.json`（受 `CLAUDE_CONFIG_DIR` 影响）中的
 * Claude Code 凭据；文件缺失或无 OAuth 条目时经由 parseClaudeCodeBlob
 * 返回 null。
 */
const readCredentialsFile = () =>
  parseClaudeCodeBlob(readJsonFile(path.join(claudeConfigDirectory(), '.credentials.json')), 'credentials-file');

/**
 * 第三优先级：从 OpenCode 的 auth.json（别名 anthropic/claude）读取通过
 * OpenCode 登录 Anthropic 得到的凭据。access/token 与 refresh/expires
 * 字段名都兼容；该来源不提供订阅套餐信息（planLabel 为 null）。
 */
const readOpenCodeCredential = () => {
  const entry = normalizeAuthEntry(getAuthEntry(readAuthFile(), OPENCODE_AUTH_ALIASES));
  const accessToken = asNonEmptyString(entry?.access) ?? asNonEmptyString(entry?.token);
  if (!accessToken) return null;
  return {
    accessToken,
    refreshToken: asNonEmptyString(entry.refresh),
    expiresAt: normalizeTimestamp(entry.expires),
    planLabel: null,
    source: 'opencode-auth'
  };
};

/**
 * 最低优先级：读取 `CLAUDE_CODE_OAUTH_TOKEN` 环境变量中的裸 access token；
 * 没有配套 refresh token 与过期时间。
 */
const readEnvCredential = () => {
  const accessToken = asNonEmptyString(process.env.CLAUDE_CODE_OAUTH_TOKEN);
  if (!accessToken) return null;
  return { accessToken, refreshToken: null, expiresAt: null, planLabel: null, source: 'env' };
};

/**
 * 按优先级返回第一个能产出凭据的来源：Keychain > credentials 文件 >
 * OpenCode auth.json > 环境变量。macOS 上 Keychain 优先是因为文件是
 * Claude Code 已不再更新的残留，内容可能过期。
 */
/**
 * First credential a source can produce, in priority order.
 *
 * The Keychain wins over the credentials file because on macOS the file is a
 * leftover that Claude Code no longer updates.
 *
 * @returns {ClaudeCredential|null}
 */
export const loadClaudeCredential = () =>
  readKeychainCredential()
  ?? readCredentialsFile()
  ?? readOpenCodeCredential()
  ?? readEnvCredential();
