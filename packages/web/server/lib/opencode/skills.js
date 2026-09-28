/**
 * OpenCode 技能（skill）的发现与文件系统 CRUD 核心层。
 *
 * 统一处理多套技能目录约定：
 * - OpenCode 原生：用户级 `~/.config/opencode/skills`（SKILL_DIR）与项目级
 *   `<项目>/.opencode/skills`，并兼容旧版单数目录 `skill`（读取时旧目录存在且新目录
 *   不存在则用旧目录，写入时一律用新目录）；
 * - 外部生态：Claude Code 的 `.claude/skills` 与 agents 约定的 `.agents/skills`
 *   （均分用户级 home 目录与项目级两种位置）；
 * - 额外来源：OpenCode 配置 `skills.paths` 声明的目录，以及 XDG 与 macOS cache
 *   目录中缓存的技能。
 *
 * 本模块只做同步文件操作与路径解析，不感知 HTTP；路由层（skill-routes.js）通过
 * 依赖注入消费这里的导出函数。所有函数失败时直接 throw，由路由层转换为错误响应。
 */
import fs from 'fs';
import path from 'path';
import os from 'os';
import {
  SKILL_DIR,
  OPENCODE_CONFIG_DIR,
  SKILL_SCOPE,
  ensureDirs,
  parseMdFile,
  writeMdFile,
  readConfigLayers,
  readConfig,
  walkSkillMdFiles,
  addSkillFromMdFile,
  resolveSkillSearchDirectories,
  listSkillSupportingFiles,
  readSkillSupportingFile,
  writeSkillSupportingFile,
  deleteSkillSupportingFile,
  getAncestors,
  findWorktreeRoot,
} from './shared.js';

/**
 * 内置技能（built-in skill）的虚拟路径哨兵值。
 * OpenCode 引擎上报内置技能时没有对应的真实文件路径，用该值标记；
 * 后续的读写、改名、托管路径判断逻辑据此跳过文件系统操作。
 */
const BUILT_IN_SKILL_LOCATION = '<built-in>';

/**
 * 确保项目级技能目录存在（幂等创建）。
 * 同时补齐新版复数目录 `.opencode/skills` 与旧版单数目录 `.opencode/skill`，
 * 旧目录仍需存在是因为部分工具链还会读取它。
 * @param {string} workingDirectory 项目根目录（工作目录）路径
 * @returns {string} 新版复数形式的技能根目录路径
 */
function ensureProjectSkillDir(workingDirectory) {
  const projectSkillDir = path.join(workingDirectory, '.opencode', 'skills');
  if (!fs.existsSync(projectSkillDir)) {
    fs.mkdirSync(projectSkillDir, { recursive: true });
  }
  const legacyProjectSkillDir = path.join(workingDirectory, '.opencode', 'skill');
  if (!fs.existsSync(legacyProjectSkillDir)) {
    fs.mkdirSync(legacyProjectSkillDir, { recursive: true });
  }
  return projectSkillDir;
}

/**
 * 解析项目级技能目录：仅当旧版单数目录存在且新版复数目录不存在时返回旧路径，
 * 否则一律返回新版 `.opencode/skills/<skillName>`，保证新写入总是落到新目录。
 */
function getProjectSkillDir(workingDirectory, skillName) {
  const pluralPath = path.join(workingDirectory, '.opencode', 'skills', skillName);
  const legacyPath = path.join(workingDirectory, '.opencode', 'skill', skillName);
  if (fs.existsSync(legacyPath) && !fs.existsSync(pluralPath)) return legacyPath;
  return pluralPath;
}

/**
 * 解析项目级技能的 SKILL.md 路径；目录选择规则与 getProjectSkillDir 相同
 * （旧版单数目录仅在无新版复数目录时生效）。
 */
function getProjectSkillPath(workingDirectory, skillName) {
  const pluralPath = path.join(workingDirectory, '.opencode', 'skills', skillName, 'SKILL.md');
  const legacyPath = path.join(workingDirectory, '.opencode', 'skill', skillName, 'SKILL.md');
  if (fs.existsSync(legacyPath) && !fs.existsSync(pluralPath)) return legacyPath;
  return pluralPath;
}

/**
 * 解析用户级技能目录：优先 `~/.config/opencode/skills/<skillName>`，
 * 仅当旧版 `~/.config/opencode/skill/<skillName>` 存在且新目录不存在时返回旧路径。
 */
function getUserSkillDir(skillName) {
  const pluralPath = path.join(SKILL_DIR, skillName);
  const legacyPath = path.join(OPENCODE_CONFIG_DIR, 'skill', skillName);
  if (fs.existsSync(legacyPath) && !fs.existsSync(pluralPath)) return legacyPath;
  return pluralPath;
}

/**
 * 解析用户级技能的 SKILL.md 路径；目录选择规则与 getUserSkillDir 相同。
 */
function getUserSkillPath(skillName) {
  const pluralPath = path.join(SKILL_DIR, skillName, 'SKILL.md');
  const legacyPath = path.join(OPENCODE_CONFIG_DIR, 'skill', skillName, 'SKILL.md');
  if (fs.existsSync(legacyPath) && !fs.existsSync(pluralPath)) return legacyPath;
  return pluralPath;
}

/**
 * 解析项目级 Claude Code 兼容技能目录：`<workingDirectory>/.claude/skills/<skillName>`。
 */
function getClaudeSkillDir(workingDirectory, skillName) {
  return path.join(workingDirectory, '.claude', 'skills', skillName);
}

/**
 * 解析项目级 Claude Code 兼容技能的 SKILL.md 路径。
 */
function getClaudeSkillPath(workingDirectory, skillName) {
  return path.join(getClaudeSkillDir(workingDirectory, skillName), 'SKILL.md');
}

/**
 * 解析用户级 Claude Code 兼容技能目录：`~/.claude/skills/<skillName>`。
 */
function getUserClaudeSkillDir(skillName) {
  return path.join(os.homedir(), '.claude', 'skills', skillName);
}

/**
 * 解析用户级 Claude Code 兼容技能的 SKILL.md 路径。
 */
function getUserClaudeSkillPath(skillName) {
  return path.join(getUserClaudeSkillDir(skillName), 'SKILL.md');
}

/**
 * 解析用户级 agents 约定技能目录：`~/.agents/skills/<skillName>`。
 */
function getUserAgentsSkillDir(skillName) {
  return path.join(os.homedir(), '.agents', 'skills', skillName);
}

/**
 * 解析用户级 agents 约定技能的 SKILL.md 路径。
 */
function getUserAgentsSkillPath(skillName) {
  return path.join(getUserAgentsSkillDir(skillName), 'SKILL.md');
}

/**
 * 解析项目级 agents 约定技能目录：`<workingDirectory>/.agents/skills/<skillName>`。
 */
function getProjectAgentsSkillDir(workingDirectory, skillName) {
  return path.join(workingDirectory, '.agents', 'skills', skillName);
}

/**
 * 解析项目级 agents 约定技能的 SKILL.md 路径。
 */
function getProjectAgentsSkillPath(workingDirectory, skillName) {
  return path.join(getProjectAgentsSkillDir(workingDirectory, skillName), 'SKILL.md');
}

/**
 * 定位指定技能当前实际落盘的位置，返回其 scope、SKILL.md 路径与来源。
 *
 * 查找优先级：discoverSkills 的发现结果（覆盖配置层、skills.paths、cache 等
 * 全部来源，内置技能路径为 '<built-in>' 哨兵值时原样返回）＞ 项目级 OpenCode 目录
 * ＞ 项目级 .claude 目录 ＞ 用户级 OpenCode 目录 ＞ 用户级 .claude ＞ 用户级 .agents。
 *
 * @param {string} skillName 技能名
 * @param {string|null} workingDirectory 项目工作目录；为空时跳过所有项目级查找
 * @returns {{scope: string|null, path: string|null, source: string|null}}
 *   找不到时三个字段均为 null；source 取 'opencode'、'claude' 或 'agents'
 */
function getSkillScope(skillName, workingDirectory) {
  const discovered = discoverSkills(workingDirectory).find((skill) => skill.name === skillName);
  if (discovered?.path) {
    return { scope: discovered.scope || null, path: discovered.path, source: discovered.source || null };
  }

  if (workingDirectory) {
    const projectPath = getProjectSkillPath(workingDirectory, skillName);
    if (fs.existsSync(projectPath)) {
      return { scope: SKILL_SCOPE.PROJECT, path: projectPath, source: 'opencode' };
    }
    
    const claudePath = getClaudeSkillPath(workingDirectory, skillName);
    if (fs.existsSync(claudePath)) {
      return { scope: SKILL_SCOPE.PROJECT, path: claudePath, source: 'claude' };
    }
  }
  
  const userPath = getUserSkillPath(skillName);
  if (fs.existsSync(userPath)) {
    return { scope: SKILL_SCOPE.USER, path: userPath, source: 'opencode' };
  }

  const userClaudePath = getUserClaudeSkillPath(skillName);
  if (fs.existsSync(userClaudePath)) {
    return { scope: SKILL_SCOPE.USER, path: userClaudePath, source: 'claude' };
  }

  const userAgentsPath = getUserAgentsSkillPath(skillName);
  if (fs.existsSync(userAgentsPath)) {
    return { scope: SKILL_SCOPE.USER, path: userAgentsPath, source: 'agents' };
  }
  
  return { scope: null, path: null, source: null };
}

/**
 * 决定新建技能的写入位置：技能已存在时沿用现有路径与 scope；
 * 否则按请求的 scope（缺省 user）返回对应的目标路径，且总是落在 OpenCode 原生目录。
 */
function getSkillWritePath(skillName, workingDirectory, requestedScope) {
  const existing = getSkillScope(skillName, workingDirectory);
  if (existing.path) {
    return existing;
  }
  
  const scope = requestedScope || SKILL_SCOPE.USER;
  if (scope === SKILL_SCOPE.PROJECT && workingDirectory) {
    return { 
      scope: SKILL_SCOPE.PROJECT, 
      path: getProjectSkillPath(workingDirectory, skillName),
      source: 'opencode'
    };
  }
  
  return { 
    scope: SKILL_SCOPE.USER, 
    path: getUserSkillPath(skillName),
    source: 'opencode'
  };
}

/**
 * 扫描所有约定目录，返回去重后的完整技能清单。
 *
 * 收集顺序：用户级 `.claude`/`.agents` 目录 → 工作目录至 worktree 根之间各级祖先
 * 目录的项目级 `.claude`/`.agents` 目录 → OpenCode 配置层目录（`skill` 与 `skills`
 * 两种子目录都扫；用户配置目录标记为 user scope，其余为 project scope）→
 * 配置文件 `skills.paths` 声明的额外目录（支持 `~/` 前缀与相对路径）→
 * XDG/macOS cache 目录下的缓存技能（user scope）。
 *
 * 同名技能先到先得（addSkillFromMdFile 按 name 去重），因此显式目录天然优先于
 * cache 目录。任一目录不存在时静默跳过；读配置失败按空处理。
 * @param {string|null} workingDirectory 工作目录；为空时跳过所有项目级扫描
 * @returns {Array<object>} 去重后的技能对象数组（name、path、scope、source 等）
 */
function discoverSkills(workingDirectory) {
  const skills = new Map();

  for (const externalRootName of ['.claude', '.agents']) {
    const homeRoot = path.join(os.homedir(), externalRootName, 'skills');
    const source = externalRootName === '.agents' ? 'agents' : 'claude';
    for (const skillMdPath of walkSkillMdFiles(homeRoot)) {
      addSkillFromMdFile(skills, skillMdPath, SKILL_SCOPE.USER, source);
    }
  }

  if (workingDirectory) {
    const worktreeRoot = findWorktreeRoot(workingDirectory) || path.resolve(workingDirectory);
    const ancestors = getAncestors(workingDirectory, worktreeRoot);
    for (const ancestor of ancestors) {
      for (const externalRootName of ['.claude', '.agents']) {
        const source = externalRootName === '.agents' ? 'agents' : 'claude';
        const externalSkillsRoot = path.join(ancestor, externalRootName, 'skills');
        for (const skillMdPath of walkSkillMdFiles(externalSkillsRoot)) {
          addSkillFromMdFile(skills, skillMdPath, SKILL_SCOPE.PROJECT, source);
        }
      }
    }
  }

  const configDirectories = resolveSkillSearchDirectories(workingDirectory);
  const homeOpencodeDir = path.resolve(path.join(os.homedir(), '.opencode'));
  const customConfigDir = process.env.OPENCODE_CONFIG_DIR
    ? path.resolve(process.env.OPENCODE_CONFIG_DIR)
    : null;
  for (const dir of configDirectories) {
    for (const subDir of ['skill', 'skills']) {
      const root = path.join(dir, subDir);
      for (const skillMdPath of walkSkillMdFiles(root)) {
        const isUserConfigDir = dir === OPENCODE_CONFIG_DIR
          || dir === homeOpencodeDir
          || (customConfigDir && dir === customConfigDir);
        const scope = isUserConfigDir ? SKILL_SCOPE.USER : SKILL_SCOPE.PROJECT;
        addSkillFromMdFile(skills, skillMdPath, scope, 'opencode');
      }
    }
  }

  let configuredPaths = [];
  try {
    const config = readConfig(workingDirectory);
    configuredPaths = Array.isArray(config?.skills?.paths) ? config.skills.paths : [];
  } catch {
    configuredPaths = [];
  }
  for (const skillPath of configuredPaths) {
    if (typeof skillPath !== 'string' || !skillPath.trim()) continue;
    const expanded = skillPath.startsWith('~/')
      ? path.join(os.homedir(), skillPath.slice(2))
      : skillPath;
    const resolved = path.isAbsolute(expanded)
      ? path.resolve(expanded)
      : path.resolve(workingDirectory || process.cwd(), expanded);
    for (const skillMdPath of walkSkillMdFiles(resolved)) {
      addSkillFromMdFile(skills, skillMdPath, SKILL_SCOPE.PROJECT, 'opencode');
    }
  }

  const cacheCandidates = [];
  if (process.env.XDG_CACHE_HOME) {
    cacheCandidates.push(path.join(process.env.XDG_CACHE_HOME, 'opencode', 'skills'));
  }
  cacheCandidates.push(path.join(os.homedir(), '.cache', 'opencode', 'skills'));
  cacheCandidates.push(path.join(os.homedir(), 'Library', 'Caches', 'opencode', 'skills'));

  for (const cacheRoot of cacheCandidates) {
    if (!fs.existsSync(cacheRoot)) continue;
    const entries = fs.readdirSync(cacheRoot, { withFileTypes: true });
    for (const entry of entries) {
      if (!entry.isDirectory()) continue;
      const skillRoot = path.join(cacheRoot, entry.name);
      for (const skillMdPath of walkSkillMdFiles(skillRoot)) {
        addSkillFromMdFile(skills, skillMdPath, SKILL_SCOPE.USER, 'opencode');
      }
    }
  }

  return Array.from(skills.values());
}

/**
 * 合并两份已发现的技能列表：primary 优先，fallback 只补充 primary 中不存在的技能名。
 * 用于把 OpenCode 引擎上报的技能与本地文件系统扫描结果合并（引擎结果优先）。
 * 名称 trim 后判重，空名或非法条目直接丢弃，保持原有相对顺序。
 */
function mergeDiscoveredSkills(primarySkills = [], fallbackSkills = []) {
  const merged = [];
  const seenNames = new Set();

  // 追加技能：按 trim 后的名称去重，空名或已见过的直接跳过。
  const appendSkill = (skill) => {
    const name = typeof skill?.name === 'string' ? skill.name.trim() : '';
    if (!name || seenNames.has(name)) {
      return;
    }
    seenNames.add(name);
    merged.push(skill);
  };

  for (const skill of primarySkills || []) {
    appendSkill(skill);
  }
  for (const skill of fallbackSkills || []) {
    appendSkill(skill);
  }

  return merged;
}

/**
 * 汇总某个技能在各候选位置的元数据，供 UI 展示与编辑定位。
 *
 * 返回结构的 `md` 字段是按优先级选出的“当前生效”来源（发现结果 ＞ 项目 OpenCode
 * ＞ 项目 .claude ＞ 用户 OpenCode ＞ 用户 .claude ＞ 用户 .agents），包含是否存在、
 * 路径、目录、scope、source、frontmatter 字段列表、description、instructions（正文）
 * 与支撑文件列表；发现结果为内置技能（path 为 '<built-in>'）时不读文件，直接透传
 * 引擎提供的 description/content。其余字段 projectMd、claudeMd、userMd、userClaudeMd、
 * userAgentsMd 分别记录各候选位置的 { exists, path, dir }，供前端做迁移提示。
 *
 * @param {string} skillName 技能名
 * @param {string|null} workingDirectory 工作目录
 * @param {object|null} [discoveredSkill] 可选的发现结果（避免重复全量扫描）；
 *   传入但名称不匹配时会被忽略并退回全量发现
 */
function getSkillSources(skillName, workingDirectory, discoveredSkill = null) {
  // 判断路径是否为可读的普通文件（stat 失败按不存在处理）。
  const isReadableFile = (filePath) => {
    if (!filePath) return false;
    try {
      return fs.statSync(filePath).isFile();
    } catch {
      return false;
    }
  };

  const projectPath = workingDirectory ? getProjectSkillPath(workingDirectory, skillName) : null;
  const projectExists = projectPath && fs.existsSync(projectPath);
  const projectDir = projectExists ? path.dirname(projectPath) : null;
  
  const claudePath = workingDirectory ? getClaudeSkillPath(workingDirectory, skillName) : null;
  const claudeExists = claudePath && fs.existsSync(claudePath);
  const claudeDir = claudeExists ? path.dirname(claudePath) : null;
  const userClaudePath = getUserClaudeSkillPath(skillName);
  const userClaudeExists = fs.existsSync(userClaudePath);
  const userClaudeDir = userClaudeExists ? path.dirname(userClaudePath) : null;
  
  const userPath = getUserSkillPath(skillName);
  const userExists = fs.existsSync(userPath);
  const userDir = userExists ? path.dirname(userPath) : null;

  const userAgentsPath = getUserAgentsSkillPath(skillName);
  const userAgentsExists = fs.existsSync(userAgentsPath);
  const userAgentsDir = userAgentsExists ? path.dirname(userAgentsPath) : null;

  const matchedDiscovered = discoveredSkill && discoveredSkill.name === skillName
    ? discoveredSkill
    : discoverSkills(workingDirectory).find((skill) => skill.name === skillName);
  const discoveredDescription =
    matchedDiscovered && typeof matchedDiscovered.description === 'string'
      ? matchedDiscovered.description
      : '';
  const discoveredContent =
    matchedDiscovered && typeof matchedDiscovered.content === 'string'
      ? matchedDiscovered.content
      : '';
  const discoveredPath =
    matchedDiscovered && typeof matchedDiscovered.path === 'string'
      ? matchedDiscovered.path
      : null;
  const isBuiltInDiscovered = discoveredPath === BUILT_IN_SKILL_LOCATION;
  
  let mdPath = null;
  let mdScope = null;
  let mdSource = null;
  let mdDir = null;
  
  if (isBuiltInDiscovered) {
    mdScope = matchedDiscovered.scope || SKILL_SCOPE.USER;
    mdSource = matchedDiscovered.source || 'opencode';
  } else if (discoveredPath) {
    mdPath = discoveredPath;
    mdScope = matchedDiscovered.scope || null;
    mdSource = matchedDiscovered.source || null;
    mdDir = isReadableFile(discoveredPath) ? path.dirname(discoveredPath) : null;
  } else if (projectExists) {
    mdPath = projectPath;
    mdScope = SKILL_SCOPE.PROJECT;
    mdSource = 'opencode';
    mdDir = projectDir;
  } else if (claudeExists) {
    mdPath = claudePath;
    mdScope = SKILL_SCOPE.PROJECT;
    mdSource = 'claude';
    mdDir = claudeDir;
  } else if (userExists) {
    mdPath = userPath;
    mdScope = SKILL_SCOPE.USER;
    mdSource = 'opencode';
    mdDir = userDir;
  } else if (userClaudeExists) {
    mdPath = userClaudePath;
    mdScope = SKILL_SCOPE.USER;
    mdSource = 'claude';
    mdDir = userClaudeDir;
  } else if (userAgentsExists) {
    mdPath = userAgentsPath;
    mdScope = SKILL_SCOPE.USER;
    mdSource = 'agents';
    mdDir = userAgentsDir;
  }
  
  const mdExists = isBuiltInDiscovered || isReadableFile(mdPath);
  if (!mdExists) {
    mdPath = null;
    mdDir = null;
    mdScope = null;
    mdSource = null;
  }

  const sources = {
    md: {
      exists: mdExists,
      path: mdPath,
      dir: mdDir,
      scope: mdScope,
      source: mdSource,
      fields: isBuiltInDiscovered ? ['description', 'instructions'] : [],
      supportingFiles: [],
      name: matchedDiscovered?.name || skillName,
      description: discoveredDescription,
      instructions: isBuiltInDiscovered ? discoveredContent : ''
    },
    projectMd: {
      exists: projectExists,
      path: projectPath,
      dir: projectDir
    },
    claudeMd: {
      exists: claudeExists,
      path: claudePath,
      dir: claudeDir
    },
    userMd: {
      exists: userExists,
      path: userPath,
      dir: userDir
    },
    userClaudeMd: {
      exists: userClaudeExists,
      path: userClaudePath,
      dir: userClaudeDir
    },
    userAgentsMd: {
      exists: userAgentsExists,
      path: userAgentsPath,
      dir: userAgentsDir
    }
  };

  if (mdExists && mdDir) {
    const { frontmatter, body } = parseMdFile(mdPath);
    sources.md.fields = Object.keys(frontmatter);
    sources.md.description = frontmatter.description || '';
    sources.md.name = frontmatter.name || skillName;
    if (body) {
      sources.md.fields.push('instructions');
      sources.md.instructions = body;
    } else {
      sources.md.instructions = '';
    }
    sources.md.supportingFiles = listSkillSupportingFiles(mdDir);
  }

  return sources;
}

/**
 * 校验技能名：1-64 个字符，仅小写字母与数字，可用连字符连接但首尾不得是连字符。
 */
function isValidSkillName(skillName) {
  return typeof skillName === 'string'
    && skillName.length > 0
    && skillName.length <= 64
    && /^[a-z0-9][a-z0-9-]*[a-z0-9]$|^[a-z0-9]$/.test(skillName);
}

/**
 * 校验技能名，非法时抛出带原因的 Error（供路由层直接返回给客户端）。
 */
function assertValidSkillName(skillName) {
  if (!isValidSkillName(skillName)) {
    throw new Error(`Invalid skill name "${skillName}". Must be 1-64 lowercase alphanumeric characters with hyphens, cannot start or end with hyphen.`);
  }
}

/**
 * 新建技能：写入 SKILL.md（frontmatter + 正文），可选写入支撑文件。
 *
 * 步骤：确保全局目录存在 → 校验名称 → 确认任意 scope 下均无同名技能（否则抛错）
 * → 按 scope 与 config.source 选择目标目录 → 创建目录并写 SKILL.md → 逐个落盘
 * supportingFiles。scope 为 project 时必须提供工作目录，否则降级为 user；
 * config.source 为 'agents' 时写入 `.agents/skills` 约定目录，否则写 OpenCode 原生目录。
 * frontmatter.name 缺省用 skillName，description 缺失抛错；config 中的
 * instructions/scope/source/supportingFiles 不会进入 frontmatter。
 *
 * @param {string} skillName 技能名
 * @param {object} config 技能内容：description 必填，instructions 为正文，
 *   source 可为 'agents'，supportingFiles 为 [{ path, content }]，其余键并入 frontmatter
 * @param {string|null} workingDirectory 项目工作目录（project scope 必需）
 * @param {string} scope 'user' 或 'project'
 */
function createSkill(skillName, config, workingDirectory, scope) {
  ensureDirs();
  assertValidSkillName(skillName);

  const existing = getSkillScope(skillName, workingDirectory);
  if (existing.path) {
    throw new Error(`Skill ${skillName} already exists at ${existing.path}`);
  }

  let targetDir;
  let targetPath;
  let targetScope;
  
  const requestedScope = scope === SKILL_SCOPE.PROJECT ? SKILL_SCOPE.PROJECT : SKILL_SCOPE.USER;
  const requestedSource = config?.source === 'agents' ? 'agents' : 'opencode';

  if (requestedScope === SKILL_SCOPE.PROJECT && workingDirectory) {
    ensureProjectSkillDir(workingDirectory);
    if (requestedSource === 'agents') {
      targetDir = getProjectAgentsSkillDir(workingDirectory, skillName);
      targetPath = getProjectAgentsSkillPath(workingDirectory, skillName);
    } else {
      targetDir = getProjectSkillDir(workingDirectory, skillName);
      targetPath = getProjectSkillPath(workingDirectory, skillName);
    }
    targetScope = SKILL_SCOPE.PROJECT;
  } else {
    if (requestedSource === 'agents') {
      targetDir = getUserAgentsSkillDir(skillName);
      targetPath = getUserAgentsSkillPath(skillName);
    } else {
      targetDir = getUserSkillDir(skillName);
      targetPath = getUserSkillPath(skillName);
    }
    targetScope = SKILL_SCOPE.USER;
  }

  fs.mkdirSync(targetDir, { recursive: true });

  const { instructions, scope: _scopeFromConfig, source: _sourceFromConfig, supportingFiles, ...frontmatter } = config;
  void _scopeFromConfig;
  void _sourceFromConfig;

  if (!frontmatter.name) {
    frontmatter.name = skillName;
  }
  if (!frontmatter.description) {
    throw new Error('Skill description is required');
  }

  writeMdFile(targetPath, frontmatter, instructions || '');
  
  if (supportingFiles && Array.isArray(supportingFiles)) {
    for (const file of supportingFiles) {
      if (file.path && file.content !== undefined) {
        writeSkillSupportingFile(targetDir, file.path, file.content);
      }
    }
  }
  
  console.log(`Created new skill: ${skillName} (scope: ${targetScope}, path: ${targetPath})`);
}

/**
 * 更新已有技能：可修改 frontmatter 字段、instructions 正文以及支撑文件。
 *
 * targetPath 非空时直接以该绝对路径为目标（必须是 SKILL.md 文件），否则按
 * getSkillScope 定位；找不到抛 `Skill "<name>" not found`。定位后还会校验文件
 * frontmatter.name 与 skillName 一致，防止改错文件。
 *
 * updates 处理规则：scope/source/targetPath/renameTo 是控制字段，忽略不写入；
 * instructions 覆盖正文；supportingFiles 数组按条目的 delete 标志删除或按 content
 * 写入；其余键一律并入 frontmatter。仅在 SKILL.md 内容被修改时才回写文件。
 *
 * @param {string} skillName 技能名
 * @param {object} updates 待更新的字段集合
 * @param {string|null} workingDirectory 项目工作目录
 * @param {string|null} [targetPath] 显式指定的 SKILL.md 绝对路径（编辑外部技能时使用）
 */
function updateSkill(skillName, updates, workingDirectory, targetPath = null) {
  ensureDirs();

  const requestedPath = typeof targetPath === 'string' && targetPath.trim()
    ? path.resolve(targetPath.trim())
    : null;
  const existing = requestedPath && fs.existsSync(requestedPath)
    ? { scope: null, path: requestedPath, source: null }
    : getSkillScope(skillName, workingDirectory);
  if (!existing.path) {
    throw new Error(`Skill "${skillName}" not found`);
  }
  if (path.basename(existing.path) !== 'SKILL.md') {
    throw new Error(`Skill "${skillName}" target must be a SKILL.md file`);
  }
  
  const mdPath = existing.path;
  const mdDir = path.dirname(mdPath);
  const mdData = parseMdFile(mdPath);
  const frontmatterName = typeof mdData.frontmatter?.name === 'string' ? mdData.frontmatter.name : skillName;
  if (frontmatterName !== skillName) {
    throw new Error(`Skill "${skillName}" does not match ${mdPath}`);
  }

  let mdModified = false;

  for (const [field, value] of Object.entries(updates)) {
    if (field === 'scope' || field === 'source' || field === 'targetPath' || field === 'renameTo') {
      continue;
    }
    
    if (field === 'instructions') {
      const normalizedValue = typeof value === 'string' ? value : (value == null ? '' : String(value));
      mdData.body = normalizedValue;
      mdModified = true;
      continue;
    }

    if (field === 'supportingFiles') {
      if (Array.isArray(value)) {
        for (const file of value) {
          if (file.delete && file.path) {
            deleteSkillSupportingFile(mdDir, file.path);
          } else if (file.path && file.content !== undefined) {
            writeSkillSupportingFile(mdDir, file.path, file.content);
          }
        }
      }
      continue;
    }

    mdData.frontmatter[field] = value;
    mdModified = true;
  }

  if (mdModified) {
    writeMdFile(mdPath, mdData.frontmatter, mdData.body);
  }

  console.log(`Updated skill: ${skillName} (path: ${mdPath})`);
}

/**
 * 删除技能：移除所有约定位置中同名技能的整个目录（含支撑文件）。
 *
 * 依次尝试项目级 OpenCode、项目级 .claude、项目级 .agents、用户级 OpenCode、
 * 用户级 .agents、用户级 .claude 目录，找到即递归删除并记录日志；
 * 一个都不存在时抛出 `Skill "<name>" not found`。
 */
function deleteSkill(skillName, workingDirectory) {
  let deleted = false;

  if (workingDirectory) {
    const projectDir = getProjectSkillDir(workingDirectory, skillName);
    if (fs.existsSync(projectDir)) {
      fs.rmSync(projectDir, { recursive: true, force: true });
      console.log(`Deleted project-level skill directory: ${projectDir}`);
      deleted = true;
    }
    
    const claudeDir = getClaudeSkillDir(workingDirectory, skillName);
    if (fs.existsSync(claudeDir)) {
      fs.rmSync(claudeDir, { recursive: true, force: true });
      console.log(`Deleted claude-compat skill directory: ${claudeDir}`);
      deleted = true;
    }

    const projectAgentsDir = getProjectAgentsSkillDir(workingDirectory, skillName);
    if (fs.existsSync(projectAgentsDir)) {
      fs.rmSync(projectAgentsDir, { recursive: true, force: true });
      console.log(`Deleted project-level agents skill directory: ${projectAgentsDir}`);
      deleted = true;
    }
  }

  const userDir = getUserSkillDir(skillName);
  if (fs.existsSync(userDir)) {
    fs.rmSync(userDir, { recursive: true, force: true });
    console.log(`Deleted user-level skill directory: ${userDir}`);
    deleted = true;
  }

  const userAgentsDir = getUserAgentsSkillDir(skillName);
  if (fs.existsSync(userAgentsDir)) {
    fs.rmSync(userAgentsDir, { recursive: true, force: true });
    console.log(`Deleted user-level agents skill directory: ${userAgentsDir}`);
    deleted = true;
  }

  const userClaudeDir = getUserClaudeSkillDir(skillName);
  if (fs.existsSync(userClaudeDir)) {
    fs.rmSync(userClaudeDir, { recursive: true, force: true });
    console.log(`Deleted user-level claude skill directory: ${userClaudeDir}`);
    deleted = true;
  }

  if (!deleted) {
    throw new Error(`Skill "${skillName}" not found`);
  }
}

/**
 * 判断 candidatePath（解析后）是否等于 parentPath 或位于其目录树内。
 * 前缀匹配时强制带上路径分隔符，避免 `/foo/bar-baz` 误判在 `/foo/bar` 之下。
 */
function isPathInside(candidatePath, parentPath) {
  if (!candidatePath || !parentPath) return false;
  const resolvedCandidate = path.resolve(candidatePath);
  const resolvedParent = path.resolve(parentPath);
  return resolvedCandidate === resolvedParent
    || resolvedCandidate.startsWith(`${resolvedParent}${path.sep}`);
}

/**
 * 列出本服务“托管”的全部技能根目录（解析并去重后的绝对路径）：
 * 用户级 OpenCode 新/旧目录、`~/.claude/skills`、`~/.agents/skills`、
 * OPENCODE_CONFIG_DIR 覆盖目录，以及工作目录至 worktree 根之间各级祖先下的
 * `.opencode`（新/旧）、`.claude`、`.agents` 目录。
 * 只有落在这些根目录内的技能才允许改名等管理操作。
 */
function getManagedSkillRoots(workingDirectory) {
  const roots = [];
  // 去重地压入一个解析为绝对路径的根目录。
  const pushRoot = (dir) => {
    if (!dir) return;
    const resolved = path.resolve(dir);
    if (!roots.includes(resolved)) {
      roots.push(resolved);
    }
  };

  pushRoot(SKILL_DIR);
  pushRoot(path.join(OPENCODE_CONFIG_DIR, 'skill'));
  pushRoot(path.join(os.homedir(), '.opencode', 'skills'));
  pushRoot(path.join(os.homedir(), '.opencode', 'skill'));
  pushRoot(path.join(os.homedir(), '.claude', 'skills'));
  pushRoot(path.join(os.homedir(), '.agents', 'skills'));

  const customConfigDir = process.env.OPENCODE_CONFIG_DIR
    ? path.resolve(process.env.OPENCODE_CONFIG_DIR)
    : null;
  if (customConfigDir) {
    pushRoot(path.join(customConfigDir, 'skills'));
    pushRoot(path.join(customConfigDir, 'skill'));
  }

  if (workingDirectory) {
    const worktreeRoot = findWorktreeRoot(workingDirectory) || path.resolve(workingDirectory);
    for (const ancestor of getAncestors(workingDirectory, worktreeRoot)) {
      pushRoot(path.join(ancestor, '.opencode', 'skills'));
      pushRoot(path.join(ancestor, '.opencode', 'skill'));
      pushRoot(path.join(ancestor, '.claude', 'skills'));
      pushRoot(path.join(ancestor, '.agents', 'skills'));
    }
  }

  return roots;
}

/**
 * 判断某个 SKILL.md 路径是否位于托管技能根目录之内。
 * 空路径与内置技能哨兵值 '<built-in>' 均不算托管路径；rename 前用它做越界防护。
 */
function isManagedSkillPath(skillMdPath, workingDirectory) {
  if (!skillMdPath || skillMdPath === BUILT_IN_SKILL_LOCATION) {
    return false;
  }
  const skillDir = path.dirname(path.resolve(skillMdPath));
  return getManagedSkillRoots(workingDirectory).some((root) => isPathInside(skillDir, root));
}

/**
 * 重命名技能：原地重命名技能目录，并同步改写 SKILL.md frontmatter 中的 name。
 *
 * 前置校验：新名称合法；新旧名不同；旧技能存在、非内置、目标是 SKILL.md、
 * 位于托管目录内、frontmatter.name 与旧名一致；新名字无冲突且目标目录不存在。
 * 校验全部通过后先重命名目录（保证支撑文件与正文一并保留），再改写 name 字段；
 * 改写失败时尝试把目录回滚到旧名，回滚也失败则记录错误并抛出原始异常。
 */
function renameSkill(oldName, newName, workingDirectory) {
  ensureDirs();
  assertValidSkillName(newName);

  if (oldName === newName) {
    return;
  }

  const existing = getSkillScope(oldName, workingDirectory);
  if (!existing.path) {
    throw new Error(`Skill "${oldName}" not found`);
  }
  if (existing.path === BUILT_IN_SKILL_LOCATION || !fs.existsSync(existing.path)) {
    throw new Error(`Skill "${oldName}" cannot be renamed`);
  }
  if (path.basename(existing.path) !== 'SKILL.md') {
    throw new Error(`Skill "${oldName}" target must be a SKILL.md file`);
  }
  if (!isManagedSkillPath(existing.path, workingDirectory)) {
    throw new Error(`Skill "${oldName}" is outside managed skill directories and cannot be renamed`);
  }

  const mdDataBeforeMove = parseMdFile(existing.path);
  const frontmatterName = typeof mdDataBeforeMove.frontmatter?.name === 'string'
    ? mdDataBeforeMove.frontmatter.name
    : oldName;
  if (frontmatterName !== oldName) {
    throw new Error(`Skill "${oldName}" does not match ${existing.path}`);
  }

  const conflict = getSkillScope(newName, workingDirectory);
  if (conflict.path) {
    throw new Error(`Skill ${newName} already exists at ${conflict.path}`);
  }

  const oldDir = path.dirname(existing.path);
  const newDir = path.join(path.dirname(oldDir), newName);
  const directoriesDiffer = path.resolve(oldDir) !== path.resolve(newDir);

  if (directoriesDiffer && fs.existsSync(newDir)) {
    throw new Error(`Skill directory already exists at ${newDir}`);
  }

  // Rename the skill directory in place so supporting files and SKILL.md body are preserved.
  if (directoriesDiffer) {
    fs.renameSync(oldDir, newDir);
  }

  const newPath = path.join(newDir, 'SKILL.md');
  try {
    const mdData = parseMdFile(newPath);
    mdData.frontmatter = {
      ...mdData.frontmatter,
      name: newName,
    };
    writeMdFile(newPath, mdData.frontmatter, mdData.body);
  } catch (error) {
    if (directoriesDiffer && fs.existsSync(newDir) && !fs.existsSync(oldDir)) {
      try {
        fs.renameSync(newDir, oldDir);
      } catch (rollbackError) {
        console.error(`Failed to rollback skill rename from ${newDir} to ${oldDir}:`, rollbackError);
      }
    }
    throw error;
  }

  console.log(`Renamed skill: ${oldName} -> ${newName} (path: ${newPath})`);
}

export {
  getSkillSources,
  discoverSkills,
  mergeDiscoveredSkills,
  createSkill,
  updateSkill,
  deleteSkill,
  renameSkill,
  isManagedSkillPath,
};
