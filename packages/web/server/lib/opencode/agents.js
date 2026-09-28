/**
 * OpenCode agent 的 CRUD 实现，供 opencode 路由层调用。
 *
 * agent 有三种存在形态：项目级 .md（<workdir>/.opencode/agents/）、用户级
 * .md（~/.config/opencode/agents/，支持子目录分组布局）以及 opencode.json
 * 的 agent 段。读取时项目级优先于用户级、.md 优先于 JSON；更新时遵循
 * “字段定义在哪就写回哪”，内置 agent 的覆盖则落到用户级。prompt 支持
 * “{file:路径}” 形式的外部文件引用。
 */
import fs from 'fs';
import path from 'path';
import {
  CONFIG_FILE,
  AGENT_DIR,
  AGENT_SCOPE,
  ensureDirs,
  parseMdFile,
  writeMdFile,
  readConfigLayers,
  readConfigFile,
  writeConfig,
  getJsonEntrySource,
  getJsonWriteTarget,
  isPromptFileReference,
  resolvePromptFilePath,
  writePromptFile,
} from './shared.js';

// ============== AGENT SCOPE HELPERS ==============

/**
 * Ensure project-level agent directory exists
 */
/**
 * 确保项目级 agent 目录存在（中文补充）：同时创建新布局 .opencode/agents
 * 与旧布局 .opencode/agent（兼容历史路径），返回新布局的路径。
 */
function ensureProjectAgentDir(workingDirectory) {
  const projectAgentDir = path.join(workingDirectory, '.opencode', 'agents');
  if (!fs.existsSync(projectAgentDir)) {
    fs.mkdirSync(projectAgentDir, { recursive: true });
  }
  const legacyProjectAgentDir = path.join(workingDirectory, '.opencode', 'agent');
  if (!fs.existsSync(legacyProjectAgentDir)) {
    fs.mkdirSync(legacyProjectAgentDir, { recursive: true });
  }
  return projectAgentDir;
}

/**
 * Get project-level agent path
 */
/**
 * 计算项目级 agent 的 .md 路径（中文补充）：默认 .opencode/agents/<name>.md；
 * 仅当旧路径存在而新路径不存在时返回旧路径 .opencode/agent/<name>.md，
 * 以便继续读写历史文件而不产生副本。
 */
function getProjectAgentPath(workingDirectory, agentName) {
  const pluralPath = path.join(workingDirectory, '.opencode', 'agents', `${agentName}.md`);
  const legacyPath = path.join(workingDirectory, '.opencode', 'agent', `${agentName}.md`);
  if (fs.existsSync(legacyPath) && !fs.existsSync(pluralPath)) return legacyPath;
  return pluralPath;
}

/**
 * Create a per-request lookup cache for user-level agent path resolution.
 */
/**
 * 创建一个请求级的用户级 agent 查找缓存（中文补充）：含 名字 → 路径 的
 * 全量索引、单名查询结果缓存、以及索引是否已构建的标志。同一请求内复用
 * 可避免对 AGENT_DIR 的重复扫描。
 */
function createAgentLookupCache() {
  return {
    userAgentIndexByName: new Map(),
    userAgentLookupByName: new Map(),
    userAgentIndexReady: false,
  };
}

/**
 * 构建 cache 中的用户级 agent 索引（仅一次）：深度优先遍历 AGENT_DIR 及其
 * 全部子目录，目录项按文件名字典序处理，收集 .md 文件（去扩展名即
 * agent 名）；同名 agent 以遍历中先遇到者为准。构建完成后由
 * userAgentIndexReady 短路，不再重扫。
 */
function buildUserAgentIndex(cache) {
  if (cache.userAgentIndexReady) return;
  cache.userAgentIndexReady = true;

  if (!fs.existsSync(AGENT_DIR)) return;

  const dirsToVisit = [AGENT_DIR];
  while (dirsToVisit.length > 0) {
    const dir = dirsToVisit.pop();
    let entries;
    try {
      entries = fs.readdirSync(dir, { withFileTypes: true });
    } catch {
      continue;
    }

    entries.sort((a, b) => a.name.localeCompare(b.name));

    for (const entry of entries) {
      if (!entry.isFile() || !entry.name.endsWith('.md')) continue;
      const agentName = entry.name.slice(0, -3);
      if (!cache.userAgentIndexByName.has(agentName)) {
        cache.userAgentIndexByName.set(agentName, path.join(dir, entry.name));
      }
    }

    for (let i = entries.length - 1; i >= 0; i -= 1) {
      const entry = entries[i];
      if (entry.isDirectory()) {
        dirsToVisit.push(path.join(dir, entry.name));
      }
    }
  }
}

/**
 * 按名字查询用户级 agent 的 .md 路径（带缓存）：先查逐名缓存，未命中则
 * 触发一次索引构建后再查索引，并把结果（含 null）写回缓存。
 * 返回绝对路径或 null。
 */
function getIndexedUserAgentPath(agentName, cache) {
  if (cache.userAgentLookupByName.has(agentName)) {
    return cache.userAgentLookupByName.get(agentName);
  }

  buildUserAgentIndex(cache);
  const found = cache.userAgentIndexByName.get(agentName) || null;
  cache.userAgentLookupByName.set(agentName, found);
  return found;
}

/**
 * Get user-level agent path — walks subfolders to support grouped layouts.
 * e.g. ~/.config/opencode/agents/business/ceo-diginno.md
 */
/**
 * 取用户级 agent 的 .md 路径（中文补充）：先看平铺路径 AGENT_DIR/<name>.md，
 * 再看旧目录 agent/<name>.md，再借索引查子目录分组布局；都不存在时返回
 * 平铺路径作为新建 agent 的落点。lookupCache 可传请求级缓存，缺省临时建。
 */
function getUserAgentPath(agentName, lookupCache = null) {
  // 1. Check flat path first (legacy / newly created agents)
  const pluralPath = path.join(AGENT_DIR, `${agentName}.md`);
  if (fs.existsSync(pluralPath)) return pluralPath;

  const legacyPath = path.join(AGENT_DIR, '..', 'agent', `${agentName}.md`);
  if (fs.existsSync(legacyPath)) return legacyPath;

  // 2. Lookup subfolders for grouped layout
  const cache = lookupCache || createAgentLookupCache();
  const found = getIndexedUserAgentPath(agentName, cache);
  if (found) return found;

  // 3. Return expected flat path as default (for new agent creation)
  return pluralPath;
}

/**
 * Determine agent scope based on where the .md file exists
 * Priority: project level > user level > null (built-in only)
 */
/**
 * 判定 agent 实际所在的作用域（中文补充）：项目级 .md 存在则 project，
 * 否则用户级 .md 存在则 user；两者都没有返回 { scope: null, path: null }，
 * 表示只剩内置定义或 JSON 覆盖。
 */
function getAgentScope(agentName, workingDirectory, lookupCache = null) {
  if (workingDirectory) {
    const projectPath = getProjectAgentPath(workingDirectory, agentName);
    if (fs.existsSync(projectPath)) {
      return { scope: AGENT_SCOPE.PROJECT, path: projectPath };
    }
  }
  
  const userPath = getUserAgentPath(agentName, lookupCache);
  if (fs.existsSync(userPath)) {
    return { scope: AGENT_SCOPE.USER, path: userPath };
  }
  
  return { scope: null, path: null };
}

/**
 * Get the path where an agent should be written based on scope
 */
/**
 * 决定更新 agent 时应写入的位置（中文补充）：已有 .md 时原位返回（项目级
 * 优先）；新建或覆盖内置时按 requestedScope（缺省 user）选择落点，
 * project 需要有工作目录，否则回落用户级。
 */
function getAgentWritePath(agentName, workingDirectory, requestedScope, lookupCache = null) {
  // For updates: check existing location first (project takes precedence)
  const existing = getAgentScope(agentName, workingDirectory, lookupCache);
  if (existing.path) {
    return existing;
  }

  // For new agents or built-in overrides: use requested scope or default to user
  const scope = requestedScope || AGENT_SCOPE.USER;
  if (scope === AGENT_SCOPE.PROJECT && workingDirectory) {
    return {
      scope: AGENT_SCOPE.PROJECT,
      path: getProjectAgentPath(workingDirectory, agentName)
    };
  }

  return {
    scope: AGENT_SCOPE.USER,
    path: getUserAgentPath(agentName, lookupCache)
  };
}

/**
 * Detect where an agent's permission field is currently defined
 * Priority: project .md > user .md > project JSON > user JSON
 * Returns: { source: 'md'|'json'|null, scope: 'project'|'user'|null, path: string|null }
 */
/**
 * 探测 agent 的 permission 字段当前定义在哪（中文补充）：优先级为
 * 项目 .md > 用户 .md > custom JSON > 项目 JSON > 用户 JSON，返回
 * { source: 'md'|'json'|null, scope, path }，供更新时写回原处。
 */
function getAgentPermissionSource(agentName, workingDirectory, lookupCache = null) {
  // Check project-level .md first
  if (workingDirectory) {
    const projectMdPath = getProjectAgentPath(workingDirectory, agentName);
    if (fs.existsSync(projectMdPath)) {
      const { frontmatter } = parseMdFile(projectMdPath);
      if (frontmatter.permission !== undefined) {
        return { source: 'md', scope: AGENT_SCOPE.PROJECT, path: projectMdPath };
      }
    }
  }

  // Check user-level .md
  const userMdPath = getUserAgentPath(agentName, lookupCache);
  if (fs.existsSync(userMdPath)) {
    const { frontmatter } = parseMdFile(userMdPath);
    if (frontmatter.permission !== undefined) {
      return { source: 'md', scope: AGENT_SCOPE.USER, path: userMdPath };
    }
  }

  // Check JSON layers in effective override order. readConfigLayers merges
  // user -> project -> custom, so custom wins over project, project over user.
  const layers = readConfigLayers(workingDirectory);

  const customJsonPermission = layers.customConfig?.agent?.[agentName]?.permission;
  if (customJsonPermission !== undefined && layers.paths.customPath) {
    return { source: 'json', scope: 'custom', path: layers.paths.customPath };
  }

  const projectJsonPermission = layers.projectConfig?.agent?.[agentName]?.permission;
  if (projectJsonPermission !== undefined && layers.paths.projectPath) {
    return { source: 'json', scope: AGENT_SCOPE.PROJECT, path: layers.paths.projectPath };
  }

  const userJsonPermission = layers.userConfig?.agent?.[agentName]?.permission;
  if (userJsonPermission !== undefined) {
    return { source: 'json', scope: AGENT_SCOPE.USER, path: layers.paths.userPath };
  }

  return { source: null, scope: null, path: null };
}

/**
 * 把新的 permission 写进目标对象：newPermission 为 null/undefined 时删除
 * 字段（空对象由调用方先归一成 null），否则直接赋值。就地修改 target。
 */
function applyAgentPermission(target, newPermission) {
  if (newPermission == null) {
    delete target.permission;
  } else {
    target.permission = newPermission;
  }
}

/**
 * 汇总一个 agent 在 .md 与 JSON 两侧的存在性与字段清单，供 UI 展示来源。
 * md 侧：项目级/用户级路径与命中者（项目优先），字段为 frontmatter 键，
 * 非空正文额外记为 'prompt'；json 侧：由 getJsonEntrySource 定位条目所在层，
 * 未命中时 path 回退到可写层（custom > project > user）。
 */
function getAgentSources(agentName, workingDirectory, lookupCache = createAgentLookupCache()) {
  const projectPath = workingDirectory ? getProjectAgentPath(workingDirectory, agentName) : null;
  const projectExists = projectPath && fs.existsSync(projectPath);

  const userPath = getUserAgentPath(agentName, lookupCache);
  const userExists = fs.existsSync(userPath);

  const mdPath = projectExists ? projectPath : (userExists ? userPath : null);
  const mdExists = !!mdPath;
  const mdScope = projectExists ? AGENT_SCOPE.PROJECT : (userExists ? AGENT_SCOPE.USER : null);

  const layers = readConfigLayers(workingDirectory);
  const jsonSource = getJsonEntrySource(layers, 'agent', agentName);
  const jsonSection = jsonSource.section;
  const jsonPath = jsonSource.path || layers.paths.customPath || layers.paths.projectPath || layers.paths.userPath;
  const jsonScope = jsonSource.path === layers.paths.projectPath ? AGENT_SCOPE.PROJECT : AGENT_SCOPE.USER;

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
      sources.md.fields.push('prompt');
    }
  }

  if (jsonSection) {
    sources.json.fields = Object.keys(jsonSection);
  }

  return sources;
}

/**
 * 读取一个 agent 的完整配置：.md 存在（项目级优先）时 source 为 'md'，
 * frontmatter 全量返回，非空正文并入 prompt 字段；否则查各层 JSON 的
 * agent 段（source 'json'）；都没有则 source 'none' 且 config 为空对象。
 */
function getAgentConfig(agentName, workingDirectory, lookupCache = createAgentLookupCache()) {
  const projectPath = workingDirectory ? getProjectAgentPath(workingDirectory, agentName) : null;
  const projectExists = projectPath && fs.existsSync(projectPath);

  const userPath = getUserAgentPath(agentName, lookupCache);
  const userExists = fs.existsSync(userPath);

  if (projectExists || userExists) {
    const mdPath = projectExists ? projectPath : userPath;
    const { frontmatter, body } = parseMdFile(mdPath);

    return {
      source: 'md',
      scope: projectExists ? AGENT_SCOPE.PROJECT : AGENT_SCOPE.USER,
      config: {
        ...frontmatter,
        ...(typeof body === 'string' && body.length > 0 ? { prompt: body } : {}),
      },
    };
  }

  const layers = readConfigLayers(workingDirectory);
  const jsonSource = getJsonEntrySource(layers, 'agent', agentName);

  if (jsonSource.exists && jsonSource.section) {
    const scope = jsonSource.path === layers.paths.projectPath ? AGENT_SCOPE.PROJECT : AGENT_SCOPE.USER;
    return {
      source: 'json',
      scope,
      config: { ...jsonSource.section },
    };
  }

  return {
    source: 'none',
    scope: null,
    config: {},
  };
}

/**
 * 新建 agent（.md 形式）：先确保目录存在，再检查项目级 .md、用户级 .md
 * 与 JSON 三处均无同名 agent（有则抛错避免覆盖）。scope 为 project 时
 * 写项目目录，否则写用户级。config 中 prompt 作为正文，scope 字段被剔除，
 * 值为 null/undefined 的 frontmatter 字段被过滤。成功后打 log。
 */
function createAgent(agentName, config, workingDirectory, scope) {
  ensureDirs();
  const lookupCache = createAgentLookupCache();

  const projectPath = workingDirectory ? getProjectAgentPath(workingDirectory, agentName) : null;
  const userPath = getUserAgentPath(agentName, lookupCache);

  if (projectPath && fs.existsSync(projectPath)) {
    throw new Error(`Agent ${agentName} already exists as project-level .md file`);
  }

  if (fs.existsSync(userPath)) {
    throw new Error(`Agent ${agentName} already exists as user-level .md file`);
  }

  const layers = readConfigLayers(workingDirectory);
  const jsonSource = getJsonEntrySource(layers, 'agent', agentName);
  if (jsonSource.exists) {
    throw new Error(`Agent ${agentName} already exists in opencode.json`);
  }

  let targetPath;
  let targetScope;

  if (scope === AGENT_SCOPE.PROJECT && workingDirectory) {
    ensureProjectAgentDir(workingDirectory);
    targetPath = projectPath;
    targetScope = AGENT_SCOPE.PROJECT;
  } else {
    targetPath = userPath;
    targetScope = AGENT_SCOPE.USER;
  }

  const { prompt, scope: _scopeFromConfig, ...rawFrontmatter } = config;
  const frontmatter = Object.fromEntries(
    Object.entries(rawFrontmatter).filter(([, value]) => value !== null && value !== undefined)
  );

  writeMdFile(targetPath, frontmatter, prompt || '');
  console.log(`Created new agent: ${agentName} (scope: ${targetScope}, path: ${targetPath})`);
}

/**
 * 更新 agent 的任意字段，逐字段写回其当前定义处：
 * - prompt：有 .md（或新建覆盖）时改正文；否则 JSON 里是 “{file:…}”
 *   引用就写引用的文件；新值本身是引用时存 JSON，普通文本也存 JSON；
 *   值为 null 表示清空（按同样规则定位清空目标）；
 * - permission：先经 getAgentPermissionSource 定位来源并原位修改，空对象
 *   归一为 null（即删除字段）；来源在其它文件时直接改写那个文件；
 * - 其它字段：值为 null 删除；原本在 JSON 改 JSON、在 .md 改 .md；两侧
 *   都没有时，新建覆盖写 .md，其余进 JSON。
 * 内置 agent 的首次编辑（无 .md 无 JSON 字段）会创建用户级 .md。循环结束
 * 后统一把变更过的 .md 与 JSON 落盘。
 */
function updateAgent(agentName, updates, workingDirectory) {
  ensureDirs();
  const lookupCache = createAgentLookupCache();

  const { scope, path: mdPath } = getAgentWritePath(agentName, workingDirectory, undefined, lookupCache);
  const mdExists = mdPath && fs.existsSync(mdPath);

  const layers = readConfigLayers(workingDirectory);
  const jsonSource = getJsonEntrySource(layers, 'agent', agentName);
  const jsonSection = jsonSource.section;
  const hasJsonFields = jsonSource.exists && jsonSection && Object.keys(jsonSection).length > 0;
  const jsonTarget = jsonSource.exists
    ? { config: jsonSource.config, path: jsonSource.path }
    : getJsonWriteTarget(layers, AGENT_SCOPE.USER);
  let config = jsonTarget.config || {};

  const isBuiltinOverride = !mdExists && !hasJsonFields;

  let targetPath = mdPath;
  let targetScope = scope;

  if (!mdExists && isBuiltinOverride) {
    targetPath = getUserAgentPath(agentName, lookupCache);
    targetScope = AGENT_SCOPE.USER;
  }

  let mdData = mdExists ? parseMdFile(mdPath) : (isBuiltinOverride ? { frontmatter: {}, body: '' } : null);

  let mdModified = false;
  let jsonModified = false;
  const creatingNewMd = isBuiltinOverride;

  for (const [field, value] of Object.entries(updates)) {
    // Skip undefined values — they would overwrite existing frontmatter fields with nothing
    if (value === undefined) continue;

    if (field === 'prompt') {
      if (value === null) {
        if (mdExists || creatingNewMd) {
          if (mdData) {
            mdData.body = '';
            mdModified = true;
          }
          continue;
        }

        if (isPromptFileReference(jsonSection?.prompt)) {
          const promptFilePath = resolvePromptFilePath(jsonSection.prompt);
          if (!promptFilePath) {
            throw new Error(`Invalid prompt file reference for agent ${agentName}`);
          }
          writePromptFile(promptFilePath, '');
          continue;
        }

        if (config.agent?.[agentName]) {
          delete config.agent[agentName].prompt;

          if (Object.keys(config.agent[agentName]).length === 0) {
            delete config.agent[agentName];
          }
          if (Object.keys(config.agent).length === 0) {
            delete config.agent;
          }

          jsonModified = true;
        }
        continue;
      }

      const normalizedValue = typeof value === 'string' ? value : (value == null ? '' : String(value));

      if (mdExists || creatingNewMd) {
        if (mdData) {
          mdData.body = normalizedValue;
          mdModified = true;
        }
        continue;
      } else if (isPromptFileReference(jsonSection?.prompt)) {
        const promptFilePath = resolvePromptFilePath(jsonSection.prompt);
        if (!promptFilePath) {
          throw new Error(`Invalid prompt file reference for agent ${agentName}`);
        }
        writePromptFile(promptFilePath, normalizedValue);
        continue;
      } else if (isPromptFileReference(normalizedValue)) {
        if (!config.agent) config.agent = {};
        if (!config.agent[agentName]) config.agent[agentName] = {};
        config.agent[agentName].prompt = normalizedValue;
        jsonModified = true;
        continue;
      }

      if (!config.agent) config.agent = {};
      if (!config.agent[agentName]) config.agent[agentName] = {};
      config.agent[agentName].prompt = normalizedValue;
      jsonModified = true;
      continue;
    }

    if (field === 'permission') {
      const permissionSource = getAgentPermissionSource(agentName, workingDirectory, lookupCache);
      // The client edits the complete source permission map; persist it verbatim.
      // (The old non-wildcard re-merge resurrected rules the user deleted.)
      const newPermission = value && typeof value === 'object' && Object.keys(value).length === 0 ? null : value;

      if (permissionSource.source === 'md') {
        if (mdData && permissionSource.path === targetPath) {
          applyAgentPermission(mdData.frontmatter, newPermission);
          mdModified = true;
        } else {
          const existingMdData = parseMdFile(permissionSource.path);
          applyAgentPermission(existingMdData.frontmatter, newPermission);
          writeMdFile(permissionSource.path, existingMdData.frontmatter, existingMdData.body);
          console.log(`Updated permission in .md file: ${permissionSource.path}`);
        }
      } else if (permissionSource.source === 'json') {
        if (permissionSource.path === (jsonTarget.path || CONFIG_FILE)) {
          if (!config.agent) config.agent = {};
          if (!config.agent[agentName]) config.agent[agentName] = {};
          applyAgentPermission(config.agent[agentName], newPermission);
          jsonModified = true;
        } else {
          const existingConfig = readConfigFile(permissionSource.path);
          if (!existingConfig.agent) existingConfig.agent = {};
          if (!existingConfig.agent[agentName]) existingConfig.agent[agentName] = {};
          applyAgentPermission(existingConfig.agent[agentName], newPermission);
          writeConfig(existingConfig, permissionSource.path);
          console.log(`Updated permission in JSON: ${permissionSource.path}`);
        }
      } else {
        if (mdExists && mdData) {
          applyAgentPermission(mdData.frontmatter, newPermission);
          mdModified = true;
        } else if (hasJsonFields) {
          if (!config.agent) config.agent = {};
          if (!config.agent[agentName]) config.agent[agentName] = {};
          applyAgentPermission(config.agent[agentName], newPermission);
          jsonModified = true;
        } else {
          const writeTarget = getJsonWriteTarget(layers, AGENT_SCOPE.USER);
          if (!writeTarget.config.agent) writeTarget.config.agent = {};
          if (!writeTarget.config.agent[agentName]) writeTarget.config.agent[agentName] = {};
          applyAgentPermission(writeTarget.config.agent[agentName], newPermission);
          writeConfig(writeTarget.config, writeTarget.path);
          console.log(`Created permission in JSON: ${writeTarget.path}`);
        }
      }
      continue;
    }

    const inMd = mdData?.frontmatter?.[field] !== undefined;
    const inJson = jsonSection?.[field] !== undefined;

    if (value === null) {
      if (mdData && inMd) {
        delete mdData.frontmatter[field];
        mdModified = true;
      }

      if (inJson && config.agent?.[agentName]) {
        delete config.agent[agentName][field];

        if (Object.keys(config.agent[agentName]).length === 0) {
          delete config.agent[agentName];
        }
        if (Object.keys(config.agent).length === 0) {
          delete config.agent;
        }

        jsonModified = true;
      }

      continue;
    }

    if (inJson) {
      if (!config.agent) config.agent = {};
      if (!config.agent[agentName]) config.agent[agentName] = {};
      config.agent[agentName][field] = value;
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
        if (!config.agent) config.agent = {};
        if (!config.agent[agentName]) config.agent[agentName] = {};
        config.agent[agentName][field] = value;
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

  console.log(`Updated agent: ${agentName} (scope: ${targetScope}, md: ${mdModified}, json: ${jsonModified})`);
}

/**
 * 从 config 对象的 agent 段删除一个条目：条目不存在或 agent 段结构异常
 * 返回 false；删除后段为空则连 agent 段一并移除，返回 true。就地修改。
 */
function deleteJsonAgentEntry(config, agentName) {
  const agentMap = config.agent;
  if (!agentMap || typeof agentMap !== 'object' || Array.isArray(agentMap) || !agentMap[agentName]) return false;
  delete agentMap[agentName];
  if (Object.keys(agentMap).length === 0) {
    delete config.agent;
  }
  return true;
}

/**
 * 删除 agent。指定 scope 时只删对应层：project 依次尝试 项目 .md → 项目
 * JSON，未找到抛错；user 依次尝试 用户 .md → custom（或用户）JSON，未找到
 * 抛错。未指定 scope 时按 项目 .md → 用户 .md → JSON 任意层 的顺序删除
 * 第一个命中者；三处都没有则视为内置 agent，抛错拒绝删除。
 */
function deleteAgent(agentName, workingDirectory, scope) {
  const lookupCache = createAgentLookupCache();
  const requestedScope = scope === AGENT_SCOPE.PROJECT || scope === AGENT_SCOPE.USER ? scope : null;

  if ((!requestedScope || requestedScope === AGENT_SCOPE.PROJECT) && workingDirectory) {
    const projectPath = getProjectAgentPath(workingDirectory, agentName);
    if (fs.existsSync(projectPath)) {
      fs.unlinkSync(projectPath);
      console.log(`Deleted project-level agent .md file: ${projectPath}`);
      return;
    }
  }

  if (!requestedScope || requestedScope === AGENT_SCOPE.USER) {
    const userPath = getUserAgentPath(agentName, lookupCache);
    if (fs.existsSync(userPath)) {
      fs.unlinkSync(userPath);
      console.log(`Deleted user-level agent .md file: ${userPath}`);
      return;
    }
  }

  const layers = readConfigLayers(workingDirectory);

  if (requestedScope === AGENT_SCOPE.PROJECT) {
    if (layers.paths.projectPath && deleteJsonAgentEntry(layers.projectConfig, agentName)) {
      writeConfig(layers.projectConfig, layers.paths.projectPath);
      console.log(`Removed project-level agent from opencode.json: ${agentName}`);
      return;
    }
    throw new Error(`Project agent ${agentName} not found`);
  }

  if (requestedScope === AGENT_SCOPE.USER) {
    const userJsonPath = layers.paths.customPath || layers.paths.userPath;
    const userJsonConfig = layers.paths.customPath ? layers.customConfig : layers.userConfig;
    if (userJsonPath && deleteJsonAgentEntry(userJsonConfig, agentName)) {
      writeConfig(userJsonConfig, userJsonPath);
      console.log(`Removed user-level agent from opencode.json: ${agentName}`);
      return;
    }
    throw new Error(`User agent ${agentName} not found`);
  }

  const jsonSource = getJsonEntrySource(layers, 'agent', agentName);
  if (jsonSource.exists && jsonSource.config && jsonSource.path && deleteJsonAgentEntry(jsonSource.config, agentName)) {
    writeConfig(jsonSource.config, jsonSource.path);
    console.log(`Removed agent from opencode.json: ${agentName}`);
    return;
  }

  throw new Error(`Agent ${agentName} is built-in or not deletable`);
}

export {
  getAgentSources,
  getAgentConfig,
  createAgent,
  updateAgent,
  deleteAgent,
};
