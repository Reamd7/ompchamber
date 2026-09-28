/**
 * OpenCode 配置与 Markdown 资产的共享读写工具模块。
 *
 * 集中管理三类资源的文件操作：agent / command / skill 的 Markdown 定义文件
 * （frontmatter + 正文），以及分层 JSONC 配置（user / project / custom）的
 * 读取、合并与写回。routes.js、agents.js 等 opencode 路由都基于这里的原语。
 *
 * 关键约定：
 * - 配置按 user <- project <- custom（OPENCODE_CONFIG 环境变量）顺序深度合并；
 * - JSONC 解析失败抛出 code 为 INVALID_JSONC 的错误，并拒绝在其上写入，
 *   防止把半截解析结果覆盖回完整配置文件；
 * - frontmatter 解析镜像 OpenCode 的宽容规则（未加引号的冒号等）。
 */
import fs from 'fs';
import path from 'path';
import os from 'os';
import yaml from 'yaml';
import { parse as parseJsonc, printParseErrorCode } from 'jsonc-parser';

// ============== PATH CONSTANTS ==============

/** OpenCode 用户级配置根目录（~/.config/opencode）。 */
const OPENCODE_CONFIG_DIR = path.join(os.homedir(), '.config', 'opencode');
/** 用户级 agent Markdown 文件目录（OPENCODE_CONFIG_DIR/agents）。 */
const AGENT_DIR = path.join(OPENCODE_CONFIG_DIR, 'agents');
/** 用户级 command Markdown 文件目录（OPENCODE_CONFIG_DIR/commands）。 */
const COMMAND_DIR = path.join(OPENCODE_CONFIG_DIR, 'commands');
/** 用户级 skill 根目录（OPENCODE_CONFIG_DIR/skills）。 */
const SKILL_DIR = path.join(OPENCODE_CONFIG_DIR, 'skills');
/** 用户级配置文件路径（~/.config/opencode/config.json），也是 writeConfig 的默认写入目标。 */
const CONFIG_FILE = path.join(OPENCODE_CONFIG_DIR, 'config.json');
/** 匹配 “{file:路径}” 形式 prompt 文件引用的正则（大小写不敏感），捕获组为路径本身。 */
const PROMPT_FILE_PATTERN = /^\{file:(.+)\}$/i;

// ============== SCOPE TYPE CONSTANTS ==============

/** agent 资源的作用域常量：user 为用户级（全局），project 为项目级。 */
const AGENT_SCOPE = {
  USER: 'user',
  PROJECT: 'project'
};

/** command 资源的作用域常量：user 为用户级（全局），project 为项目级。 */
const COMMAND_SCOPE = {
  USER: 'user',
  PROJECT: 'project'
};

/** skill 资源的作用域常量：user 为用户级（全局），project 为项目级。 */
const SKILL_SCOPE = {
  USER: 'user',
  PROJECT: 'project'
};

// ============== DIRECTORY OPERATIONS ==============

/**
 * 确保 OpenCode 相关的四个目录存在：配置根目录、agents、commands、skills。
 * 缺失时以 recursive 模式逐级创建，已存在则跳过；无返回值。
 * 首次写入任何用户级资产前应先调用。
 */
function ensureDirs() {
  if (!fs.existsSync(OPENCODE_CONFIG_DIR)) {
    fs.mkdirSync(OPENCODE_CONFIG_DIR, { recursive: true });
  }
  if (!fs.existsSync(AGENT_DIR)) {
    fs.mkdirSync(AGENT_DIR, { recursive: true });
  }
  if (!fs.existsSync(COMMAND_DIR)) {
    fs.mkdirSync(COMMAND_DIR, { recursive: true });
  }
  if (!fs.existsSync(SKILL_DIR)) {
    fs.mkdirSync(SKILL_DIR, { recursive: true });
  }
}

// ============== MARKDOWN FILE OPERATIONS ==============

// Mirror of OpenCode's markdown frontmatter sanitizer (packages/opencode/src/
// config/markdown.ts): other coding agents accept unquoted colons in YAML
// values (e.g. `description: Build agent: creates builds`), which strict YAML
// rejects. Rewrite those values as block scalars and retry the parse, so files
// OpenCode accepts are parsed identically here.
/**
 * 宽容化 frontmatter 文本，镜像 OpenCode 的 markdown.ts 处理规则。
 *
 * 逐行检查顶层键值行：值中含未加引号的冒号时，改写为 YAML 块标量
 * （“key: |-” + 缩进行），使严格 YAML 会拒绝的文件仍能解析成功。
 * 注释行、空行、缩进（嵌套）行、已加引号或已是块标量的行原样保留。
 * 返回改写后的文本，不修改原字符串。
 */
function sanitizeFrontmatter(frontmatter) {
  return frontmatter
    .split(/\r?\n/)
    .flatMap((line) => {
      if (line.trim().startsWith('#') || line.trim() === '' || /^\s+/.test(line)) return [line];
      const entry = line.match(/^([a-zA-Z_][a-zA-Z0-9_]*)\s*:\s*(.*)$/);
      if (!entry) return [line];
      const value = entry[2].trim();
      if (value === '' || value === '>' || value === '|' || value.startsWith('"') || value.startsWith("'")) return [line];
      if (!value.includes(':')) return [line];
      return [`${entry[1]}: |-`, `  ${value}`];
    })
    .join('\n');
}

/**
 * 读取并解析一个 Markdown 定义文件（agent / command / SKILL.md）。
 *
 * 返回 { frontmatter, body }：frontmatter 为 YAML 解析出的对象
 * （无 frontmatter 或两次解析都失败时为 {}，失败会打 warn）；body 为结束
 * 分隔线之后的正文（已 trim）。兼容 UTF-8 BOM 与文件末尾无换行的结束
 * 分隔线，否则旧文件的 frontmatter 会被误当作正文在下次保存时重复写入。
 */
function parseMdFile(filePath) {
  const rawContent = fs.readFileSync(filePath, 'utf8');
  // Strip a UTF-8 BOM so frontmatter is recognized regardless of the editor
  // that saved the file.
  const content = rawContent.charCodeAt(0) === 0xfeff ? rawContent.slice(1) : rawContent;
  // The closing `---` may sit at end-of-file without a trailing newline.
  // gray-matter (used by OpenCode) accepts that, so we must too: otherwise the
  // whole file is treated as the prompt body and a later save rewrites the
  // existing YAML block into the body, duplicating the frontmatter.
  const match = content.match(/^---\r?\n([\s\S]*?)\r?\n---(?:\r?\n|$)([\s\S]*)$/);

  if (!match) {
    return { frontmatter: {}, body: content.trim() };
  }

  let frontmatter = {};
  try {
    frontmatter = yaml.parse(match[1]) || {};
  } catch (error) {
    // Lenient fallback for frontmatter that strict YAML rejects but OpenCode
    // still accepts (unquoted colons in scalar values).
    try {
      frontmatter = yaml.parse(sanitizeFrontmatter(match[1])) || {};
    } catch {
      console.warn(`Failed to parse markdown frontmatter ${filePath}, treating as empty:`, error);
      frontmatter = {};
    }
  }

  const body = match[2].trim();
  return { frontmatter, body };
}

/**
 * 把 frontmatter 与正文序列化写回 Markdown 文件。
 *
 * 写入前过滤值为 null / undefined 的 frontmatter 键，避免 YAML 里出现
 * 空值；文件内容固定为 “--- + YAML + --- + 空行 + 正文”。成功打 log，
 * 失败打 error 并抛出 'Failed to write agent markdown file'。
 */
function writeMdFile(filePath, frontmatter, body) {
  try {
    const cleanedFrontmatter = Object.fromEntries(
      Object.entries(frontmatter).filter(([, value]) => value != null)
    );
    const yamlStr = yaml.stringify(cleanedFrontmatter);
    const content = `---\n${yamlStr}---\n\n${body}`;
    fs.writeFileSync(filePath, content, 'utf8');
    console.log(`Successfully wrote markdown file: ${filePath}`);
  } catch (error) {
    console.error(`Failed to write markdown file ${filePath}:`, error);
    throw new Error('Failed to write agent markdown file');
  }
}

// ============== CONFIG FILE OPERATIONS ==============

/**
 * 列出项目级配置文件的候选路径（按优先级）：
 * 工作目录下的 opencode.json、opencode.jsonc，以及 .opencode/ 下的同名两个。
 * workingDirectory 为空时返回空数组。
 */
function getProjectConfigCandidates(workingDirectory) {
  if (!workingDirectory) return [];
  return [
    path.join(workingDirectory, 'opencode.json'),
    path.join(workingDirectory, 'opencode.jsonc'),
    path.join(workingDirectory, '.opencode', 'opencode.json'),
    path.join(workingDirectory, '.opencode', 'opencode.jsonc'),
  ];
}

/**
 * 选出项目级配置文件的实际路径：返回第一个存在的候选；
 * 都不存在时返回首选候选（opencode.json）作为未来写入的落点。
 * workingDirectory 为空时返回 null。
 */
function getProjectConfigPath(workingDirectory) {
  if (!workingDirectory) return null;

  const candidates = getProjectConfigCandidates(workingDirectory);

  for (const candidate of candidates) {
    if (fs.existsSync(candidate)) {
      return candidate;
    }
  }

  return candidates[0];
}

/**
 * 汇总三层配置的路径信息。
 *
 * 返回 { userPaths, projectPath, customPath }：userPaths 为用户级候选列表
 * （config.json 优先）；customPath 来自 OPENCODE_CONFIG 环境变量，每次调用
 * 时才解析（而不是 import 时固化），保证运行时变更与测试隔离生效。
 */
function getConfigPaths(workingDirectory) {
  return {
    userPaths: [
      path.join(OPENCODE_CONFIG_DIR, 'config.json'),
      path.join(OPENCODE_CONFIG_DIR, 'opencode.json'),
      path.join(OPENCODE_CONFIG_DIR, 'opencode.jsonc'),
    ],
    projectPath: getProjectConfigPath(workingDirectory),
    // Resolve at call time so OPENCODE_CONFIG changes (and tests) take effect.
    customPath: process.env.OPENCODE_CONFIG
      ? path.resolve(process.env.OPENCODE_CONFIG)
      : null,
  };
}

/**
 * 从用户级候选路径中选出第一个实际存在的作为主配置；全部不存在时回退
 * 到 CONFIG_FILE（~/.config/opencode/config.json）作为默认写入目标。
 */
function getPrimaryUserConfigPath(userPaths) {
  for (const userPath of userPaths) {
    if (fs.existsSync(userPath)) {
      return userPath;
    }
  }

  return CONFIG_FILE;
}

/** 配置解析失败时挂在 error.code 上的标识，用于把“文件内容坏了”与其它 IO 错误区分开。 */
const INVALID_JSONC = 'INVALID_JSONC';

/** 判断给定错误是否为 JSONC 解析失败（code === 'INVALID_JSONC'）。 */
function isInvalidJsoncError(error) {
  return Boolean(error && typeof error === 'object' && error.code === INVALID_JSONC);
}

/**
 * 构造 JSONC 解析失败的错误消息：包含文件路径；解析器给出定位信息时
 * 追加错误类型与字节偏移量，方便用户定位坏掉的位置。
 */
function formatJsoncParseError(filePath, errors) {
  const first = Array.isArray(errors) && errors.length > 0 ? errors[0] : null;
  const location = first && Number.isFinite(first.offset)
    ? ` (${printParseErrorCode(first.error)} at offset ${first.offset})`
    : '';
  return `OpenCode configuration at ${filePath} contains invalid JSONC and cannot be loaded safely${location}`;
}

/**
 * 判断一次解析是否等价于“空文件”：结果为 undefined 且所有错误都是
 * ValueExpected（即只有注释/空白）。其它错误（YAML、纯文本、杂散 token）
 * 说明文件有真实内容但没看懂，绝不能当作空配置处理。
 */
function isCommentOnlyParse(parsed, errors) {
  // Comment-only / whitespace-only files parse to undefined with nothing but
  // ValueExpected. Any other error means real content we failed to understand
  // (YAML, plain text, a stray leading token), which must not read as empty.
  return parsed === undefined
    && errors.every((entry) => printParseErrorCode(entry.error) === 'ValueExpected');
}

/**
 * 把 JSONC 文本解析为普通对象，是配置读取的统一入口。
 *
 * 允许尾逗号；仅注释/空白的内容返回 {}；解析有错误或结果不是普通对象
 * （如数组、标量）时抛出 code 为 INVALID_JSONC 的错误，绝不返回部分解析树。
 */
function parseConfigObject(content, filePath) {
  const errors = [];
  const parsed = parseJsonc(content, errors, { allowTrailingComma: true });
  if (isCommentOnlyParse(parsed, errors)) {
    return {};
  }
  if (errors.length > 0 || !parsed || typeof parsed !== 'object' || Array.isArray(parsed)) {
    const error = new Error(formatJsoncParseError(filePath, errors));
    error.code = INVALID_JSONC;
    throw error;
  }
  return parsed;
}

/**
 * 读取单个配置文件并返回解析后的对象。
 *
 * 文件不存在或内容全为空白时返回 {}。JSONC 非法时抛出 INVALID_JSONC
 * 错误（历史上忽略解析错误曾导致截断对象覆盖完整配置）；其它读取失败
 * 打 error 后统一抛出 'Failed to read OpenCode configuration'。
 */
function readConfigFile(filePath) {
  if (!filePath || !fs.existsSync(filePath)) {
    return {};
  }
  try {
    const content = fs.readFileSync(filePath, 'utf8');
    const normalized = content.trim();
    if (!normalized) {
      return {};
    }
    // Refuse partial jsonc-parser trees. Ignoring errors previously let mutations
    // rewrite a truncated object (often only `$schema`) over the full config.
    return parseConfigObject(normalized, filePath);
  } catch (error) {
    if (isInvalidJsoncError(error)) {
      throw error;
    }
    console.error(`Failed to read config file: ${filePath}`, error);
    throw new Error('Failed to read OpenCode configuration');
  }
}

/** 判断值是否为非 null、非数组的普通对象（合并逻辑的递归条件）。 */
function isPlainObject(value) {
  return value && typeof value === 'object' && !Array.isArray(value);
}

/**
 * 深度合并两层配置对象：双方均为普通对象时逐键递归合并，
 * 任一方不是普通对象时 override 整体覆盖 base（数组、标量均如此）。
 * 不修改入参，返回新对象。
 */
function mergeConfigs(base, override) {
  if (!isPlainObject(base) || !isPlainObject(override)) {
    return override;
  }
  const result = { ...base };
  for (const [key, value] of Object.entries(override)) {
    if (key in result) {
      const baseValue = result[key];
      if (isPlainObject(baseValue) && isPlainObject(value)) {
        result[key] = mergeConfigs(baseValue, value);
      } else {
        result[key] = value;
      }
    } else {
      result[key] = value;
    }
  }
  return result;
}

/**
 * 读取单个配置层，返回 { config, error }。
 *
 * JSONC 解析失败被捕获并记录在 error 字段（config 为 {}），供上层聚合
 * 成 layerErrors；其它异常（IO 等）原样向上抛出。
 */
function readConfigLayer(filePath) {
  try {
    return { config: readConfigFile(filePath), error: null };
  } catch (error) {
    if (isInvalidJsoncError(error)) {
      console.error(error.message);
      return { config: {}, error };
    }
    throw error;
  }
}

/**
 * 读取并合并 user / project / custom 三层配置。
 *
 * 返回 { userConfig, projectConfig, customConfig, mergedConfig, paths,
 * layerErrors }：前三者是各层原始解析结果；mergedConfig 按
 * user <- project <- custom 顺序深度合并（custom 优先级最高）；
 * layerErrors 只收集 JSONC 解析错误（含路径、code、message），
 * 供写入前的 throwIfLayerError 校验使用。
 */
function readConfigLayers(workingDirectory) {
  const { userPaths, projectPath, customPath } = getConfigPaths(workingDirectory);
  const userPath = getPrimaryUserConfigPath(userPaths);
  const userLayer = readConfigLayer(userPath);
  const projectLayer = readConfigLayer(projectPath);
  const customLayer = readConfigLayer(customPath);
  const mergedConfig = mergeConfigs(
    mergeConfigs(userLayer.config, projectLayer.config),
    customLayer.config,
  );

  const layerErrors = [];
  if (userLayer.error) {
    layerErrors.push({ path: userPath, code: userLayer.error.code, message: userLayer.error.message });
  }
  if (projectLayer.error && projectPath) {
    layerErrors.push({ path: projectPath, code: projectLayer.error.code, message: projectLayer.error.message });
  }
  if (customLayer.error && customPath) {
    layerErrors.push({ path: customPath, code: customLayer.error.code, message: customLayer.error.message });
  }

  return {
    userConfig: userLayer.config,
    projectConfig: projectLayer.config,
    customConfig: customLayer.config,
    mergedConfig,
    paths: { userPath, projectPath, customPath },
    layerErrors,
  };
}

/** 读取工作目录对应的三层配置并只返回合并结果（不关心分层细节时的便捷入口）。 */
function readConfig(workingDirectory) {
  return readConfigLayers(workingDirectory).mergedConfig;
}

/**
 * 根据配置文件路径反查它属于哪一层：命中 customPath 或 projectPath 时返回
 * 对应层配置；其它情况（含 targetPath 为空）一律返回用户层配置。
 */
function getConfigForPath(layers, targetPath) {
  if (!targetPath) {
    return layers.userConfig;
  }
  if (layers.paths.customPath && targetPath === layers.paths.customPath) {
    return layers.customConfig;
  }
  if (layers.paths.projectPath && targetPath === layers.paths.projectPath) {
    return layers.projectConfig;
  }
  return layers.userConfig;
}

/**
 * 把配置对象以两空格缩进的 JSON 写入指定文件（默认 CONFIG_FILE）。
 *
 * 覆盖前的双重保护：先完整解析既有内容（解析不动则拒绝写入，防止破坏
 * 无法理解的文件），再把原文件复制为 *.ompchamber.backup 备份。目标目录
 * 不存在时自动创建。INVALID_JSONC 错误原样抛出；其它失败统一抛出
 * 'Failed to write OpenCode configuration'。
 */
function writeConfig(config, filePath = CONFIG_FILE) {
  try {
    if (fs.existsSync(filePath)) {
      // Defense in depth: never overwrite a file we cannot fully parse.
      const existing = fs.readFileSync(filePath, 'utf8').trim();
      if (existing) {
        parseConfigObject(existing, filePath);
      }

      const backupFile = `${filePath}.ompchamber.backup`;
      fs.copyFileSync(filePath, backupFile);
      console.log(`Created config backup: ${backupFile}`);
    }

    fs.mkdirSync(path.dirname(filePath), { recursive: true });
    fs.writeFileSync(filePath, JSON.stringify(config, null, 2), 'utf8');
    console.log(`Successfully wrote config file: ${filePath}`);
  } catch (error) {
    if (isInvalidJsoncError(error)) {
      throw error;
    }
    console.error(`Failed to write config file: ${filePath}`, error);
    throw new Error('Failed to write OpenCode configuration');
  }
}

/** 在 layers.layerErrors 中查找指定路径的解析错误；找不到或入参不完整时返回 null。 */
function getLayerError(layers, filePath) {
  if (!filePath || !Array.isArray(layers?.layerErrors)) {
    return null;
  }
  return layers.layerErrors.find((entry) => entry.path === filePath) || null;
}

/**
 * 若指定路径的配置层此前解析失败，则用原 code 与 message 重建并抛出错误。
 * 读取或写入某层前调用，强制让操作在坏层上尽早失败，而不是基于不完整数据继续。
 */
function throwIfLayerError(layers, filePath) {
  const failed = getLayerError(layers, filePath);
  if (!failed) {
    return;
  }
  const error = new Error(failed.message);
  error.code = failed.code;
  throw error;
}

/**
 * 按 custom → project → user 的优先级，查找某个配置条目（如 agent、command）
 * 首次出现的层。
 *
 * @param layers readConfigLayers 的返回值
 * @param sectionKey 配置段名（如 'agent'、'command'）
 * @param entryName 段内的条目名
 * @returns 命中时 { section, config, path, exists: true }；各层都没有且无解析
 * 错误时返回 { section: null, config: null, path: null, exists: false }。
 * 查找前会对途经层做 throwIfLayerError 校验，坏层会直接抛错。
 */
function getJsonEntrySource(layers, sectionKey, entryName) {
  const { userConfig, projectConfig, customConfig, paths } = layers;
  if (paths.customPath) {
    throwIfLayerError(layers, paths.customPath);
    const customSection = customConfig?.[sectionKey]?.[entryName];
    if (customSection !== undefined) {
      return { section: customSection, config: customConfig, path: paths.customPath, exists: true };
    }
  }

  if (paths.projectPath && !getLayerError(layers, paths.projectPath)) {
    const projectSection = projectConfig?.[sectionKey]?.[entryName];
    if (projectSection !== undefined) {
      return { section: projectSection, config: projectConfig, path: paths.projectPath, exists: true };
    }
  }

  throwIfLayerError(layers, paths.userPath);
  const userSection = userConfig?.[sectionKey]?.[entryName];
  if (userSection !== undefined) {
    return { section: userSection, config: userConfig, path: paths.userPath, exists: true };
  }

  return { section: null, config: null, path: null, exists: false };
}

/**
 * 决定配置写入应落到哪一层，返回 { config, path }。
 *
 * 规则：设置了 OPENCODE_CONFIG（customPath 存在）时永远写 custom 层；
 * 否则 preferredScope 为 project 且存在项目配置时写项目层；最后回退到
 * 用户层。返回前对目标层做 throwIfLayerError 校验，坏层拒绝写入。
 */
function getJsonWriteTarget(layers, preferredScope) {
  const { userConfig, projectConfig, customConfig, paths } = layers;
  if (paths.customPath) {
    throwIfLayerError(layers, paths.customPath);
    return { config: customConfig, path: paths.customPath };
  }
  if (preferredScope === AGENT_SCOPE.PROJECT && paths.projectPath) {
    throwIfLayerError(layers, paths.projectPath);
    return { config: projectConfig, path: paths.projectPath };
  }
  throwIfLayerError(layers, paths.userPath);
  return { config: userConfig, path: paths.userPath };
}

// ============== GIT/WORKTREE HELPERS ==============

/**
 * 返回从 startDir 逐级向上直到 stopDir（含）的目录绝对路径列表；
 * stopDir 为空或不指定时一直列到文件系统根。startDir 为空返回空数组。
 */
function getAncestors(startDir, stopDir) {
  if (!startDir) return [];
  const result = [];
  let current = path.resolve(startDir);
  const resolvedStop = stopDir ? path.resolve(stopDir) : null;

  while (true) {
    result.push(current);
    if (resolvedStop && current === resolvedStop) {
      break;
    }
    const parent = path.dirname(current);
    if (parent === current) {
      break;
    }
    current = parent;
  }

  return result;
}

/**
 * 从 startDir 向上查找最近一个包含 .git 条目的目录作为 worktree 根；
 * 一路到根目录仍未找到则返回 null。用于界定项目级配置的搜索边界。
 */
function findWorktreeRoot(startDir) {
  if (!startDir) return null;
  let current = path.resolve(startDir);

  while (true) {
    if (fs.existsSync(path.join(current, '.git'))) {
      return current;
    }
    const parent = path.dirname(current);
    if (parent === current) {
      return null;
    }
    current = parent;
  }
}

// ============== PROMPT FILE HELPERS ==============

/** 判断值是否为 “{file:路径}” 形式的 prompt 文件引用字符串（先 trim 再匹配）。 */
function isPromptFileReference(value) {
  if (typeof value !== 'string') {
    return false;
  }
  return PROMPT_FILE_PATTERN.test(value.trim());
}

/**
 * 把 “{file:路径}” 引用解析为实际文件绝对路径。
 *
 * “./x” 与其它相对路径均相对 OPENCODE_CONFIG_DIR 解析，绝对路径原样保留。
 * 不是合法引用、或捕获的路径为空时返回 null（调用方据此判定“非文件引用”）。
 */
function resolvePromptFilePath(reference) {
  const match = typeof reference === 'string' ? reference.trim().match(PROMPT_FILE_PATTERN) : null;
  if (!match) {
    return null;
  }
  let target = match[1].trim();
  if (!target) {
    return null;
  }

  if (target.startsWith('./')) {
    target = target.slice(2);
    target = path.join(OPENCODE_CONFIG_DIR, target);
  } else if (!path.isAbsolute(target)) {
    target = path.join(OPENCODE_CONFIG_DIR, target);
  }

  return target;
}

/**
 * 写入 prompt 正文文件：父目录不存在时递归创建，content 为
 * null / undefined 时写入空字符串；成功后打 log。
 */
function writePromptFile(filePath, content) {
  const dir = path.dirname(filePath);
  fs.mkdirSync(dir, { recursive: true });
  fs.writeFileSync(filePath, content ?? '', 'utf8');
  console.log(`Updated prompt file: ${filePath}`);
}

// ============== SKILL FILE OPERATIONS ==============

/**
 * 递归遍历 rootDir，收集所有名为 SKILL.md 的文件的绝对路径。
 * 目录不存在返回空数组；子目录读取失败（权限等）静默跳过该目录。
 */
function walkSkillMdFiles(rootDir) {
  if (!rootDir || !fs.existsSync(rootDir)) return [];

  const results = [];
  // 递归收集当前目录及子目录下的 SKILL.md（内部辅助函数）。
  const walk = (dir) => {
    let entries = [];
    try {
      entries = fs.readdirSync(dir, { withFileTypes: true });
    } catch {
      return;
    }

    for (const entry of entries) {
      const fullPath = path.join(dir, entry.name);
      if (entry.isDirectory()) {
        walk(fullPath);
        continue;
      }
      if (entry.isFile() && entry.name === 'SKILL.md') {
        results.push(fullPath);
      }
    }
  };

  walk(rootDir);
  return results;
}

/**
 * 解析一个 SKILL.md 并登记进 skillsMap（以 frontmatter.name 为键）。
 *
 * 解析抛错、或 name 缺失/为空串时直接跳过，不抛出错误——单个坏技能
 * 不应影响其余技能的发现。description 取自 frontmatter，缺省为空串。
 */
function addSkillFromMdFile(skillsMap, skillMdPath, scope, source) {
  let parsed;
  try {
    parsed = parseMdFile(skillMdPath);
  } catch {
    return;
  }

  const name = typeof parsed.frontmatter?.name === 'string'
    ? parsed.frontmatter.name.trim()
    : '';
  const description = typeof parsed.frontmatter?.description === 'string'
    ? parsed.frontmatter.description
    : '';

  if (!name) {
    return;
  }

  skillsMap.set(name, {
    name,
    path: skillMdPath,
    scope,
    source,
    description,
  });
}

/**
 * 计算技能发现需要扫描的目录列表（绝对路径、去重、按优先级排序）：
 * 用户配置目录 → 工作目录到 worktree 根每一级的 .opencode → ~/.opencode
 * → OPENCODE_CONFIG_DIR 环境变量指定的目录。
 * 靠前的目录优先级更高，调用方据此决定同名技能的覆盖关系。
 */
function resolveSkillSearchDirectories(workingDirectory) {
  const directories = [];
  // 去重地压入一个目录（内部辅助函数）。
  const pushDir = (dir) => {
    if (!dir) return;
    const resolved = path.resolve(dir);
    if (!directories.includes(resolved)) {
      directories.push(resolved);
    }
  };

  pushDir(OPENCODE_CONFIG_DIR);

  if (workingDirectory) {
    const worktreeRoot = findWorktreeRoot(workingDirectory) || path.resolve(workingDirectory);
    const projectDirs = getAncestors(workingDirectory, worktreeRoot)
      .map((dir) => path.join(dir, '.opencode'));
    projectDirs.forEach(pushDir);
  }

  pushDir(path.join(os.homedir(), '.opencode'));

  const customConfigDir = process.env.OPENCODE_CONFIG_DIR
    ? path.resolve(process.env.OPENCODE_CONFIG_DIR)
    : null;
  pushDir(customConfigDir);

  return directories;
}

/**
 * 递归列出技能目录中除 SKILL.md 外的全部支持文件。
 * 每项为 { name, path, fullPath }：文件名、相对技能目录的路径、绝对路径。
 * 目录不存在时返回空数组。
 */
function listSkillSupportingFiles(skillDir) {
  if (!fs.existsSync(skillDir)) {
    return [];
  }

  const files = [];

  // 深度优先遍历，把非 SKILL.md 的文件按相对路径收集进 files（内部辅助函数）。
  function walkDir(dir, relativePath = '') {
    const entries = fs.readdirSync(dir, { withFileTypes: true });
    for (const entry of entries) {
      const fullPath = path.join(dir, entry.name);
      const relPath = relativePath ? path.join(relativePath, entry.name) : entry.name;

      if (entry.isDirectory()) {
        walkDir(fullPath, relPath);
      } else if (entry.name !== 'SKILL.md') {
        files.push({
          name: entry.name,
          path: relPath,
          fullPath: fullPath
        });
      }
    }
  }

  walkDir(skillDir);
  return files;
}

/**
 * 校验 relativePath 解析后仍位于 skillDir 的真实路径之内，防止 “../” 等
 * 路径穿越逃出技能目录。越界时抛出 code 为 EACCES 的 'Access to file
 * denied'；校验通过时返回解析后的绝对路径供后续读写使用。
 */
function assertPathWithinSkillDir(skillDir, relativePath) {
  const root = fs.realpathSync(skillDir);
  const target = path.resolve(root, relativePath);
  const relative = path.relative(root, target);
  const isWithin = relative === '' || (!relative.startsWith('..') && !path.isAbsolute(relative));

  if (!isWithin) {
    const error = new Error('Access to file denied');
    error.code = 'EACCES';
    throw error;
  }

  return target;
}

/**
 * 读取技能目录内的支持文件内容（UTF-8）。路径先经 assertPathWithinSkillDir
 * 校验；文件不存在返回 null。
 */
function readSkillSupportingFile(skillDir, relativePath) {
  const fullPath = assertPathWithinSkillDir(skillDir, relativePath);
  if (!fs.existsSync(fullPath)) {
    return null;
  }
  return fs.readFileSync(fullPath, 'utf8');
}

/**
 * 写入（或覆盖）技能目录内的支持文件，父目录不存在时递归创建；
 * 路径同样受 assertPathWithinSkillDir 的越界校验保护。
 */
function writeSkillSupportingFile(skillDir, relativePath, content) {
  const fullPath = assertPathWithinSkillDir(skillDir, relativePath);
  const dir = path.dirname(fullPath);
  fs.mkdirSync(dir, { recursive: true });
  fs.writeFileSync(fullPath, content, 'utf8');
}

/**
 * 删除技能目录内的支持文件，并自下而上清理因删除而变空的父目录
 * （直到技能根目录为止，遇非空目录或异常即停）。文件不存在时静默跳过。
 */
function deleteSkillSupportingFile(skillDir, relativePath) {
  const root = fs.realpathSync(skillDir);
  const fullPath = assertPathWithinSkillDir(skillDir, relativePath);
  if (fs.existsSync(fullPath)) {
    fs.unlinkSync(fullPath);
    let parentDir = path.dirname(fullPath);
    while (parentDir !== root) {
      try {
        const entries = fs.readdirSync(parentDir);
        if (entries.length === 0) {
          fs.rmdirSync(parentDir);
          parentDir = path.dirname(parentDir);
        } else {
          break;
        }
      } catch {
        break;
      }
    }
  }
}

export {
  OPENCODE_CONFIG_DIR,
  AGENT_DIR,
  COMMAND_DIR,
  SKILL_DIR,
  CONFIG_FILE,
  AGENT_SCOPE,
  COMMAND_SCOPE,
  SKILL_SCOPE,
  ensureDirs,
  parseMdFile,
  writeMdFile,
  readConfigFile,
  readConfigLayer,
  isPlainObject,
  readConfigLayers,
  readConfig,
  getConfigForPath,
  writeConfig,
  getJsonEntrySource,
  getJsonWriteTarget,
  getAncestors,
  findWorktreeRoot,
  isPromptFileReference,
  resolvePromptFilePath,
  writePromptFile,
  walkSkillMdFiles,
  addSkillFromMdFile,
  resolveSkillSearchDirectories,
  listSkillSupportingFiles,
  readSkillSupportingFile,
  writeSkillSupportingFile,
  deleteSkillSupportingFile,
};
