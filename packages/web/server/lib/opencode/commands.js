/**
 * OpenCode 自定义命令（command）的配置读写层：管理项目级 / 用户级 .md 命令
 * 文件与 opencode.json 中 command 段的双源读写，供配置实体路由调用。
 * 路径解析兼容旧版单数目录（项目内 .opencode/command 与用户级
 * ~/.config/opencode/command），仅在旧文件存在且新路径未占用时沿用旧位置。
 */
import fs from 'fs';
import path from 'path';
import {
  CONFIG_FILE,
  OPENCODE_CONFIG_DIR,
  COMMAND_DIR,
  COMMAND_SCOPE,
  ensureDirs,
  parseMdFile,
  writeMdFile,
  readConfigLayers,
  writeConfig,
  getJsonEntrySource,
  getJsonWriteTarget,
  isPromptFileReference,
  resolvePromptFilePath,
  writePromptFile,
} from './shared.js';

// ============== COMMAND SCOPE HELPERS ==============

/**
 * Ensure project-level command directory exists
 */
/**
 * 确保项目级命令目录存在：同时创建现行复数目录 .opencode/commands 与旧版
 * 单数目录 .opencode/command（旧路径上的既有文件仍可被读写）。
 * @param {string} workingDirectory 项目根目录
 * @returns {string} 现行项目级命令目录路径
 */
function ensureProjectCommandDir(workingDirectory) {
  const projectCommandDir = path.join(workingDirectory, '.opencode', 'commands');
  if (!fs.existsSync(projectCommandDir)) {
    fs.mkdirSync(projectCommandDir, { recursive: true });
  }
  const legacyProjectCommandDir = path.join(workingDirectory, '.opencode', 'command');
  if (!fs.existsSync(legacyProjectCommandDir)) {
    fs.mkdirSync(legacyProjectCommandDir, { recursive: true });
  }
  return projectCommandDir;
}

/**
 * Get project-level command path
 */
/**
 * 解析项目级命令 .md 的目标路径：仅当旧版单数路径已存在且复数路径不存在时
 * 沿用旧路径，否则返回复数路径（不要求文件已存在）。
 * @param {string} workingDirectory 项目根目录
 * @param {string} commandName 命令名
 * @returns {string} 命令 .md 文件路径
 */
function getProjectCommandPath(workingDirectory, commandName) {
  const pluralPath = path.join(workingDirectory, '.opencode', 'commands', `${commandName}.md`);
  const legacyPath = path.join(workingDirectory, '.opencode', 'command', `${commandName}.md`);
  if (fs.existsSync(legacyPath) && !fs.existsSync(pluralPath)) return legacyPath;
  return pluralPath;
}

/**
 * Get user-level command path
 */
/**
 * 解析用户级命令 .md 的目标路径：同样优先保留已存在的旧版
 * ~/.config/opencode/command 下的文件，否则落在现行 COMMAND_DIR。
 * @param {string} commandName 命令名
 * @returns {string} 命令 .md 文件路径
 */
function getUserCommandPath(commandName) {
  const pluralPath = path.join(COMMAND_DIR, `${commandName}.md`);
  const legacyPath = path.join(OPENCODE_CONFIG_DIR, 'command', `${commandName}.md`);
  if (fs.existsSync(legacyPath) && !fs.existsSync(pluralPath)) return legacyPath;
  return pluralPath;
}

/**
 * Determine command scope based on where the .md file exists
 * Priority: project level > user level > null (built-in only)
 */
/**
 * 按文件存在位置判定命令归属：项目级优先于用户级；两级均无 .md 时返回
 * scope 与 path 双 null（仅剩内置命令或 JSON 配置定义）。
 * @param {string} commandName 命令名
 * @param {string | null} workingDirectory 项目目录（空则跳过项目级探测）
 * @returns {{ scope: string | null, path: string | null }} 归属 scope 与对应 .md 路径
 */
function getCommandScope(commandName, workingDirectory) {
  if (workingDirectory) {
    const projectPath = getProjectCommandPath(workingDirectory, commandName);
    if (fs.existsSync(projectPath)) {
      return { scope: COMMAND_SCOPE.PROJECT, path: projectPath };
    }
  }
  
  const userPath = getUserCommandPath(commandName);
  if (fs.existsSync(userPath)) {
    return { scope: COMMAND_SCOPE.USER, path: userPath };
  }
  
  return { scope: null, path: null };
}

/**
 * Get the path where a command should be written based on scope
 */
/**
 * 决定命令的写入位置：已有 .md 时沿用其现有位置（项目级优先）；新建或覆盖
 * 内置命令时按 requestedScope 选择（缺省用户级），项目级仅在提供项目目录时生效。
 * @param {string} commandName 命令名
 * @param {string | null} workingDirectory 项目目录
 * @param {string | null} [requestedScope] 请求的写入 scope
 * @returns {{ scope: string, path: string }} 写入目标 scope 与路径
 */
function getCommandWritePath(commandName, workingDirectory, requestedScope) {
  // For updates: check existing location first (project takes precedence)
  const existing = getCommandScope(commandName, workingDirectory);
  if (existing.path) {
    return existing;
  }
  
  // For new commands or built-in overrides: use requested scope or default to user
  const scope = requestedScope || COMMAND_SCOPE.USER;
  if (scope === COMMAND_SCOPE.PROJECT && workingDirectory) {
    return { 
      scope: COMMAND_SCOPE.PROJECT, 
      path: getProjectCommandPath(workingDirectory, commandName) 
    };
  }
  
  return { 
    scope: COMMAND_SCOPE.USER, 
    path: getUserCommandPath(commandName) 
  };
}

/**
 * 汇总命令的双源定义状态：md（项目 / 用户级 .md，附 frontmatter 字段列表，
 * 存在正文时额外标记 template 字段）与 json（opencode.json command 段，附
 * 字段列表），另附 projectMd / userMd 分层视图，供设置页展示来源与冲突。
 * @param {string} commandName 命令名
 * @param {string | null} workingDirectory 项目目录（空则跳过项目级探测）
 * @returns {{ md: object, json: object, projectMd: object, userMd: object }}
 *   各来源的存在性、路径、scope 与字段名数组
 */
function getCommandSources(commandName, workingDirectory) {
  const projectPath = workingDirectory ? getProjectCommandPath(workingDirectory, commandName) : null;
  const projectExists = projectPath && fs.existsSync(projectPath);

  const userPath = getUserCommandPath(commandName);
  const userExists = fs.existsSync(userPath);

  const mdPath = projectExists ? projectPath : (userExists ? userPath : null);
  const mdExists = !!mdPath;
  const mdScope = projectExists ? COMMAND_SCOPE.PROJECT : (userExists ? COMMAND_SCOPE.USER : null);

  const layers = readConfigLayers(workingDirectory);
  const jsonSource = getJsonEntrySource(layers, 'command', commandName);
  const jsonSection = jsonSource.section;
  const jsonPath = jsonSource.path || layers.paths.customPath || layers.paths.projectPath || layers.paths.userPath;
  const jsonScope = jsonSource.path === layers.paths.projectPath ? COMMAND_SCOPE.PROJECT : COMMAND_SCOPE.USER;

  const sources = {
    md: {
      exists: mdExists,
      path: mdPath,
      scope: mdScope,
      fields: []
    },
    json: {
      exists: jsonSource.exists,
      path: jsonPath,
      scope: jsonSource.exists ? jsonScope : null,
      fields: []
    },
    projectMd: {
      exists: projectExists,
      path: projectPath
    },
    userMd: {
      exists: userExists,
      path: userPath
    }
  };

  if (mdExists) {
    const { frontmatter, body } = parseMdFile(mdPath);
    sources.md.fields = Object.keys(frontmatter);
    if (body) {
      sources.md.fields.push('template');
    }
  }

  if (jsonSection) {
    sources.json.fields = Object.keys(jsonSection);
  }

  return sources;
}

/**
 * 新建命令：先 ensureDirs，再对项目级 .md、用户级 .md、opencode.json 三处
 * 查重（任一存在即抛错）；随后按 scope 选择目标路径（项目级需提供项目目录，
 * 其余一律用户级），把 config.template 写为正文、其余字段写入 frontmatter。
 * 仅写 .md，不落 JSON 配置。
 * @param {string} commandName 命令名
 * @param {object} config 命令定义（template 为正文，其余字段进 frontmatter）
 * @param {string | null} workingDirectory 项目目录
 * @param {string | null} [scope] 请求的写入 scope
 * @returns {void}
 * @throws {Error} 任一来源已存在同名命令时抛出
 */
function createCommand(commandName, config, workingDirectory, scope) {
  ensureDirs();

  const projectPath = workingDirectory ? getProjectCommandPath(workingDirectory, commandName) : null;
  const userPath = getUserCommandPath(commandName);

  if (projectPath && fs.existsSync(projectPath)) {
    throw new Error(`Command ${commandName} already exists as project-level .md file`);
  }

  if (fs.existsSync(userPath)) {
    throw new Error(`Command ${commandName} already exists as user-level .md file`);
  }

  const layers = readConfigLayers(workingDirectory);
  const jsonSource = getJsonEntrySource(layers, 'command', commandName);
  if (jsonSource.exists) {
    throw new Error(`Command ${commandName} already exists in opencode.json`);
  }

  let targetPath;
  let targetScope;

  if (scope === COMMAND_SCOPE.PROJECT && workingDirectory) {
    ensureProjectCommandDir(workingDirectory);
    targetPath = projectPath;
    targetScope = COMMAND_SCOPE.PROJECT;
  } else {
    targetPath = userPath;
    targetScope = COMMAND_SCOPE.USER;
  }

  const { template, scope: _scopeFromConfig, ...frontmatter } = config;

  writeMdFile(targetPath, frontmatter, template || '');
  console.log(`Created new command: ${commandName} (scope: ${targetScope}, path: ${targetPath})`);
}

/**
 * 更新命令字段：template 字段按来源分流——.md 存在（或作为内置覆盖新建 .md）
 * 时写正文；否则当 JSON 中模板为文件引用时写入对应提示词文件，普通值写入
 * command 段。其余字段优先写原来源（JSON 已有写 JSON，.md frontmatter 已有
 * 写 .md），全新字段按 .md 可用性落盘；最后统一写回 .md / JSON 并打印摘要。
 * @param {string} commandName 命令名
 * @param {object} updates 字段到新值的映射
 * @param {string | null} workingDirectory 项目目录
 * @returns {void}
 * @throws {Error} template 为非法文件引用时抛出
 */
function updateCommand(commandName, updates, workingDirectory) {
  ensureDirs();

  const { scope, path: mdPath } = getCommandWritePath(commandName, workingDirectory);
  const mdExists = mdPath && fs.existsSync(mdPath);

  const layers = readConfigLayers(workingDirectory);
  const jsonSource = getJsonEntrySource(layers, 'command', commandName);
  const jsonSection = jsonSource.section;
  const hasJsonFields = jsonSource.exists && jsonSection && Object.keys(jsonSection).length > 0;
  const jsonTarget = jsonSource.exists
    ? { config: jsonSource.config, path: jsonSource.path }
    : getJsonWriteTarget(layers, workingDirectory ? COMMAND_SCOPE.PROJECT : COMMAND_SCOPE.USER);
  let config = jsonTarget.config || {};

  const isBuiltinOverride = !mdExists && !hasJsonFields;

  let targetPath = mdPath;
  let targetScope = scope;

  if (!mdExists && isBuiltinOverride) {
    targetPath = getUserCommandPath(commandName);
    targetScope = COMMAND_SCOPE.USER;
  }

  const mdData = mdExists ? parseMdFile(mdPath) : (isBuiltinOverride ? { frontmatter: {}, body: '' } : null);

  let mdModified = false;
  let jsonModified = false;
  const creatingNewMd = isBuiltinOverride;

  for (const [field, value] of Object.entries(updates)) {
    if (field === 'template') {
      const normalizedValue = typeof value === 'string' ? value : (value == null ? '' : String(value));

      if (mdExists || creatingNewMd) {
        if (mdData) {
          mdData.body = normalizedValue;
          mdModified = true;
        }
        continue;
      } else if (isPromptFileReference(jsonSection?.template)) {
        const templateFilePath = resolvePromptFilePath(jsonSection.template);
        if (!templateFilePath) {
          throw new Error(`Invalid template file reference for command ${commandName}`);
        }
        writePromptFile(templateFilePath, normalizedValue);
        continue;
      } else if (isPromptFileReference(normalizedValue)) {
        if (!config.command) config.command = {};
        if (!config.command[commandName]) config.command[commandName] = {};
        config.command[commandName].template = normalizedValue;
        jsonModified = true;
        continue;
      }

      if (!config.command) config.command = {};
      if (!config.command[commandName]) config.command[commandName] = {};
      config.command[commandName].template = normalizedValue;
      jsonModified = true;
      continue;
    }

    const inMd = mdData?.frontmatter?.[field] !== undefined;
    const inJson = jsonSection?.[field] !== undefined;

    if (inJson) {
      if (!config.command) config.command = {};
      if (!config.command[commandName]) config.command[commandName] = {};
      config.command[commandName][field] = value;
      jsonModified = true;
    } else if (inMd || creatingNewMd) {
      if (mdData) {
        mdData.frontmatter[field] = value;
        mdModified = true;
      }
    } else {
      if ((mdExists || creatingNewMd) && mdData) {
        mdData.frontmatter[field] = value;
        mdModified = true;
      } else {
        if (!config.command) config.command = {};
        if (!config.command[commandName]) config.command[commandName] = {};
        config.command[commandName][field] = value;
        jsonModified = true;
      }
    }
  }

  if (mdModified && mdData) {
    writeMdFile(targetPath, mdData.frontmatter, mdData.body);
  }

  if (jsonModified) {
    writeConfig(config, jsonTarget.path || CONFIG_FILE);
  }

  console.log(`Updated command: ${commandName} (scope: ${targetScope}, md: ${mdModified}, json: ${jsonModified})`);
}

/**
 * 删除命令：移除项目级与用户级 .md 文件，并从 opencode.json 的 command 段
 * 删除对应条目（三处独立执行，能删几处删几处）；三处均不存在时抛出
 * "not found" 错误。
 * @param {string} commandName 命令名
 * @param {string | null} workingDirectory 项目目录
 * @returns {void}
 * @throws {Error} 命令在任何来源都不存在时抛出
 */
function deleteCommand(commandName, workingDirectory) {
  let deleted = false;

  if (workingDirectory) {
    const projectPath = getProjectCommandPath(workingDirectory, commandName);
    if (fs.existsSync(projectPath)) {
      fs.unlinkSync(projectPath);
      console.log(`Deleted project-level command .md file: ${projectPath}`);
      deleted = true;
    }
  }

  const userPath = getUserCommandPath(commandName);
  if (fs.existsSync(userPath)) {
    fs.unlinkSync(userPath);
    console.log(`Deleted user-level command .md file: ${userPath}`);
    deleted = true;
  }

  const layers = readConfigLayers(workingDirectory);
  const jsonSource = getJsonEntrySource(layers, 'command', commandName);
  if (jsonSource.exists && jsonSource.config && jsonSource.path) {
    if (!jsonSource.config.command) jsonSource.config.command = {};
    delete jsonSource.config.command[commandName];
    writeConfig(jsonSource.config, jsonSource.path);
    console.log(`Removed command from opencode.json: ${commandName}`);
    deleted = true;
  }

  if (!deleted) {
    throw new Error(`Command "${commandName}" not found`);
  }
}

/** 对外导出命令的读取与增删改接口，供配置实体路由（config-entity-routes）调用。 */
export {
  getCommandSources,
  createCommand,
  updateCommand,
  deleteCommand,
};
