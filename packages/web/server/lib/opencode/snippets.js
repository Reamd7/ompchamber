/**
 * OpenCode snippet（提示词片段）的数据层与 #hashtag 展开引擎。
 *
 * snippet 以 Markdown 文件存储：项目层位于 .opencode/snippets（兼容 snippet），
 * 全局层位于 ~/.config/opencode/snippets（兼容 snippet），加载顺序为全局在前、
 * 项目在后 —— 后注册的项目层会覆盖同名全局 snippet。每个 snippet 由可选的
 * YAML frontmatter（aliases/description）与正文组成，正文可用 <prepend>、
 * <append>、<inject> 块控制展开位置。对外提供 snippet 的 CRUD 与
 * expandSnippets（把文本中的 #name 引用递归展开为片段正文，含防循环上限）。
 */
import fs from 'fs';
import path from 'path';
import os from 'os';
import yaml from 'yaml';

/** OpenCode 全局配置目录 ~/.config/opencode。 */
const OPENCODE_CONFIG_DIR = path.join(os.homedir(), '.config', 'opencode');
/** 全局 snippet 目录（单数形式，当前首选）。 */
const GLOBAL_SNIPPET_DIR = path.join(OPENCODE_CONFIG_DIR, 'snippet');
/** 全局 snippet 目录（复数形式，兼容旧路径）。 */
const GLOBAL_SNIPPET_DIR_ALT = path.join(OPENCODE_CONFIG_DIR, 'snippets');
/** snippet 文件的扩展名。 */
const SNIPPET_EXTENSION = '.md';
/** snippet 名称与别名的合法格式：字母/数字开头，仅含字母、数字、连字符与下划线，最长 80 字符（忽略大小写）。 */
const SNIPPET_NAME_PATTERN = /^[a-z0-9][a-z0-9_-]{0,79}$/i;
/** 从正文中提取 #hashtag 引用的全局正则；跨调用复用，使用前必须重置 lastIndex。 */
const HASHTAG_PATTERN = /#([a-z0-9_-]+)/gi;
/** 单个 snippet 在一次展开中的最大展开次数，防止自引用或互相引用造成死循环。 */
const MAX_EXPANSION_COUNT = 15;

/**
 * 返回项目层 snippet 目录候选（.opencode/snippets 优先于 .opencode/snippet）；
 * 未提供 workingDirectory 时返回空数组。
 */
function getProjectSnippetDirs(workingDirectory) {
  if (!workingDirectory) return [];
  return [
    path.join(workingDirectory, '.opencode', 'snippets'),
    path.join(workingDirectory, '.opencode', 'snippet'),
  ];
}

/** 返回全局 snippet 目录候选（复数形式优先于单数形式，与写入路径的选择保持一致）。 */
function getGlobalSnippetDirs() {
  return [GLOBAL_SNIPPET_DIR_ALT, GLOBAL_SNIPPET_DIR];
}

/**
 * 汇总加载目录列表：全局目录在前、项目目录在后，并标注各自的 source
 * （global/project）；该顺序即覆盖优先级。
 */
function getLoadDirs(workingDirectory) {
  return [
    ...getGlobalSnippetDirs().map((dir) => ({ dir, source: 'global' })),
    ...getProjectSnippetDirs(workingDirectory).map((dir) => ({ dir, source: 'project' })),
  ];
}

/** 断言名称（或别名）符合 SNIPPET_NAME_PATTERN，不合法时抛出 Error。 */
function assertValidSnippetName(name) {
  if (typeof name !== 'string' || !SNIPPET_NAME_PATTERN.test(name)) {
    throw new Error('Snippet name must use letters, numbers, dashes, or underscores');
  }
}

/**
 * 读取并解析 snippet Markdown 文件：若以 YAML frontmatter（--- 分隔）开头，
 * 则解析出 frontmatter 对象与正文；否则整个内容作为正文、frontmatter 为空对象。
 */
function parseMarkdownFile(filePath) {
  const content = fs.readFileSync(filePath, 'utf8');
  const match = content.match(/^---\r?\n([\s\S]*?)\r?\n---\r?\n?([\s\S]*)$/);
  if (!match) {
    return { frontmatter: {}, body: content.trim() };
  }
  return {
    frontmatter: yaml.parse(match[1]) || {},
    body: match[2].trim(),
  };
}

/** 从 frontmatter 提取别名列表：兼容 aliases（数组或单值）与 alias 键，trim 后过滤空值。 */
function normalizeAliases(frontmatter) {
  const raw = frontmatter.aliases ?? frontmatter.alias;
  if (!raw) return [];
  const aliases = Array.isArray(raw) ? raw : [raw];
  return aliases.map((alias) => String(alias).trim()).filter(Boolean);
}

/**
 * 将 snippet 写入 Markdown 文件：把非空 aliases 与 description 序列化为
 * YAML frontmatter 再拼接正文，无 frontmatter 字段时只写正文；自动创建父目录。
 */
function writeMarkdownFile(filePath, { content, aliases = [], description }) {
  const frontmatter = {};
  const normalizedAliases = aliases.map((alias) => String(alias).trim()).filter(Boolean);
  if (normalizedAliases.length > 0) frontmatter.aliases = normalizedAliases;
  if (description?.trim()) frontmatter.description = description.trim();

  const body = content ?? '';
  const output = Object.keys(frontmatter).length > 0
    ? `---\n${yaml.stringify(frontmatter)}---\n${body ? `\n${body}` : ''}`
    : body;

  fs.mkdirSync(path.dirname(filePath), { recursive: true });
  fs.writeFileSync(filePath, output, 'utf8');
}

/**
 * 加载单个 snippet 文件：文件名（去掉扩展名）必须符合命名规则，否则返回
 * null；解析 frontmatter 与正文，返回含 name、content、aliases、
 * description、filePath 与 source 的 snippet 对象。
 */
function loadSnippetFile(dir, filename, source) {
  const name = path.basename(filename, SNIPPET_EXTENSION);
  if (!SNIPPET_NAME_PATTERN.test(name)) return null;
  const filePath = path.join(dir, filename);
  const { frontmatter, body } = parseMarkdownFile(filePath);
  return {
    name,
    content: body,
    aliases: normalizeAliases(frontmatter),
    description: typeof frontmatter.description === 'string' ? frontmatter.description : undefined,
    filePath,
    source,
  };
}

/**
 * 从注册表中移除 snippet 的别名键：跳过与正式名称冲突的键，
 * 仅删除仍指向该 snippet 的条目。
 */
function removeSnippetAliases(registry, snippet, canonicalNames) {
  for (const alias of snippet.aliases) {
    const aliasKey = alias.toLowerCase();
    if (canonicalNames.has(aliasKey)) continue;
    if (registry.get(aliasKey) === snippet) registry.delete(aliasKey);
  }
}

/**
 * 将 snippet 注册进查找表：以小写名称与别名为键指向同一 snippet；
 * 覆盖同名已有 snippet 前先清掉其别名。正式名称优先于别名，非法别名不注册。
 */
function registerSnippet(registry, snippet, canonicalNames) {
  const key = snippet.name.toLowerCase();
  const existing = registry.get(key);
  if (existing?.name.toLowerCase() === key) {
    removeSnippetAliases(registry, existing, canonicalNames);
  }
  registry.set(key, snippet);
  canonicalNames.add(key);
  for (const alias of snippet.aliases) {
    const aliasKey = alias.toLowerCase();
    if (SNIPPET_NAME_PATTERN.test(alias) && !canonicalNames.has(aliasKey)) registry.set(aliasKey, snippet);
  }
}

/**
 * 扫描全部加载目录（全局 + 项目），构建「小写名称/别名 -> snippet」查找表；
 * 单个文件解析失败仅告警并跳过，不影响其余 snippet 的加载。
 */
function loadSnippetRegistry(workingDirectory) {
  const registry = new Map();
  const canonicalNames = new Set();
  for (const { dir, source } of getLoadDirs(workingDirectory)) {
    if (!fs.existsSync(dir)) continue;
    for (const filename of fs.readdirSync(dir)) {
      if (!filename.endsWith(SNIPPET_EXTENSION)) continue;
      try {
        const snippet = loadSnippetFile(dir, filename, source);
        if (snippet) registerSnippet(registry, snippet, canonicalNames);
      } catch (error) {
        console.warn(`[Snippets] Failed to load ${path.join(dir, filename)}:`, error);
      }
    }
  }
  return registry;
}

/** 将注册表按 snippet 去重（名称与别名可指向同一对象）并按名称字母序排序返回。 */
function listUniqueSnippets(registry) {
  const seen = new Set();
  const snippets = [];
  for (const snippet of registry.values()) {
    const key = `${snippet.source}:${snippet.filePath}`;
    if (seen.has(key)) continue;
    seen.add(key);
    snippets.push(snippet);
  }
  return snippets.sort((a, b) => a.name.localeCompare(b.name));
}

/**
 * 解析写入目录：scope 为 project 时要求 workingDirectory，并在新旧两种目录
 * 之间优先复用已存在者；全局 scope 同理在两个全局目录间选择。
 */
function getWritableSnippetDir(scope, workingDirectory) {
  if (scope === 'project') {
    if (!workingDirectory) throw new Error('Project directory is required for project snippets');
    const preferred = path.join(workingDirectory, '.opencode', 'snippet');
    const alternate = path.join(workingDirectory, '.opencode', 'snippets');
    return fs.existsSync(alternate) && !fs.existsSync(preferred) ? alternate : preferred;
  }
  return fs.existsSync(GLOBAL_SNIPPET_DIR_ALT) && !fs.existsSync(GLOBAL_SNIPPET_DIR)
    ? GLOBAL_SNIPPET_DIR_ALT
    : GLOBAL_SNIPPET_DIR;
}

/** 按名称或别名（大小写不敏感）查找 snippet；名称非法时抛错，未找到返回 null。 */
function findSnippetByName(name, workingDirectory) {
  assertValidSnippetName(name);
  const registry = loadSnippetRegistry(workingDirectory);
  return registry.get(name.toLowerCase()) ?? null;
}

/**
 * 解析 snippet 正文中的定位块：<prepend> 与 <append> 块的内容分别归入对应
 * 数组并从正文移除（未闭合的块取到结尾）；<inject> 块整体丢弃；
 * 返回剩余的 inline 正文与两个块数组。
 */
function parseSnippetBlocks(content) {
  const blocks = { prepend: [], append: [] };
  let inline = content;
  for (const type of ['prepend', 'append']) {
    const regex = new RegExp(`<${type}>([\\s\\S]*?)(?:<\\/${type}>|$)`, 'gi');
    inline = inline.replace(regex, (_match, value) => {
      const normalized = String(value).trim();
      if (normalized) blocks[type].push(normalized);
      return '';
    });
  }
  inline = inline.replace(/<inject>[\s\S]*?(?:<\/inject>|$)/gi, '').trim();
  return { inline, prepend: blocks.prepend, append: blocks.append };
}

/**
 * 递归展开文本中的 #hashtag 引用：循环扫描直到无变化或检测到循环。
 * 命中 snippet 时按 MAX_EXPANSION_COUNT 限制单片段展开次数；#skill( 形式
 * （hashtag 后紧跟左括号）不视为 snippet 引用。prepend/append 块递归展开后
 * 收集进 collector，inline 部分替换原位置并参与后续扫描。
 */
function expandText(text, registry, expansionCounts, collector) {
  let expanded = text;
  let changed = true;

  while (changed) {
    const previous = expanded;
    let loopDetected = false;
    HASHTAG_PATTERN.lastIndex = 0;

    expanded = expanded.replace(HASHTAG_PATTERN, (match, name, offset, input) => {
      if (name.toLowerCase() === 'skill' && input[offset + match.length] === '(') return match;
      const snippet = registry.get(name.toLowerCase());
      if (!snippet) return match;

      const key = snippet.name.toLowerCase();
      const count = (expansionCounts.get(key) || 0) + 1;
      if (count > MAX_EXPANSION_COUNT) {
        loopDetected = true;
        return match;
      }
      expansionCounts.set(key, count);

      const parsed = parseSnippetBlocks(snippet.content);
      for (const block of parsed.prepend) collector.prepend.push(expandText(block, registry, expansionCounts, collector));
      for (const block of parsed.append) collector.append.push(expandText(block, registry, expansionCounts, collector));
      return expandText(parsed.inline, registry, expansionCounts, collector);
    });

    changed = expanded !== previous && !loopDetected;
  }

  return expanded;
}

/** 列出全部 snippet（全局 + 项目层合并、去重、按名称排序）。 */
export function listSnippets(workingDirectory) {
  return listUniqueSnippets(loadSnippetRegistry(workingDirectory));
}

/** 按名称或别名获取单个 snippet，未找到返回 null。 */
export function getSnippet(name, workingDirectory) {
  return findSnippetByName(name, workingDirectory);
}

/**
 * 创建 snippet：校验名称并解析写入目录，同名文件已存在时抛错；写入后重新
 * 加载并返回新 snippet 对象。scope 默认 global，可选 project。
 */
export function createSnippet(name, config, workingDirectory, scope = 'global') {
  assertValidSnippetName(name);
  const dir = getWritableSnippetDir(scope, workingDirectory);
  const filePath = path.join(dir, `${name}${SNIPPET_EXTENSION}`);
  if (fs.existsSync(filePath)) throw new Error(`Snippet "${name}" already exists`);
  writeMarkdownFile(filePath, config || {});
  return getSnippet(name, workingDirectory);
}

/**
 * 就地更新已存在的 snippet：以现有内容为基础合并 updates 后整体重写文件
 * （未提供的字段沿用原值）；未找到时抛错。返回更新后的 snippet。
 */
export function updateSnippet(name, updates, workingDirectory) {
  const existing = findSnippetByName(name, workingDirectory);
  if (!existing) throw new Error(`Snippet "${name}" not found`);
  writeMarkdownFile(existing.filePath, { ...existing, ...(updates || {}) });
  return getSnippet(name, workingDirectory);
}

/** 删除 snippet 对应的 Markdown 文件；未找到时抛错。 */
export function deleteSnippet(name, workingDirectory) {
  const existing = findSnippetByName(name, workingDirectory);
  if (!existing) throw new Error(`Snippet "${name}" not found`);
  fs.unlinkSync(existing.filePath);
}

/**
 * 展开文本中的全部 snippet 引用：构建注册表后从入口展开，最终按
 * 「prepend 块 + 展开后正文 + append 块」的顺序用空行拼接返回。
 */
export function expandSnippets(text, workingDirectory) {
  const registry = loadSnippetRegistry(workingDirectory);
  const collector = { prepend: [], append: [] };
  const expanded = expandText(text || '', registry, new Map(), collector).trim();
  return [...collector.prepend, expanded, ...collector.append].filter(Boolean).join('\n\n');
}
