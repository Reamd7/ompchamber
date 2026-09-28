/**
 * Google Provider - Auth
 *
 * Authentication resolution logic for Google quota providers.
 * @module quota/providers/google/auth
 */
/**
 * Google 配额提供方的鉴权来源解析：从 OpenCode auth.json 读取 Gemini CLI
 * 登录凭据、从 antigravity-accounts.json 读取 Antigravity 账号，并按
 * sourceId 选择对应的 OAuth client（gemini / antigravity 各自的公开
 * client_id + secret）。全部只读、无 I/O 副作用，凭据缺失的来源返回 null。
 */

import {
  ANTIGRAVITY_ACCOUNTS_PATHS,
  readJsonFile,
  getAuthEntry,
  normalizeAuthEntry,
  asObject,
  asNonEmptyString,
  toTimestamp
} from '../../utils/index.js';
import { readAuthFile } from '../../../opencode/auth.js';
import { parseGoogleRefreshToken } from './transforms.js';

/** Antigravity 客户端公开的 Google OAuth client_id（与官方扩展一致）。 */
const ANTIGRAVITY_GOOGLE_CLIENT_ID =
  '1071006060591-tmhssin2h21lcre235vtolojh4g403ep.apps.googleusercontent.com';
/** Antigravity 客户端公开的 Google OAuth client_secret。 */
const ANTIGRAVITY_GOOGLE_CLIENT_SECRET = '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';
/** Gemini CLI 公开的 Google OAuth client_id。 */
const GEMINI_GOOGLE_CLIENT_ID =
  '681255809395-oo8ft2oprdrnp9e3aqf6av3hmdib135j.apps.googleusercontent.com';
/** Gemini CLI 公开的 Google OAuth client_secret。 */
const GEMINI_GOOGLE_CLIENT_SECRET = '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';
/** API 请求未携带项目时的默认 Google Cloud 项目 ID。 */
export const DEFAULT_PROJECT_ID = 'rising-fact-p41fc';

/**
 * 按来源返回 OAuth client 凭据对：gemini 来源用 Gemini CLI 的 client，
 * 其余（antigravity）用 Antigravity 的 client。两者决定了 refresh token
 * 换 access token 时使用的应用身份。
 * @param {string} sourceId 来源标识
 * @returns {{ clientId: string, clientSecret: string }}
 */
export const resolveGoogleOAuthClient = (sourceId) => {
  if (sourceId === 'gemini') {
    return {
      clientId: GEMINI_GOOGLE_CLIENT_ID,
      clientSecret: GEMINI_GOOGLE_CLIENT_SECRET
    };
  }

  return {
    clientId: ANTIGRAVITY_GOOGLE_CLIENT_ID,
    clientSecret: ANTIGRAVITY_GOOGLE_CLIENT_SECRET
  };
};

/**
 * 从 OpenCode auth.json（别名 google / google.oauth）解析 Gemini CLI 登录。
 * 兼容两种形态：平铺（access/refresh）与嵌套 `oauth` 对象（token/refresh）；
 * refresh token 内可携带 projectId。access 与 refresh 均缺失时返回 null。
 * @param {object} auth readAuthFile() 的结果
 * @returns {{ sourceId: 'gemini', sourceLabel: string, accessToken, refreshToken, projectId, expires } | null}
 */
const resolveGeminiCliAuth = (auth) => {
  const entry = normalizeAuthEntry(getAuthEntry(auth, ['google', 'google.oauth']));
  const entryObject = asObject(entry);
  if (!entryObject) {
    return null;
  }

  const oauthObject = asObject(entryObject.oauth) ?? entryObject;
  const accessToken = asNonEmptyString(oauthObject.access) ?? asNonEmptyString(oauthObject.token);
  const refreshParts = parseGoogleRefreshToken(oauthObject.refresh);

  if (!accessToken && !refreshParts.refreshToken) {
    return null;
  }

  return {
    sourceId: 'gemini',
    sourceLabel: 'Gemini',
    accessToken,
    refreshToken: refreshParts.refreshToken,
    projectId: refreshParts.projectId ?? refreshParts.managedProjectId,
    expires: toTimestamp(oauthObject.expires)
  };
};

/**
 * 读取 antigravity-accounts.json（config 与 data 目录两处候选路径）中当前
 * 激活账号的 refresh token；文件缺失、无账号或无 token 时返回 null。
 * projectId 的优先级：账号字段 projectId > managedProjectId > 复合
 * refresh token 中携带的项目。
 * @returns {{ sourceId: 'antigravity', sourceLabel: string, refreshToken, projectId, email } | null}
 */
const resolveAntigravityAuth = () => {
  for (const filePath of ANTIGRAVITY_ACCOUNTS_PATHS) {
    const data = readJsonFile(filePath);
    const accounts = data?.accounts;
    if (Array.isArray(accounts) && accounts.length > 0) {
      const index = typeof data.activeIndex === 'number' ? data.activeIndex : 0;
      const account = accounts[index] ?? accounts[0];
      if (account?.refreshToken) {
        const refreshParts = parseGoogleRefreshToken(account.refreshToken);
        return {
          sourceId: 'antigravity',
          sourceLabel: 'Antigravity',
          refreshToken: refreshParts.refreshToken,
          projectId: asNonEmptyString(account.projectId)
            ?? asNonEmptyString(account.managedProjectId)
            ?? refreshParts.projectId
            ?? refreshParts.managedProjectId,
          email: account.email
        };
      }
    }
  }

  return null;
};

/**
 * 汇总所有可用的 Google 鉴权来源：先 Gemini CLI（OpenCode auth.json），
 * 再 Antigravity（antigravity-accounts.json）。两个来源可并存，配额抓取
 * 会对每个来源各跑一遍并合并模型结果。
 * @returns {Array<object>} 可用的来源列表（可能为空）
 */
export const resolveGoogleAuthSources = () => {
  const auth = readAuthFile();
  const sources = [];

  const geminiAuth = resolveGeminiCliAuth(auth);
  if (geminiAuth) {
    sources.push(geminiAuth);
  }

  const antigravityAuth = resolveAntigravityAuth();
  if (antigravityAuth) {
    sources.push(antigravityAuth);
  }

  return sources;
};
