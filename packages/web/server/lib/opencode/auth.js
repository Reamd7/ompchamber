/**
 * OpenCode auth.json 文件的读写工具。
 *
 * 直接操作 ~/.local/share/opencode/auth.json（按 provider ID 存放凭证），
 * 供管理界面查询与增删各 provider 的授权。写入前会先备份原文件，且在
 * 非 Windows 平台始终把数据目录与文件权限收紧为 0700/0600。
 */
import fs from 'fs';
import path from 'path';
import os from 'os';

/** OpenCode 数据目录（~/.local/share/opencode）。 */
const OPENCODE_DATA_DIR = path.join(os.homedir(), '.local', 'share', 'opencode');
/** OpenCode 授权文件 auth.json 的完整路径。 */
const AUTH_FILE = path.join(OPENCODE_DATA_DIR, 'auth.json');

/**
 * 读取并解析 auth.json。
 * 文件不存在或内容为空白时返回空对象；JSON 解析失败视为配置损坏，
 * 打印错误后抛出 'Failed to read OpenCode auth configuration'。
 * @returns {object} 以 provider ID 为键的授权映射。
 */
function readAuthFile() {
  if (!fs.existsSync(AUTH_FILE)) {
    return {};
  }
  try {
    const content = fs.readFileSync(AUTH_FILE, 'utf8');
    const trimmed = content.trim();
    if (!trimmed) {
      return {};
    }
    return JSON.parse(trimmed);
  } catch (error) {
    console.error('Failed to read auth file:', error);
    throw new Error('Failed to read OpenCode auth configuration');
  }
}

/**
 * 写入 auth.json（整体覆盖，带备份与权限收紧）。
 * 目录不存在时递归创建（0700）；已有文件先复制为 *.ompchamber.backup
 * （0600）；随后以 0600 权限写入格式化 JSON。任何一步失败都打印错误并
 * 抛出 'Failed to write OpenCode auth configuration'。
 * @param {object} auth - 完整的授权映射。
 */
function writeAuthFile(auth) {
  try {
    if (!fs.existsSync(OPENCODE_DATA_DIR)) {
      fs.mkdirSync(OPENCODE_DATA_DIR, { recursive: true, mode: 0o700 });
    }
    if (process.platform !== 'win32') fs.chmodSync(OPENCODE_DATA_DIR, 0o700);

    if (fs.existsSync(AUTH_FILE)) {
      const backupFile = `${AUTH_FILE}.ompchamber.backup`;
      fs.copyFileSync(AUTH_FILE, backupFile);
      if (process.platform !== 'win32') fs.chmodSync(backupFile, 0o600);
      console.log(`Created auth backup: ${backupFile}`);
    }

    fs.writeFileSync(AUTH_FILE, JSON.stringify(auth, null, 2), { encoding: 'utf8', mode: 0o600 });
    if (process.platform !== 'win32') fs.chmodSync(AUTH_FILE, 0o600);
    console.log('Successfully wrote auth file');
  } catch (error) {
    console.error('Failed to write auth file:', error);
    throw new Error('Failed to write OpenCode auth configuration');
  }
}

/**
 * 删除指定 provider 的授权条目。
 * @param {string} providerId - provider 标识，必填且必须是字符串。
 * @returns {boolean} 是否真正删除；条目不存在时返回 false 且不写盘。
 */
function removeProviderAuth(providerId) {
  if (!providerId || typeof providerId !== 'string') {
    throw new Error('Provider ID is required');
  }

  const auth = readAuthFile();
  
  if (!auth[providerId]) {
    console.log(`Provider ${providerId} not found in auth file, nothing to remove`);
    return false;
  }

  delete auth[providerId];
  writeAuthFile(auth);
  console.log(`Removed provider auth: ${providerId}`);
  return true;
}

/**
 * 查询单个 provider 的授权信息。
 * @param {string} providerId - provider 标识。
 * @returns {object|null} 授权对象，不存在时返回 null。
 */
function getProviderAuth(providerId) {
  const auth = readAuthFile();
  return auth[providerId] || null;
}

/** 列出 auth.json 中已有授权的全部 provider ID。 */
function listProviderAuths() {
  const auth = readAuthFile();
  return Object.keys(auth);
}

// 导出：auth 文件读写、provider 授权增删查与两个关键路径常量
export {
  readAuthFile,
  writeAuthFile,
  removeProviderAuth,
  getProviderAuth,
  listProviderAuths,
  AUTH_FILE,
  OPENCODE_DATA_DIR
};
