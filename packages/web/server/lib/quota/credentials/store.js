/**
 * 受管 quota 凭据的磁盘存储层。
 *
 * 把 ollama-cloud、cursor 等 provider 的凭据以 JSON 文件形式保存在
 * `OMPCHAMBER_DATA_DIR`（默认 `~/.config/ompchamber`）下的 `quota/`
 * 目录中，目录 0o700、文件 0o600，写入走临时文件 + rename 的原子路径。
 */

import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';

/** 允许走受管存储的 provider 白名单，兼作防路径穿越的硬边界。 */
const MANAGED_QUOTA_PROVIDERS = new Set(['ollama-cloud', 'cursor']);

/**
 * 计算凭据所在目录：优先使用 OMPCHAMBER_DATA_DIR 环境变量（解析为绝对
 * 路径），否则回退到 `~/.config/ompchamber`，凭据统一放在其 `quota/` 子目录。
 */
const credentialsDirectory = () => path.join(
  process.env.OMPCHAMBER_DATA_DIR
    ? path.resolve(process.env.OMPCHAMBER_DATA_DIR)
    : path.join(os.homedir(), '.config', 'ompchamber'),
  'quota',
);

/**
 * 返回 providerId 对应的凭据 JSON 文件绝对路径。
 * providerId 不在白名单时直接抛错，杜绝 `../escape` 之类的任意路径写入。
 */
const credentialPath = (providerId) => {
  if (!MANAGED_QUOTA_PROVIDERS.has(providerId)) throw new Error('Unsupported credential provider');
  return path.join(credentialsDirectory(), `${providerId}.json`);
};

/**
 * 读取指定 provider 的凭据文件并交给 normalize 规范化。
 * 文件不存在（ENOENT）返回 null 表示未配置；其它读取/解析失败只打警告
 * 并返回 null，避免凭据缺失或损坏把调用方整个流程打成错误。
 */
export const readQuotaCredential = (providerId, normalize) => {
  try {
    return normalize(JSON.parse(fs.readFileSync(credentialPath(providerId), 'utf8')));
  } catch (error) {
    if (error?.code !== 'ENOENT') console.warn(`Failed to read ${providerId} quota credentials`);
    return null;
  }
};

/**
 * 原子写入凭据：先确保目录存在且权限为 0o700，再以 0o600 写临时文件、
 * rename 覆盖目标文件（读者永远看不到半截 JSON），最后再压实一次权限。
 * finally 兜底清理残留临时文件（rename 成功后 unlink 失败是预期的）。
 */
export const writeQuotaCredential = (providerId, credential) => {
  const target = credentialPath(providerId);
  const directory = path.dirname(target);
  const temporary = `${target}.${process.pid}.${Date.now()}.tmp`;
  fs.mkdirSync(directory, { recursive: true, mode: 0o700 });
  fs.chmodSync(directory, 0o700);
  try {
    fs.writeFileSync(temporary, `${JSON.stringify(credential, null, 2)}\n`, { mode: 0o600 });
    fs.chmodSync(temporary, 0o600);
    fs.renameSync(temporary, target);
    fs.chmodSync(target, 0o600);
  } finally {
    try { fs.unlinkSync(temporary); } catch {}
  }
};

/**
 * 删除指定 provider 的凭据文件；文件本就不存在（ENOENT）视为已删除，
 * 其余错误原样上抛。
 */
export const deleteQuotaCredential = (providerId) => {
  try { fs.unlinkSync(credentialPath(providerId)); } catch (error) {
    if (error?.code !== 'ENOENT') throw error;
  }
};

// OpenCode Go used to store a browser auth cookie here. Its usage API now uses
// OpenCode's auth.json API key, so remove the obsolete secret without reading it.
/**
 * 清理历史遗留的 `opencode-go.json`：旧版 OpenCode Go 曾把浏览器
 * auth cookie 存在这里，现在用量 API 改用 auth.json 的 API key，
 * 因此不读取内容、直接删除这份过期密钥。文件不存在视为成功。
 */
export const deleteLegacyOpenCodeGoCredential = () => {
  try {
    fs.unlinkSync(path.join(credentialsDirectory(), 'opencode-go.json'));
  } catch (error) {
    if (error?.code !== 'ENOENT') throw error;
  }
};
