/**
 * Git 身份配置（identity profiles）持久化模块。
 *
 * 将多套 git 提交身份（userName/userEmail、authType、sshKey、提交签名
 * 配置等）以 JSON 形式存储在 ~/.config/ompchamber/git-identities.json，
 * 提供整文件加载/保存与按 id 的增删改查，供 git 路由在切换提交身份时
 * 调用。写操作为整文件覆写（非原子写）。
 */
import fs from 'fs';
import path from 'path';
import os from 'os';

/** 身份配置文件所在目录：~/.config/ompchamber。 */
const STORAGE_DIR = path.join(os.homedir(), '.config', 'ompchamber');
/** 身份配置 JSON 文件的完整路径：~/.config/ompchamber/git-identities.json。 */
const STORAGE_FILE = path.join(STORAGE_DIR, 'git-identities.json');

/** 确保存储目录 ~/.config/ompchamber 存在，不存在则递归创建（同步 IO）。 */
function ensureStorageDir() {
  if (!fs.existsSync(STORAGE_DIR)) {
    fs.mkdirSync(STORAGE_DIR, { recursive: true });
  }
}

/**
 * 从磁盘加载身份配置文件。
 *
 * 文件不存在时返回 { profiles: [] }；JSON 解析失败（文件损坏等）打印
 * 错误并同样返回空结构而不抛出，保证调用方总能拿到可用对象。
 *
 * @returns {{profiles?: Array<object>}} 反序列化后的配置对象
 */
export function loadProfiles() {
  ensureStorageDir();

  if (!fs.existsSync(STORAGE_FILE)) {
    return { profiles: [] };
  }

  try {
    const content = fs.readFileSync(STORAGE_FILE, 'utf8');
    const data = JSON.parse(content);
    return data;
  } catch (error) {
    console.error('Failed to load git identity profiles:', error);
    return { profiles: [] };
  }
}

/**
 * 将配置对象以 2 空格缩进的 JSON 覆写回存储文件。
 *
 * 写入前先确保目录存在；失败时打印错误并原样向上抛出，由调用方决定
 * 如何向客户端反馈。
 *
 * @param {{profiles: Array<object>}} data 完整配置对象
 * @returns {boolean} 恒为 true（失败时抛出异常）
 */
export function saveProfiles(data) {
  ensureStorageDir();

  try {
    fs.writeFileSync(STORAGE_FILE, JSON.stringify(data, null, 2), 'utf8');
    return true;
  } catch (error) {
    console.error('Failed to save git identity profiles:', error);
    throw error;
  }
}

/** 读取全部身份配置数组；文件缺失或损坏时得到空数组。 */
export function getProfiles() {
  const data = loadProfiles();
  return data.profiles || [];
}

/**
 * 按 id 查找单条身份配置。
 * @param {string} id 配置唯一标识
 * @returns {object|null} 匹配的配置对象，未找到返回 null
 */
export function getProfile(id) {
  const profiles = getProfiles();
  return profiles.find(p => p.id === id) || null;
}

/**
 * 新建身份配置并持久化。
 *
 * 校验：id 不得与现有配置重复；id、userName、userEmail 为必填，违反时
 * 抛出 Error。其余字段（name、authType、sshKey、signCommits、signingKey、
 * host、color、icon）缺省时落入默认值（authType 默认 'ssh'、color 默认
 * 'keyword'、icon 默认 'branch'）。
 *
 * @param {object} profileData 前端提交的配置数据
 * @returns {object} 规范化并已保存的新配置
 */
export function createProfile(profileData) {
  const profiles = getProfiles();

  if (profiles.some(p => p.id === profileData.id)) {
    throw new Error(`Profile with ID "${profileData.id}" already exists`);
  }

  if (!profileData.id || !profileData.userName || !profileData.userEmail) {
    throw new Error('Profile must have id, userName, and userEmail');
  }

  const newProfile = {
    id: profileData.id,
    name: profileData.name || profileData.userName,
    userName: profileData.userName,
    userEmail: profileData.userEmail,
    authType: profileData.authType || 'ssh',
    sshKey: profileData.sshKey || null,
    signCommits: profileData.signCommits,
    signingKey: profileData.signingKey || null,
    host: profileData.host || null,
    color: profileData.color || 'keyword',
    icon: profileData.icon || 'branch'
  };

  profiles.push(newProfile);
  saveProfiles({ profiles });

  return newProfile;
}

/**
 * 按 id 更新身份配置并持久化。
 *
 * 将 updates 浅合并进现有配置；id 字段强制保持原值，不可被覆盖。
 *
 * @param {string} id 目标配置 id
 * @param {object} updates 待合并的字段集合
 * @returns {object} 合并并保存后的配置
 */
export function updateProfile(id, updates) {
  const profiles = getProfiles();
  const index = profiles.findIndex(p => p.id === id);

  if (index === -1) {
    throw new Error(`Profile with ID "${id}" not found`);
  }

  profiles[index] = {
    ...profiles[index],
    ...updates,
    id: profiles[index].id
  };

  saveProfiles({ profiles });
  return profiles[index];
}

/**
 * 按 id 删除身份配置并持久化。
 *
 * 通过删除前后数量对比判断目标是否存在。
 *
 * @param {string} id 目标配置 id
 * @returns {boolean} 恒为 true
 */
export function deleteProfile(id) {
  const profiles = getProfiles();
  const filteredProfiles = profiles.filter(p => p.id !== id);

  if (filteredProfiles.length === profiles.length) {
    throw new Error(`Profile with ID "${id}" not found`);
  }

  saveProfiles({ profiles: filteredProfiles });
  return true;
}
