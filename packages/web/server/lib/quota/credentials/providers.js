/**
 * 受管 quota 凭据的规范化与读写门面。
 *
 * 在 store.js 的原子存储之上叠加每个 provider 的字段清洗、写入前校验、
 * 以及永不回显明文的配置状态摘要；被 quota 路由和 cursor provider 共用。
 */

import { deleteQuotaCredential, readQuotaCredential, writeQuotaCredential } from './store.js';

/** 清洗单个凭据字符串：仅接受不含换行的字符串并 trim，其余一律返回空串（拒绝注入）。 */
const clean = (value) => typeof value === 'string' && !/[\r\n]/.test(value) ? value.trim() : '';

/**
 * 各 provider 的「请求体 → 凭据对象」规范化器映射；返回 null 表示输入无效。
 * 键集同时决定了哪些 provider 支持受管凭据。
 */
export const normalizers = {
  /** ollama-cloud：仅提取非空 cookie 字段。 */
  'ollama-cloud': (value) => {
    const cookie = clean(value?.cookie);
    return cookie ? { cookie } : null;
  },
  /** cursor：提取 accessToken/refreshToken，任一非空即有效（缺失项以空串占位）。 */
  cursor: (value) => {
    const accessToken = clean(value?.accessToken);
    const refreshToken = clean(value?.refreshToken);
    return accessToken || refreshToken ? { accessToken, refreshToken } : null;
  },
};

/**
 * 读取并规范化指定 provider 的受管凭据；provider 未知或尚未配置时返回 null。
 */
export const readManagedCredential = (providerId) => {
  const normalize = normalizers[providerId];
  return normalize ? readQuotaCredential(providerId, normalize) : null;
};

/**
 * 规范化输入并原子写入受管凭据；输入无效抛 `Invalid credential`，
 * 成功后返回脱敏的状态摘要（不回显凭据内容）。
 */
export const writeManagedCredential = (providerId, value) => {
  const credential = normalizers[providerId]?.(value);
  if (!credential) throw new Error('Invalid credential');
  writeQuotaCredential(providerId, credential);
  return getManagedCredentialStatus(providerId);
};

/**
 * 返回 provider 的配置状态摘要：是否已配置（configured）、cursor 额外
 * 提供 hasRefreshToken，secretMasked 恒为掩码，任何路径都不会泄露明文。
 */
export const getManagedCredentialStatus = (providerId) => {
  const credential = readManagedCredential(providerId);
  if (!credential) return { configured: false };
  if (providerId === 'cursor') return { configured: true, hasRefreshToken: Boolean(credential.refreshToken), secretMasked: '••••••••' };
  return { configured: true, secretMasked: '••••••••' };
};

/** 删除指定 provider 的受管凭据（透传存储层，ENOENT 静默）。 */
export const deleteManagedCredential = (providerId) => deleteQuotaCredential(providerId);
