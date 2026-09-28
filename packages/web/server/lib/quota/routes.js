/**
 * quota HTTP 路由：配额用量查询与受管凭据管理。
 *
 * 提供 `/api/quota/providers`（列出已配置 provider）、`/api/quota/:providerId`
 * （拉取用量），以及 ollama-cloud/cursor 受管凭据的状态查询、写入、
 * 校验、导入与删除。凭据写入前都会先实测校验，避免把无效密钥落盘。
 */

import express from 'express';
import { deleteManagedCredential, getManagedCredentialStatus, normalizers, readManagedCredential, writeManagedCredential } from './credentials/providers.js';
import { fetchOllamaCloudUsage } from './providers/ollama-cloud.js';
import { importCursorCredential, validateCursorCredential } from './providers/cursor.js';

/** providerId → 凭据校验函数映射：ollama-cloud 直接拉一次用量接口验证 cookie，cursor 验证 token 可用性。 */
const validators = {
  'ollama-cloud': fetchOllamaCloudUsage,
  cursor: validateCursorCredential,
};

/**
 * 校验路径参数 :providerId 是否为受支持的凭据 provider。
 * 不支持时直接回 404 UNSUPPORTED_PROVIDER 并返回 null，调用方据此终止；
 * 支持则原样返回 providerId。
 */
const getProvider = (req, res) => {
  const providerId = req.params.providerId;
  if (!normalizers[providerId]) {
    res.status(404).json({ code: 'UNSUPPORTED_PROVIDER', error: 'Unsupported credential provider' });
    return null;
  }
  return providerId;
};

/** 统一的凭据校验失败响应：400 INVALID_CREDENTIAL + 错误消息（兜底文案为通用失败）。 */
const credentialError = (res, error) => res.status(400).json({
  code: 'INVALID_CREDENTIAL',
  error: error instanceof Error ? error.message : 'Credential validation failed',
});

/**
 * 在 express app 上注册全部 `/api/quota*` 路由。
 *
 * @param getQuotaProviders 惰性加载 provider 模块的工厂（返回
 *   listConfiguredQuotaProviders / fetchQuotaForProvider），避免路由注册
 *   阶段就触发重量级 import。
 */
export function registerQuotaRoutes(app, { getQuotaProviders }) {
  // 列出当前已配置（有凭据/认证）的 quota provider。
  app.get('/api/quota/providers', async (_req, res) => {
    try {
      const { listConfiguredQuotaProviders } = await getQuotaProviders();
      res.json({ providers: listConfiguredQuotaProviders() });
    } catch (error) {
      console.error('Failed to list quota providers:', error);
      res.status(500).json({ error: error.message || 'Failed to list quota providers' });
    }
  });

  // 查询指定 provider 受管凭据的脱敏状态摘要。
  app.get('/api/quota/credentials/:providerId', (req, res) => {
    const providerId = getProvider(req, res);
    if (providerId) res.json(getManagedCredentialStatus(providerId));
  });

  // 写入/覆盖受管凭据：规范化请求体，实测校验通过才落盘，失败回 400。
  app.put('/api/quota/credentials/:providerId', express.json({ limit: '16kb' }), async (req, res) => {
    const providerId = getProvider(req, res);
    if (!providerId) return;
    const credential = normalizers[providerId](req.body);
    if (!credential) return credentialError(res, new Error('Invalid credential'));
    try {
      await validators[providerId](credential);
      res.json(writeManagedCredential(providerId, credential));
    } catch (error) {
      credentialError(res, error);
    }
  });

  // 用已保存的凭据发起一次真实校验；未配置回 404 NOT_CONFIGURED。
  app.post('/api/quota/credentials/:providerId/validate', async (req, res) => {
    const providerId = getProvider(req, res);
    if (!providerId) return;
    const credential = readManagedCredential(providerId);
    if (!credential) return res.status(404).json({ code: 'NOT_CONFIGURED', error: 'Not configured' });
    try {
      await validators[providerId](credential);
      res.json({ valid: true });
    } catch (error) {
      credentialError(res, error);
    }
  });

  // 从本机 Cursor 安装导入凭据（仅 cursor 支持，其余回 404）。
  app.post('/api/quota/credentials/:providerId/import', async (req, res) => {
    const providerId = getProvider(req, res);
    if (!providerId) return;
    if (providerId !== 'cursor') return res.status(404).json({ code: 'IMPORT_UNAVAILABLE', error: 'Import unavailable' });
    try {
      res.json(await importCursorCredential());
    } catch (error) {
      credentialError(res, error);
    }
  });

  // 删除指定 provider 的受管凭据。
  app.delete('/api/quota/credentials/:providerId', (req, res) => {
    const providerId = getProvider(req, res);
    if (!providerId) return;
    deleteManagedCredential(providerId);
    res.json({ configured: false });
  });

  // 拉取指定 provider 的当前配额用量；异常打日志并回 500。
  app.get('/api/quota/:providerId', async (req, res) => {
    try {
      const { providerId } = req.params;
      if (!providerId) return res.status(400).json({ error: 'Provider ID is required' });
      const { fetchQuotaForProvider } = await getQuotaProviders();
      res.json(await fetchQuotaForProvider(providerId));
    } catch (error) {
      console.error('Failed to fetch quota:', error);
      res.status(500).json({ error: error.message || 'Failed to fetch quota' });
    }
  });
}
