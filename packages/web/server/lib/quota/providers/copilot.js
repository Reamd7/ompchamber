/**
 * GitHub Copilot 配额 provider。
 *
 * 通过 GitHub 内部接口 /copilot_internal/user 读取订阅的配额快照
 * （premium interactions 等），配额语义与 VSCode Copilot Chat 扩展一致。
 * 同一份凭据同时服务主订阅与 add-on 附加订阅两个 provider 条目。
 */
import { readAuthFile } from '../../opencode/auth.js';
import {
  getAuthEntry,
  normalizeAuthEntry,
  buildResult,
  toUsageWindow,
  toNumber,
  toTimestamp
} from '../utils/index.js';

/**
 * 把 Copilot 配额响应解析为用量窗口表。
 * 目前只暴露 premium_interactions 一个窗口；每个快照的已用百分比优先由
 * entitlement/remaining 计算，缺失时退回服务端算好的 percent_remaining。
 * @param {object} payload /copilot_internal/user 的 JSON 响应
 * @returns {Object<string, object>} 窗口标签 -> toUsageWindow 结果
 */
const buildCopilotWindows = (payload) => {
  const quota = payload?.quota_snapshots ?? {};
  const resetAt = toTimestamp(payload?.quota_reset_date);
  const windows = {};

  /** 把单个配额快照换算成窗口写入 windows；unlimited 套餐输出 "Unlimited" 文案且不计百分比。 */
  // Mirrors the quota semantics of microsoft/vscode-copilot-chat
  // (CopilotUserQuotaInfo): each snapshot carries entitlement, remaining,
  // unlimited, and percent_remaining. Unlimited plans report no usable
  // entitlement; percent_remaining is a server-computed fallback.
  const addWindow = (label, snapshot) => {
    if (!snapshot) return;

    if (snapshot.unlimited === true) {
      windows[label] = toUsageWindow({
        usedPercent: null,
        windowSeconds: null,
        resetAt,
        valueLabel: 'Unlimited'
      });
      return;
    }

    const entitlement = toNumber(snapshot.entitlement);
    const remaining = toNumber(snapshot.remaining);
    let usedPercent = entitlement !== null && entitlement > 0 && remaining !== null
      ? Math.min(100, Math.max(0, 100 - (remaining / entitlement) * 100))
      : null;
    if (usedPercent === null) {
      const percentRemaining = toNumber(snapshot.percent_remaining);
      if (percentRemaining !== null) {
        usedPercent = Math.min(100, Math.max(0, 100 - percentRemaining));
      }
    }
    const valueLabel = entitlement !== null && entitlement > 0 && remaining !== null
      ? `${remaining.toFixed(0)} / ${entitlement.toFixed(0)} left`
      : null;
    windows[label] = toUsageWindow({
      usedPercent,
      windowSeconds: null,
      resetAt,
      valueLabel
    });
  };

  addWindow('premium_interactions', quota.premium_interactions);

  return windows;
};

/** 对外 provider 标识（主订阅）。 */
export const providerId = 'github-copilot';
/** 展示名（主订阅）。 */
export const providerName = 'GitHub Copilot';
/** OpenCode auth 文件中的凭据匹配别名。 */
const aliases = ['github-copilot', 'copilot'];

/** 读取 auth 文件，存在 OAuth access token 或 API token 即视为已配置。 */
export const isConfigured = () => {
  const auth = readAuthFile();
  const entry = normalizeAuthEntry(getAuthEntry(auth, aliases));
  return Boolean(entry?.access || entry?.token);
};

/**
 * 拉取主订阅（github-copilot）的配额：请求需模拟 VSCode 编辑器头的
 * 内部接口；未配置凭据返回 configured:false，非 2xx 或抛错时返回携带
 * 错误信息的 ok:false 结果，成功时返回 premium_interactions 窗口。
 */
export const fetchQuota = async () => {
  const auth = readAuthFile();
  const entry = normalizeAuthEntry(getAuthEntry(auth, aliases));
  const accessToken = entry?.access ?? entry?.token;

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
    const response = await fetch('https://api.github.com/copilot_internal/user', {
      method: 'GET',
      headers: {
        Authorization: `token ${accessToken}`,
        Accept: 'application/json',
        'Editor-Version': 'vscode/1.96.2',
        'X-Github-Api-Version': '2025-04-01'
      }
    });

    if (!response.ok) {
      return buildResult({
        providerId,
        providerName,
        ok: false,
        configured: true,
        error: `API error: ${response.status}`
      });
    }

    const payload = await response.json();
    return buildResult({
      providerId,
      providerName,
      ok: true,
      configured: true,
      usage: { windows: buildCopilotWindows(payload) }
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

/** 对外 provider 标识（add-on 附加订阅）。 */
export const providerIdAddon = 'github-copilot-addon';
/** 展示名（add-on 附加订阅）。 */
export const providerNameAddon = 'GitHub Copilot Add-on';

/**
 * 拉取 add-on 附加订阅（github-copilot-addon）的配额。
 * 与主订阅共用同一凭据和同一 API 端点，仅 providerId/providerName 不同，
 * 便于注册表把两者作为独立条目分别展示。
 */
export const fetchQuotaAddon = async () => {
  const auth = readAuthFile();
  const entry = normalizeAuthEntry(getAuthEntry(auth, aliases));
  const accessToken = entry?.access ?? entry?.token;

  if (!accessToken) {
    return buildResult({
      providerId: providerIdAddon,
      providerName: providerNameAddon,
      ok: false,
      configured: false,
      error: 'Not configured'
    });
  }

  try {
    const response = await fetch('https://api.github.com/copilot_internal/user', {
      method: 'GET',
      headers: {
        Authorization: `token ${accessToken}`,
        Accept: 'application/json',
        'Editor-Version': 'vscode/1.96.2',
        'X-Github-Api-Version': '2025-04-01'
      }
    });

    if (!response.ok) {
      return buildResult({
        providerId: providerIdAddon,
        providerName: providerNameAddon,
        ok: false,
        configured: true,
        error: `API error: ${response.status}`
      });
    }

    const payload = await response.json();
    return buildResult({
      providerId: providerIdAddon,
      providerName: providerNameAddon,
      ok: true,
      configured: true,
      usage: { windows: buildCopilotWindows(payload) }
    });
  } catch (error) {
    return buildResult({
      providerId: providerIdAddon,
      providerName: providerNameAddon,
      ok: false,
      configured: true,
      error: error instanceof Error ? error.message : 'Request failed'
    });
  }
};
