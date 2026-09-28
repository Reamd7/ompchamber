/**
 * Git 凭证发现模块。
 *
 * 解析用户主目录下的 ~/.git-credentials 文件（git credential store 格式，
 * 每行一条形如 https://user:token@host/path 的 URL），提取 host 与用户名，
 * 供 GitHub/GitLab 等账号发现与预填场景使用。
 */
import fs from 'fs';
import path from 'path';
import os from 'os';

/** ~/.git-credentials 文件的绝对路径（git credential store 的默认存储位置）。 */
const GIT_CREDENTIALS_PATH = path.join(os.homedir(), '.git-credentials');

/**
 * 扫描 ~/.git-credentials 并提取去重后的 { host, username } 列表。
 *
 * 逐行按 URL 解析：host 取 hostname 拼接非 "/" 的 pathname（支持按 path
 * 区分仓库的托管服务），username 取 URL 内嵌用户名；无法解析或缺少
 * host/username 的行直接跳过，结果按 (host, username) 去重并保持文件顺序。
 * 文件不存在返回空数组；整体读取失败仅打印错误并返回已收集部分，不抛出。
 *
 * @returns {Array<{host: string, username: string}>} 凭证摘要列表（不含密码/token）
 */
export function discoverGitCredentials() {
  const credentials = [];

  if (!fs.existsSync(GIT_CREDENTIALS_PATH)) {
    return credentials;
  }

  try {
    const content = fs.readFileSync(GIT_CREDENTIALS_PATH, 'utf8');
    const lines = content.split('\n').filter(line => line.trim());

    for (const line of lines) {
      try {
        const url = new URL(line.trim());
        const hostname = url.hostname;
        const pathname = url.pathname && url.pathname !== '/' ? url.pathname : '';
        const host = hostname + pathname;
        const username = url.username || '';

        if (host && username) {
          const exists = credentials.some(c => c.host === host && c.username === username);
          if (!exists) {
            credentials.push({ host, username });
          }
        }
      } catch {
        continue;
      }
    }
  } catch (error) {
    console.error('Failed to read .git-credentials:', error);
  }

  return credentials;
}
