/**
 * MCP（Model Context Protocol）服务器配置的数据层。
 *
 * 基于 OpenCode 分层配置（用户级 opencode.json 与项目级 .opencode/opencode.json），
 * 提供 MCP server 条目的列表、单条查询、创建、更新与删除。
 * 条目分两类：local（本地进程，command 数组）与 remote（远程 URL，可携带
 * headers、oauth 与 timeout）。所有写入先经 buildMcpEntry 清洗：剔除
 * name/scope 等非配置字段、丢弃 undefined/null 值并按类型删除互斥字段，
 * 保证落盘 JSON 与 OpenCode 的 schema 一致。
 */
import fs from 'fs';
import path from 'path';
import {
  CONFIG_FILE,
  AGENT_SCOPE,
  readConfigFile,
  readConfigLayers,
  getJsonEntrySource,
  getJsonWriteTarget,
  writeConfig,
} from './shared.js';

// ============== MCP CONFIG HELPERS ==============

/**
 * Validate MCP server name
 * 校验 MCP 服务器名称：必须是非空字符串，且仅由小写字母、数字、连字符与
 * 下划线组成，首尾字符必须是字母或数字（单个字符也合法）；
 * 不合法时直接抛出 Error，调用方无需再判空。
 */
function validateMcpName(name) {
  if (!name || typeof name !== 'string') {
    throw new Error('MCP server name is required');
  }
  if (!/^[a-z0-9][a-z0-9_-]*[a-z0-9]$|^[a-z0-9]$/.test(name)) {
    throw new Error('MCP server name must be lowercase alphanumeric with hyphens/underscores');
  }
}

/**
 * List all MCP server configs from user-level opencode.json
 * 依据条目来源文件路径推断 scope：路径等于项目层配置文件时返回
 * AGENT_SCOPE.PROJECT，否则（用户级配置）返回 AGENT_SCOPE.USER；
 * 无来源路径时返回 null。
 */
function resolveMcpScopeFromPath(layers, sourcePath) {
  if (!sourcePath) return null;
  return sourcePath === layers.paths.projectPath ? AGENT_SCOPE.PROJECT : AGENT_SCOPE.USER;
}

/**
 * 确保项目级 MCP 配置文件 .opencode/opencode.json 的父目录存在
 * （不存在则递归创建），并返回该文件的绝对路径；项目 scope 写入前的准备。
 */
function ensureProjectMcpConfigPath(workingDirectory) {
  const configDir = path.join(workingDirectory, '.opencode');
  if (!fs.existsSync(configDir)) {
    fs.mkdirSync(configDir, { recursive: true });
  }
  return path.join(configDir, 'opencode.json');
}

/**
 * 列出合并配置中的全部 MCP server 条目：读取 mergedConfig.mcp，
 * 过滤掉非对象条目，逐个用 getJsonEntrySource 定位来源层并推断 scope，
 * 返回包含 name、清洗后字段与 scope 的对象数组。
 */
function listMcpConfigs(workingDirectory) {
  const layers = readConfigLayers(workingDirectory);
  const mcp = layers?.mergedConfig?.mcp || {};

  return Object.entries(mcp)
    .filter(([, entry]) => entry && typeof entry === 'object' && !Array.isArray(entry))
    .map(([name, entry]) => {
      const source = getJsonEntrySource(layers, 'mcp', name);
      return {
        name,
        ...buildMcpEntry(entry),
        scope: resolveMcpScopeFromPath(layers, source.path),
      };
    });
}

/**
 * Get a single MCP server config by name
 * 按名称查询单条配置：条目存在时返回清洗后的字段与来源 scope，
 * 不存在时返回 null（不抛错）。
 */
function getMcpConfig(name, workingDirectory) {
  const layers = readConfigLayers(workingDirectory);
  const entry = layers?.mergedConfig?.mcp?.[name];

  if (!entry) {
    return null;
  }
  const source = getJsonEntrySource(layers, 'mcp', name);
  return {
    name,
    ...buildMcpEntry(entry),
    scope: resolveMcpScopeFromPath(layers, source.path),
  };
}

/**
 * Create a new MCP server config entry
 * 创建新条目：先校验名称并确认条目不存在（已存在抛错）；scope 为 project
 * 时要求 workingDirectory 并写入项目级配置文件，否则写入用户级可写目标。
 * 写入前剔除传入数据中的 name 字段并经 buildMcpEntry 清洗。
 */
function createMcpConfig(name, mcpConfig, workingDirectory, scope) {
  validateMcpName(name);

  const layers = readConfigLayers(workingDirectory);
  const source = getJsonEntrySource(layers, 'mcp', name);
  if (source.exists) {
    throw new Error(`MCP server "${name}" already exists`);
  }

  let targetPath = CONFIG_FILE;
  let config = {};

  if (scope === AGENT_SCOPE.PROJECT) {
    if (!workingDirectory) {
      throw new Error('Project scope requires working directory');
    }
    targetPath = ensureProjectMcpConfigPath(workingDirectory);
    config = fs.existsSync(targetPath) ? readConfigFile(targetPath) : {};
  } else {
    const jsonTarget = getJsonWriteTarget(layers, AGENT_SCOPE.USER);
    targetPath = jsonTarget.path || CONFIG_FILE;
    config = jsonTarget.config || {};
  }

  if (!config.mcp || typeof config.mcp !== 'object' || Array.isArray(config.mcp)) {
    config.mcp = {};
  }

  const { name: _ignoredName, ...entryData } = mcpConfig;
  config.mcp[name] = buildMcpEntry(entryData);

  writeConfig(config, targetPath);
  console.log(`Created MCP server config: ${name}`);
}

/**
 * Update an existing MCP server config entry
 * 更新已存在条目：定位条目真实所在的配置层与文件，在原有字段基础上
 * 浅合并 updates（剔除 name），重新清洗后写回同一文件；条目不存在时抛错。
 */
function updateMcpConfig(name, updates, workingDirectory) {
  const layers = readConfigLayers(workingDirectory);
  const source = getJsonEntrySource(layers, 'mcp', name);

  if (!source.exists) {
    throw new Error(`MCP server "${name}" not found`);
  }

  const targetPath = source.path || CONFIG_FILE;
  const config = source.config || (fs.existsSync(targetPath) ? readConfigFile(targetPath) : {});

  if (!config.mcp || typeof config.mcp !== 'object' || Array.isArray(config.mcp)) {
    config.mcp = {};
  }

  const existing = config.mcp[name];
  const { name: _ignoredName, ...updateData } = updates;

  config.mcp[name] = buildMcpEntry({ ...existing, ...updateData });

  writeConfig(config, targetPath);
  console.log(`Updated MCP server config: ${name}`);
}

/**
 * Delete an MCP server config entry
 * 删除指定名称的条目并写回其所在配置文件；mcp 键删空后连带移除该键；
 * 条目不存在时抛出 Error。
 */
function deleteMcpConfig(name, workingDirectory) {
  const layers = readConfigLayers(workingDirectory);
  const source = getJsonEntrySource(layers, 'mcp', name);
  const targetPath = source.path || CONFIG_FILE;
  const config = source.config || (fs.existsSync(targetPath) ? readConfigFile(targetPath) : {});

  if (!config.mcp || typeof config.mcp !== 'object' || config.mcp[name] === undefined) {
    throw new Error(`MCP server "${name}" not found`);
  }

  delete config.mcp[name];

  if (Object.keys(config.mcp).length === 0) {
    delete config.mcp;
  }

  writeConfig(config, targetPath);
  console.log(`Deleted MCP server config: ${name}`);
}

/**
 * Build a clean MCP entry object, omitting undefined/null values
 * 将任意输入清洗为符合 OpenCode schema 的条目：剔除 name/scope；
 * type 归一为 local/remote（非 remote 一律 local）；local 保留非空字符串
 * 数组 command，并删除 url/headers/oauth/timeout；remote 保留 trim 后的 url，
 * 删除 command，headers 清洗为字符串 Record，oauth 仅保留非空的
 * clientId/clientSecret/scope/redirectUri，timeout 仅保留正有限数值；
 * environment 清洗为扁平字符串 Record；enabled 缺省为 true。
 * 非对象输入按空对象处理。
 */
function buildMcpEntry(data) {
  const entry = (data && typeof data === 'object' && !Array.isArray(data))
    ? { ...data }
    : {};

  delete entry.name;
  delete entry.scope;

  // type is required
  entry.type = data.type === 'remote' ? 'remote' : 'local';

  if (entry.type === 'local') {
    // command must be a non-empty array of strings
    if (Array.isArray(data.command) && data.command.length > 0) {
      entry.command = data.command.map(String);
    } else {
      delete entry.command;
    }

    delete entry.url;
    delete entry.headers;
    delete entry.oauth;
    delete entry.timeout;
  } else {
    // remote: url required
    if (data.url && typeof data.url === 'string') {
      entry.url = data.url.trim();
    } else {
      delete entry.url;
    }

    delete entry.command;

    if (data.headers && typeof data.headers === 'object' && !Array.isArray(data.headers)) {
      const cleaned = {};
      for (const [k, v] of Object.entries(data.headers)) {
        if (k && v !== undefined && v !== null) {
          cleaned[k] = String(v);
        }
      }
      if (Object.keys(cleaned).length > 0) {
        entry.headers = cleaned;
      } else {
        delete entry.headers;
      }
    } else if (data.headers === undefined) {
      delete entry.headers;
    }

    if (data.oauth === false) {
      entry.oauth = false;
    } else if (data.oauth && typeof data.oauth === 'object' && !Array.isArray(data.oauth)) {
      const oauth = {};
      if (typeof data.oauth.clientId === 'string' && data.oauth.clientId.trim()) {
        oauth.clientId = data.oauth.clientId.trim();
      }
      if (typeof data.oauth.clientSecret === 'string' && data.oauth.clientSecret.trim()) {
        oauth.clientSecret = data.oauth.clientSecret.trim();
      }
      if (typeof data.oauth.scope === 'string' && data.oauth.scope.trim()) {
        oauth.scope = data.oauth.scope.trim();
      }
      if (typeof data.oauth.redirectUri === 'string' && data.oauth.redirectUri.trim()) {
        oauth.redirectUri = data.oauth.redirectUri.trim();
      }
      if (Object.keys(oauth).length > 0) {
        entry.oauth = oauth;
      } else {
        delete entry.oauth;
      }
    } else if (data.oauth === undefined) {
      delete entry.oauth;
    }

    if (typeof data.timeout === 'number' && Number.isFinite(data.timeout) && data.timeout > 0) {
      entry.timeout = data.timeout;
    } else if (data.timeout === undefined || data.timeout === null || data.timeout === '') {
      delete entry.timeout;
    }
  }

  // environment: flat Record<string, string>
  if (data.environment && typeof data.environment === 'object' && !Array.isArray(data.environment)) {
    const cleaned = {};
    for (const [k, v] of Object.entries(data.environment)) {
      if (k && v !== undefined && v !== null) {
        cleaned[k] = String(v);
      }
    }
    if (Object.keys(cleaned).length > 0) {
      entry.environment = cleaned;
    } else {
      delete entry.environment;
    }
  } else if (data.environment === undefined) {
    delete entry.environment;
  }

  // enabled defaults to true
  entry.enabled = data.enabled !== false;

  return entry;
}

// 对外导出：MCP server 配置的列表、查询、创建、更新与删除。
export {
  listMcpConfigs,
  getMcpConfig,
  createMcpConfig,
  updateMcpConfig,
  deleteMcpConfig,
};
