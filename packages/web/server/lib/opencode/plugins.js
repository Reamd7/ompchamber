/**
 * OpenCode 插件资产的存取模块。
 *
 * 管理两类插件：写入 opencode 配置 plugin 数组的条目（npm 包名或本地
 * 路径 spec，可带 options）以及直接放在 plugins 目录下的源码文件。
 * 配置按 custom（OPENCODE_CONFIG 环境变量）优先于 user
 * （~/.config/opencode），再叠加 project（工作目录 .opencode/）分层
 * 读取；插件 id 用 base64url 编码 “前缀:scope:值”，可安全放进 URL。
 * 校验失败统一抛带业务 code 的 Error（INVALID_SCOPE / INVALID_SPEC /
 * INVALID_FILENAME / ENTRY_EXISTS / FILE_EXISTS / NOT_FOUND），由
 * plugin-routes.js 映射为 HTTP 状态码。
 */
import fs from 'fs';
import os from 'os';
import path from 'path';
import {
  AGENT_SCOPE,
  readConfigFile,
  readConfigLayer,
  writeConfig,
} from './shared.js';
import { isPathSpec } from './plugin-spec.js';

/** 插件文件名白名单：小写字母或数字开头，仅含 a-z0-9-_. 且以 js/ts/mjs/cjs 结尾。 */
const PLUGIN_FILE_NAME_PATTERN = /^[a-z0-9][a-z0-9-_.]*\.(js|ts|mjs|cjs)$/;

/**
 * 类型定义补充说明（原始英文定义见下方块）：PluginScope 为条目作用域；
 * PluginParsedKind 为 spec 解析结果（npm 包或本地路径）；PluginEntry
 * 为配置内插件条目；PluginFile 为 plugins 目录下的源码文件。字段含义
 * 见各 @property 英文注释。
 */
/**
 * @typedef {'user' | 'project'} PluginScope
 * @typedef {'npm' | 'path'} PluginParsedKind
 * @typedef {Object} PluginEntry
 * @property {string} id base64url encoded "config:scope:spec"
 * @property {string} spec
 * @property {Record<string, unknown>} [options]
 * @property {PluginScope} scope
 * @property {'config'} kind
 * @property {PluginParsedKind} parsedKind
 * @property {string} sourcePath absolute path to the config file
 * @typedef {Object} PluginFile
 * @property {string} id base64url encoded "file:scope:fileName"
 * @property {string} fileName
 * @property {PluginScope} scope
 * @property {'file'} kind
 * @property {string} absolutePath
 */

/** 构造带业务 code 的 Error，供路由层映射 HTTP 状态码。 */
function codedError(message, code) {
  const error = new Error(message);
  error.code = code;
  return error;
}

/** 校验 scope 只能是 user 或 project，否则抛 INVALID_SCOPE。 */
function validateScope(scope) {
  if (scope !== AGENT_SCOPE.USER && scope !== AGENT_SCOPE.PROJECT) {
    throw codedError('Plugin scope must be user or project', 'INVALID_SCOPE');
  }
}

/**
 * 校验插件 spec：必须是非空字符串且不含 null 字节（防注入），返回
 * trim 后的 spec；违规抛 INVALID_SPEC。
 */
function validatePluginSpec(spec) {
  if (typeof spec !== 'string' || !spec.trim()) {
    throw codedError('Plugin spec must be a non-empty string', 'INVALID_SPEC');
  }
  if (spec.includes('\0')) {
    throw codedError('Plugin spec cannot contain null bytes', 'INVALID_SPEC');
  }
  return spec.trim();
}

/** 判断值是否为非数组的纯对象。 */
function isRecord(value) {
  return Boolean(value) && typeof value === 'object' && !Array.isArray(value);
}

/** 判断 options 是否为含至少一个键的纯对象。 */
function hasOptions(options) {
  return isRecord(options) && Object.keys(options).length > 0;
}

/**
 * 判定 spec 类型：isPathSpec 认定为本地路径（path），否则视为 npm
 * 包（npm）。不能用 path.sep 判断，scoped npm 包也含斜杠（见函数体
 * 内英文注释）。
 */
function parsedKindForSpec(spec) {
  // Path indicators must include Windows paths; scoped npm packages also contain '/'.
  // Do NOT use `includes(path.sep)` — scoped npm packages legitimately contain '/' (e.g. `@gitlab/opencode-gitlab-auth`).
  return isPathSpec(spec) ? 'path' : 'npm';
}

/**
 * 当前生效的 OpenCode 配置目录：设置了 OPENCODE_CONFIG 时取其所在
 * 目录，否则默认 ~/.config/opencode。
 */
function getActiveOpencodeConfigDir() {
  const customConfigPath = process.env.OPENCODE_CONFIG;
  if (customConfigPath) {
    return path.dirname(path.resolve(customConfigPath));
  }
  return path.join(os.homedir(), '.config', 'opencode');
}

/** 用户级配置文件候选路径列表（config.json / opencode.json / opencode.jsonc）。 */
function getActiveUserConfigPaths() {
  const configDir = getActiveOpencodeConfigDir();
  return [
    path.join(configDir, 'config.json'),
    path.join(configDir, 'opencode.json'),
    path.join(configDir, 'opencode.jsonc'),
  ];
}

/** OPENCODE_CONFIG 指向的自定义配置文件绝对路径，未设置时为 null。 */
function getActiveCustomConfigPath() {
  return process.env.OPENCODE_CONFIG ? path.resolve(process.env.OPENCODE_CONFIG) : null;
}

/**
 * 主用户配置路径：候选中第一个实际存在的文件；都不存在时返回默认
 * 的 config.json（作为后续创建目标）。
 */
function getPrimaryUserConfigPath() {
  const [defaultPath, ...fallbackPaths] = getActiveUserConfigPaths();
  for (const userPath of [defaultPath, ...fallbackPaths]) {
    if (fs.existsSync(userPath)) {
      return userPath;
    }
  }
  return defaultPath;
}

/**
 * 项目级配置路径：工作目录下四个候选（opencode.json/jsonc 及
 * .opencode/ 目录内同名文件）中第一个存在的；都不存在时返回首个
 * 候选。无工作目录返回 null。
 */
function getProjectConfigPath(workingDirectory) {
  if (!workingDirectory) return null;
  const candidates = [
    path.join(workingDirectory, 'opencode.json'),
    path.join(workingDirectory, 'opencode.jsonc'),
    path.join(workingDirectory, '.opencode', 'opencode.json'),
    path.join(workingDirectory, '.opencode', 'opencode.jsonc'),
  ];
  return candidates.find((candidate) => fs.existsSync(candidate)) || candidates[0];
}

/**
 * 读取三层插件配置（user / project / custom）：返回各层 config 对象、
 * 实际生效路径，以及 layerErrors（读取或 JSONC 解析失败项，含 path /
 * code / message；不存在的层不产生错误）。custom 层存在时优先于
 * user 层参与后续处理。
 */
function readPluginConfigLayers(workingDirectory) {
  const customPath = getActiveCustomConfigPath();
  const userPath = getPrimaryUserConfigPath();
  const projectPath = getProjectConfigPath(workingDirectory);
  const userLayer = readConfigLayer(userPath);
  const projectLayer = readConfigLayer(projectPath);
  const customLayer = readConfigLayer(customPath);
  return {
    userConfig: userLayer.config,
    projectConfig: projectLayer.config,
    customConfig: customLayer.config,
    paths: {
      userPath,
      projectPath,
      customPath,
    },
    layerErrors: [
      userLayer.error && { path: userPath, code: userLayer.error.code, message: userLayer.error.message },
      projectLayer.error && projectPath && { path: projectPath, code: projectLayer.error.code, message: projectLayer.error.message },
      customLayer.error && customPath && { path: customPath, code: customLayer.error.code, message: customLayer.error.message },
    ].filter(Boolean),
  };
}

/**
 * 校验插件文件名：必须匹配白名单正则且不含路径分隔符或 “..”（防
 * 目录穿越），违规抛 INVALID_FILENAME；通过时原样返回。
 */
function validateFileName(fileName) {
  if (typeof fileName !== 'string' || !fileName) {
    throw codedError('Plugin file name is required', 'INVALID_FILENAME');
  }
  if (fileName.includes('/') || fileName.includes('\\') || fileName.includes('..') || !PLUGIN_FILE_NAME_PATTERN.test(fileName)) {
    throw codedError('Plugin file name must match /^[a-z0-9][a-z0-9-_.]*\\.(js|ts|mjs|cjs)$/ and cannot contain path traversal', 'INVALID_FILENAME');
  }
  return fileName;
}

/**
 * 确保 <工作目录>/.opencode 目录存在（按需递归创建）并返回其中
 * opencode.json 的路径；无工作目录抛 INVALID_SCOPE。
 */
function ensureProjectConfigPath(workingDirectory) {
  if (!workingDirectory) {
    throw codedError('Project scope requires working directory', 'INVALID_SCOPE');
  }
  const configDir = path.join(workingDirectory, '.opencode');
  fs.mkdirSync(configDir, { recursive: true });
  return path.join(configDir, 'opencode.json');
}

/**
 * 把分层读取结果压平为待处理来源列表：custom 存在时以 custom 代表
 * user 作用域（同一 scope 只取一层），project 层存在时追加为第二来源。
 */
function configSources(layers) {
  const sources = [];
  if (layers.paths.customPath) {
    sources.push({ config: layers.customConfig, filePath: layers.paths.customPath, scope: AGENT_SCOPE.USER });
  } else {
    sources.push({ config: layers.userConfig, filePath: layers.paths.userPath, scope: AGENT_SCOPE.USER });
  }
  if (layers.paths.projectPath) {
    sources.push({ config: layers.projectConfig, filePath: layers.paths.projectPath, scope: AGENT_SCOPE.PROJECT });
  }
  return sources;
}

/**
 * 拆开 “scope:value” 形式的 id 内部值；缺少冒号分隔符抛 INVALID_SPEC。
 */
function splitScopedValue(value) {
  const separator = value.indexOf(':');
  if (separator === -1) {
    throw codedError('Plugin id value must include scope', 'INVALID_SPEC');
  }
  return {
    scope: value.slice(0, separator),
    value: value.slice(separator + 1),
  };
}

/**
 * 按 config 前缀的 id 定位插件条目：解码并校验前缀/scope/spec 后，
 * 在对应作用域层的 plugin 数组里查找相同 spec 的条目。返回
 * { source, plugin, index }（source.config 修改后可直接回写），层缺失
 * 或找不到条目时返回 null。
 */
function getPluginTarget(id, workingDirectory) {
  const decoded = decodePluginId(id);
  if (decoded.prefix !== 'config') {
    throw codedError('Plugin entry id must use config prefix', 'INVALID_SPEC');
  }
  const { scope, value: spec } = splitScopedValue(decoded.value);
  validateScope(scope);
  const layers = readPluginConfigLayers(workingDirectory);
  const source = configSources(layers).find((candidate) => candidate.scope === scope);
  const plugin = Array.isArray(source?.config?.plugin) ? source.config.plugin : [];
  const index = plugin.findIndex((raw) => parsePluginRaw(raw).spec === spec);
  if (!source || index === -1) {
    return null;
  }
  return { source, plugin, index };
}

/**
 * 插件源码文件目录：project 作用域为 <工作目录>/.opencode/plugins
 * （无工作目录抛 INVALID_SCOPE），user 作用域为当前生效配置目录下的
 * plugins。
 */
function pluginDirForScope(scope, workingDirectory) {
  validateScope(scope);
  if (scope === AGENT_SCOPE.PROJECT) {
    if (!workingDirectory) {
      throw codedError('Project scope requires working directory', 'INVALID_SCOPE');
    }
    return path.join(workingDirectory, '.opencode', 'plugins');
  }
  return path.join(getActiveOpencodeConfigDir(), 'plugins');
}

/**
 * 把 file 前缀的 id 解析为 { fileName, scope, absolutePath }：依次校验
 * 前缀、scope 与文件名合法性，再拼出 plugins 目录下的绝对路径。
 */
function fileTargetFromId(id, workingDirectory) {
  const decoded = decodePluginId(id);
  if (decoded.prefix !== 'file') {
    throw codedError('Plugin file id must use file prefix', 'INVALID_FILENAME');
  }
  const { scope, value: fileName } = splitScopedValue(decoded.value);
  validateScope(scope);
  validateFileName(fileName);
  return {
    fileName,
    scope,
    absolutePath: path.join(pluginDirForScope(scope, workingDirectory), fileName),
  };
}

/** 把 “prefix:value” 编码为 base64url 字符串，得到 URL 安全的插件 id。 */
function encodePluginId(prefix, value) {
  return Buffer.from(`${prefix}:${value}`).toString('base64url');
}

/**
 * 解码 base64url 插件 id 为 { prefix, value }；解码结果中没有冒号
 * 分隔符时抛 INVALID_SPEC。
 */
function decodePluginId(id) {
  const decoded = Buffer.from(id, 'base64url').toString('utf8');
  const separator = decoded.indexOf(':');
  if (separator === -1) {
    throw codedError('Invalid plugin id', 'INVALID_SPEC');
  }
  return { prefix: decoded.slice(0, separator), value: decoded.slice(separator + 1) };
}

/**
 * 解析配置文件 plugin 数组中的原始项：字符串视为纯 spec；[spec,
 * options] 二元组（第二项须为纯对象）视为带选项条目；其余形态抛
 * INVALID_SPEC。返回 { spec, options? }。
 */
function parsePluginRaw(raw) {
  if (typeof raw === 'string') {
    return { spec: validatePluginSpec(raw) };
  }
  if (Array.isArray(raw) && raw.length === 2 && isRecord(raw[1])) {
    return { spec: validatePluginSpec(raw[0]), options: { ...raw[1] } };
  }
  throw codedError('Plugin spec must be a string or [string, object]', 'INVALID_SPEC');
}

/**
 * 把条目序列化回配置文件存储形态：带有效 options 时为
 * [spec, options] 数组，否则为纯字符串 spec。
 */
function serializePluginEntry(entry) {
  const spec = validatePluginSpec(entry?.spec);
  if (hasOptions(entry?.options)) {
    return [spec, { ...entry.options }];
  }
  return spec;
}

/**
 * 列出所有层中的插件条目（custom 优先于 user，再叠加 project 层），
 * 每条附 id / scope / kind / parsedKind / sourcePath，便于前端区分
 * 条目来源与类型。
 */
function listPluginEntries(workingDirectory) {
  const layers = readPluginConfigLayers(workingDirectory);
  return configSources(layers).flatMap((source) => {
    if (!Array.isArray(source.config?.plugin)) {
      return [];
    }
    return source.config.plugin.map((raw) => {
      const parsed = parsePluginRaw(raw);
      return {
        id: encodePluginId('config', `${source.scope}:${parsed.spec}`),
        spec: parsed.spec,
        ...(parsed.options !== undefined ? { options: parsed.options } : {}),
        scope: source.scope,
        kind: 'config',
        parsedKind: parsedKindForSpec(parsed.spec),
        sourcePath: source.filePath,
      };
    });
  });
}

/** 按 id 查找单个插件条目，找不到返回 null。 */
function getPluginEntry(id, workingDirectory) {
  return listPluginEntries(workingDirectory).find((entry) => entry.id === id) || null;
}

/**
 * 新建插件条目：校验 spec/scope 后查重（同作用域同 spec 抛
 * ENTRY_EXISTS），把序列化结果 push 进目标配置的 plugin 数组并写回。
 * user 作用域写入 custom（若设置）或主 user 配置；project 作用域先
 * 确保 .opencode/opencode.json 存在。
 */
function createPluginEntry(entry, workingDirectory) {
  const spec = validatePluginSpec(entry?.spec);
  const scope = entry?.scope || AGENT_SCOPE.USER;
  validateScope(scope);

  const layers = readPluginConfigLayers(workingDirectory);
  const existing = configSources(layers).find((source) => (
    source.scope === scope
    && Array.isArray(source.config?.plugin)
    && source.config.plugin.some((raw) => parsePluginRaw(raw).spec === spec)
  ));
  if (existing) {
    throw codedError(`Plugin "${spec}" already exists`, 'ENTRY_EXISTS');
  }

  let targetPath = getPrimaryUserConfigPath();
  let config = {};
  if (scope === AGENT_SCOPE.PROJECT) {
    targetPath = ensureProjectConfigPath(workingDirectory);
    config = fs.existsSync(targetPath) ? readConfigFile(targetPath) : {};
  } else {
    targetPath = layers.paths.customPath || layers.paths.userPath;
    config = layers.paths.customPath ? layers.customConfig : layers.userConfig;
  }

  if (!Array.isArray(config.plugin)) {
    config.plugin = [];
  }
  config.plugin.push(serializePluginEntry({ spec, options: entry.options }));
  writeConfig(config, targetPath);
}

/**
 * 按 id 原位更新条目：updates 中未提供的字段沿用现有值（spec 与
 * options 分别处理），重新序列化后写回条目所在层的配置文件；id 不
 * 存在抛 NOT_FOUND。
 */
function updatePluginEntry(id, updates, workingDirectory) {
  const target = getPluginTarget(id, workingDirectory);
  if (!target) {
    throw codedError('Plugin entry not found', 'NOT_FOUND');
  }
  const existing = parsePluginRaw(target.plugin[target.index]);
  const nextSpec = updates?.spec === undefined ? existing.spec : validatePluginSpec(updates.spec);
  const nextOptions = updates?.options === undefined ? existing.options : updates.options;
  target.plugin[target.index] = serializePluginEntry({ spec: nextSpec, options: nextOptions });
  writeConfig(target.source.config, target.source.filePath);
}

/**
 * 按 id 删除条目并写回配置；删除后 plugin 数组为空时连 “plugin” 键
 * 一起移除，避免留空数组；id 不存在抛 NOT_FOUND。
 */
function deletePluginEntry(id, workingDirectory) {
  const target = getPluginTarget(id, workingDirectory);
  if (!target) {
    throw codedError('Plugin entry not found', 'NOT_FOUND');
  }
  target.plugin.splice(target.index, 1);
  if (target.plugin.length === 0) {
    delete target.source.config.plugin;
  }
  writeConfig(target.source.config, target.source.filePath);
}

/**
 * 列出 plugins 目录（user 作用域，提供工作目录时含 project 作用域）
 * 下所有匹配文件名白名单的源码文件，附 id / fileName / scope / kind /
 * absolutePath。
 */
function listPluginDirFiles(workingDirectory) {
  const scopes = [AGENT_SCOPE.USER];
  if (workingDirectory) {
    scopes.push(AGENT_SCOPE.PROJECT);
  }
  return scopes.flatMap((scope) => {
    const dir = pluginDirForScope(scope, workingDirectory);
    if (!fs.existsSync(dir)) {
      return [];
    }
    return fs.readdirSync(dir, { withFileTypes: true })
      .filter((entry) => entry.isFile() && PLUGIN_FILE_NAME_PATTERN.test(entry.name) && !entry.name.includes('..'))
      .map((entry) => ({
        id: encodePluginId('file', `${scope}:${entry.name}`),
        fileName: entry.name,
        scope,
        kind: 'file',
        absolutePath: path.join(dir, entry.name),
      }));
  });
}

/** 读取单个插件源码文件内容；文件不存在返回 null。 */
function readPluginDirFile(id, workingDirectory) {
  const target = fileTargetFromId(id, workingDirectory);
  if (!fs.existsSync(target.absolutePath)) {
    return null;
  }
  return {
    fileName: target.fileName,
    scope: target.scope,
    content: fs.readFileSync(target.absolutePath, 'utf8'),
  };
}

/**
 * 写入插件源码文件：校验文件名与 scope 后，默认拒绝覆盖已存在文件
 * （opts.overwrite 为 true 时允许），按需创建目录并写入
 * file.content（缺省为空串）；重名抛 FILE_EXISTS。
 */
function writePluginDirFile(file, workingDirectory, opts = {}) {
  const fileName = validateFileName(file?.fileName);
  const scope = file?.scope || AGENT_SCOPE.USER;
  validateScope(scope);
  const dir = pluginDirForScope(scope, workingDirectory);
  const absolutePath = path.join(dir, fileName);
  if (!opts.overwrite && fs.existsSync(absolutePath)) {
    throw codedError(`Plugin file "${fileName}" already exists`, 'FILE_EXISTS');
  }
  fs.mkdirSync(dir, { recursive: true });
  fs.writeFileSync(absolutePath, file?.content ?? '', 'utf8');
}

/** 删除插件源码文件；文件不存在抛 NOT_FOUND。 */
function deletePluginDirFile(id, workingDirectory) {
  const target = fileTargetFromId(id, workingDirectory);
  if (!fs.existsSync(target.absolutePath)) {
    throw codedError(`Plugin file "${target.fileName}" not found`, 'NOT_FOUND');
  }
  fs.unlinkSync(target.absolutePath);
}

// 导出条目与文件两套 CRUD，以及 id 编解码与序列化原语，供 plugin-routes.js 使用。
export {
  listPluginEntries,
  getPluginEntry,
  createPluginEntry,
  updatePluginEntry,
  deletePluginEntry,
  listPluginDirFiles,
  readPluginDirFile,
  writePluginDirFile,
  deletePluginDirFile,
  encodePluginId,
  decodePluginId,
  parsePluginRaw,
  serializePluginEntry,
};
