/**
 * 会话 Markdown 图片授权路由。
 *
 * 远程（非本机）UI 客户端无法直接读取本地文件，本模块为助手消息中引用的图片
 * 换发访问授权：解析助手消息里的 Markdown 图片语法（内联与引用定义两种形式），
 * 以助手文本为授权依据（客户端不能为消息未引用的路径换取授权），再校验目标文件
 * 位于工作区或经批准的 OpenCode 临时目录内、大小与魔数符合受支持图片格式；
 * 工作区外的文件复用 fs/routes.js 的路径绑定授权（mintOutsideFileGrant）下发。
 */
import express from 'express';
import { constants as fsConstants } from 'node:fs';
import { mintOutsideFileGrant } from '../fs/routes.js';

/** 单张图片允许的最大字节数（10 MiB），超过即拒绝授权。 */
const MAX_IMAGE_BYTES = 10 * 1024 * 1024;
/** 单次请求最多允许申请授权的图片来源数量。 */
const MAX_IMAGE_SOURCES = 12;

/**
 * 把任意值规整为去首尾空白的字符串；非字符串返回空串。
 * 用于清洗请求参数与来源列表。
 */
const asString = (value) => typeof value === 'string' ? value.trim() : '';

/**
 * 判断 target 是否位于 root 目录之内（含 root 本身）。
 * 依据 path.relative 的结果：出现 `..` 前缀或绝对路径即视为越界。
 * 只做词法比较，调用方须先以 realpath 解析符号链接后再传入。
 */
const isWithin = (target, root, path) => {
  const relative = path.relative(root, target);
  return relative === '' || (!relative.startsWith('..') && !path.isAbsolute(relative));
};

/**
 * 把 Markdown 图片来源字符串解析成本地文件路径。
 * file:// URL 只接受空 host 或 localhost，并去掉 Windows 盘符路径的多余前导斜杠；
 * 普通路径则先剥离 ?query 与 #fragment 再做 percent-decode。
 * 无法解析时返回空串。
 */
const parseFileSource = (source) => {
  if (/^file:\/\//i.test(source)) {
    try {
      const url = new URL(source);
      if (url.protocol !== 'file:' || (url.host && url.host !== 'localhost')) return '';
      const pathname = decodeURIComponent(url.pathname);
      return /^\/[A-Za-z]:\//.test(pathname) ? pathname.slice(1) : pathname;
    } catch {
      return '';
    }
  }
  const pathname = source.split(/[?#]/, 1)[0] || '';
  try {
    return decodeURIComponent(pathname);
  } catch {
    return pathname;
  }
};

/**
 * 通过文件头魔数判断字节是否为受支持的图片格式（PNG/JPEG/GIF/WEBP）。
 * 只检查开头少量字节，无需读取整个文件。
 */
const hasImageSignature = (bytes) => {
  if (bytes.length >= 8
    && bytes[0] === 0x89 && bytes.subarray(1, 4).toString('ascii') === 'PNG'
    && bytes[4] === 0x0d && bytes[5] === 0x0a && bytes[6] === 0x1a && bytes[7] === 0x0a) return true;
  if (bytes.length >= 3 && bytes[0] === 0xff && bytes[1] === 0xd8 && bytes[2] === 0xff) return true;
  const header = bytes.subarray(0, 12).toString('ascii');
  return header.startsWith('GIF87a')
    || header.startsWith('GIF89a')
    || (header.startsWith('RIFF') && header.slice(8, 12) === 'WEBP');
};

/** 规整引用式图片的标签：去首尾空白、折叠连续空白为单个空格并转小写，用于匹配定义行。 */
const normalizeReferenceLabel = (value) => value.trim().replace(/\s+/g, ' ').toLowerCase();

/** 还原 Markdown 链接目标中被反斜杠转义的 ASCII 标点（含转义的反斜杠本身）。 */
const unescapeMarkdownDestination = (value) => value.replace(/\\([!"#$%&'()*+,\-./:;<=>?@[\]^_`{|}~\\])/g, '$1');

/** 判断 value[index] 处的字符是否被转义：其前方连续反斜杠数量为奇数即视为已转义。 */
const isEscapedAt = (value, index) => {
  let slashes = 0;
  for (let cursor = index - 1; cursor >= 0 && value[cursor] === '\\'; cursor -= 1) slashes += 1;
  return slashes % 2 === 1;
};

/**
 * 从 start 开始查找第一个未被转义的 `]` 并返回其下标；找不到返回 -1。
 * 用于定位图片 alt 文本或引用标签的结束位置。
 */
const findClosingBracket = (value, start) => {
  for (let cursor = start; cursor < value.length; cursor += 1) {
    if (value[cursor] === ']' && !isEscapedAt(value, cursor)) return cursor;
  }
  return -1;
};

/**
 * 在内联图片 `![alt](dest "title")` 语法中，从目的地结束位置（start）起
 * 校验可选的 title 并定位闭合 `)` 的下标；title 未闭合或缺少 `)` 返回 -1。
 */
const findInlineImageEnd = (value, start) => {
  let cursor = start;
  while (/\s/.test(value[cursor] || '')) cursor += 1;
  if (value[cursor] === ')') return cursor;

  const opener = value[cursor];
  const closer = opener === '"' ? '"' : opener === "'" ? "'" : opener === '(' ? ')' : '';
  if (!closer) return -1;
  cursor += 1;
  for (; cursor < value.length; cursor += 1) {
    if (value[cursor] !== closer || isEscapedAt(value, cursor)) continue;
    cursor += 1;
    while (/\s/.test(value[cursor] || '')) cursor += 1;
    return value[cursor] === ')' ? cursor : -1;
  }
  return -1;
};

/**
 * 解析内联图片的目的地：从 start（`(` 之后）提取目标字符串。
 * 支持 `<dest>` 尖括号形式与裸形式；裸形式正确处理反斜杠转义与括号配对深度，
 * 遇到空白则按可选 title 规则继续寻找闭合 `)`。
 * 返回 `{ source, end }`（end 为整个图片语法的结束下标），语法不完整返回 null。
 */
const parseInlineDestination = (value, start) => {
  let cursor = start;
  while (/\s/.test(value[cursor] || '')) cursor += 1;
  if (value[cursor] === '<') {
    const end = value.indexOf('>', cursor + 1);
    if (end < 0) return null;
    const imageEnd = findInlineImageEnd(value, end + 1);
    return imageEnd < 0
      ? null
      : { source: unescapeMarkdownDestination(value.slice(cursor + 1, end)), end: imageEnd };
  }

  let source = '';
  let depth = 0;
  for (; cursor < value.length; cursor += 1) {
    const char = value[cursor];
    if (char === '\\' && cursor + 1 < value.length) {
      source += char + value[cursor + 1];
      cursor += 1;
      continue;
    }
    if (char === '(') {
      depth += 1;
      source += char;
      continue;
    }
    if (char === ')') {
      if (depth === 0) return { source: unescapeMarkdownDestination(source), end: cursor };
      depth -= 1;
      source += char;
      continue;
    }
    if (/\s/.test(char) && depth === 0) {
      const imageEnd = findInlineImageEnd(value, cursor);
      return imageEnd < 0 ? null : { source: unescapeMarkdownDestination(source), end: imageEnd };
    }
    source += char;
  }
  return null;
};

/**
 * 解析引用定义 `[label]: dest` 行中的目的地：支持 `<dest>` 尖括号形式，
 * 否则取首个空白前的连续非空白片段（保留反斜杠转义），最后统一反转义。
 * 返回目标字符串，无法解析时返回空串。
 */
const parseDefinitionDestination = (value) => {
  const trimmed = value.trimStart();
  if (trimmed.startsWith('<')) {
    const end = trimmed.indexOf('>', 1);
    return end < 0 ? '' : unescapeMarkdownDestination(trimmed.slice(1, end));
  }
  const match = /^(?:\\.|\S)+/.exec(trimmed);
  return match ? unescapeMarkdownDestination(match[0]) : '';
};

/**
 * 收集消息中所有 text part 的行，并剔除代码内容：
 * 跟踪 ``` / ~~~ 围栏（闭合围栏须字符一致且长度不小于开启围栏），
 * 围栏内的行整行丢弃，其余行去掉行内代码片段（反引号包裹）后保留。
 * 代码里的图片语法不构成授权依据，因此必须先剔除。
 */
const collectMarkdownLinesOutsideCode = (message) => {
  const lines = [];
  for (const part of Array.isArray(message?.parts) ? message.parts : []) {
    if (part?.type !== 'text' || typeof part.text !== 'string') continue;
    let fence = null;
    for (const line of part.text.split('\n')) {
      const fenceMatch = /^\s{0,3}(`{3,}|~{3,})/.exec(line);
      if (fenceMatch) {
        const marker = fenceMatch[1];
        if (!fence) {
          fence = { char: marker[0], size: marker.length };
        } else if (marker[0] === fence.char && marker.length >= fence.size) {
          fence = null;
        }
        continue;
      }
      if (fence) continue;
      lines.push(line.replace(/`+[^`]*`+/g, ''));
    }
  }
  return lines;
};

/**
 * 提取消息 Markdown 文本引用的全部图片来源（去重 Set）。
 * 第一遍扫描引用定义行，建立 规范化标签 -> 目的地 的映射；
 * 第二遍逐行扫描 `![alt](dest)`、`![alt][label]` 与 `![label][]` 形式的图片，
 * 引用式经标签查表取得目的地。游标跳过已消费片段，避免 alt 文本被二次匹配。
 */
const markdownImageSources = (message) => {
  const sources = new Set();
  const markdownLines = collectMarkdownLinesOutsideCode(message);
  const definitions = new Map();
  for (const line of markdownLines) {
    const match = /^\s{0,3}\[([^\]]+)]\s*:\s*(.*)$/.exec(line);
    if (!match) continue;
    const source = parseDefinitionDestination(match[2]);
    if (source) definitions.set(normalizeReferenceLabel(match[1]), source);
  }

  for (const line of markdownLines) {
    for (let cursor = 0; cursor < line.length; cursor += 1) {
      if (line[cursor] !== '!' || line[cursor + 1] !== '[' || isEscapedAt(line, cursor)) continue;
      const altEnd = findClosingBracket(line, cursor + 2);
      if (altEnd < 0) continue;
      const alt = line.slice(cursor + 2, altEnd);
      const next = line[altEnd + 1];
      if (next === '(') {
        const parsed = parseInlineDestination(line, altEnd + 2);
        if (parsed?.source) sources.add(parsed.source);
        cursor = parsed?.end ?? altEnd;
        continue;
      }
      let label = alt;
      if (next === '[') {
        const labelEnd = findClosingBracket(line, altEnd + 2);
        if (labelEnd < 0) continue;
        label = line.slice(altEnd + 2, labelEnd) || alt;
        cursor = labelEnd;
      } else {
        cursor = altEnd;
      }
      const source = definitions.get(normalizeReferenceLabel(label));
      if (source) sources.add(source);
    }
  }
  return sources;
};

/**
 * 通过 OpenCode API 拉取指定会话中的一条消息。
 * directory 同时写入 query 与 `x-opencode-directory` 请求头（percent-encode，
 * OpenCode 拒绝原始非 ASCII 值）；请求带 10 秒超时。
 * 404 或结构不符（缺 info、parts 非数组）返回 null，其余非 2xx 抛错。
 */
const fetchMessage = async ({ sessionId, messageId, directory, buildOpenCodeUrl, getOpenCodeAuthHeaders }) => {
  const url = new URL(buildOpenCodeUrl(
    `/session/${encodeURIComponent(sessionId)}/message/${encodeURIComponent(messageId)}`,
    '',
  ));
  url.searchParams.set('directory', directory);
  const response = await fetch(url, {
    headers: {
      accept: 'application/json',
      // Percent-encoded to match the SDK wire format; raw non-ASCII values
      // are rejected by OpenCode.
      'x-opencode-directory': encodeURIComponent(directory),
      ...getOpenCodeAuthHeaders(),
    },
    signal: AbortSignal.timeout(10_000),
  });
  if (response.status === 404) return null;
  if (!response.ok) throw new Error(`OpenCode returned ${response.status}`);
  const message = await response.json().catch(() => null);
  return message?.info && Array.isArray(message.parts) ? message : null;
};

/**
 * 安检单个图片来源并返回检查结果。
 * 相对路径按工作区解析；工作区外的路径仅允许位于 approvedTempRoot。
 * 先对根与目标做 realpath 解析符号链接（词法前缀不是授权边界），
 * 再以 O_NOFOLLOW 打开：必须是普通文件、不超过 MAX_IMAGE_BYTES、
 * 魔数符合受支持图片格式。
 * 返回 `{ status: 'ready', path, outsideWorkspace }`；文件不存在为 'missing'，
 * 其余失败（越界、权限、符号链接逃逸、非图片、超限）为 'error'。
 */
const inspectImage = async ({ source, directory, approvedTempRoot, fsPromises, path }) => {
  const parsed = parseFileSource(source);
  if (!parsed) return { status: 'error' };
  const sourcePath = path.isAbsolute(parsed) ? parsed : path.resolve(directory, parsed);
  const workspaceRoot = path.resolve(directory);
  const outsideWorkspace = !isWithin(path.resolve(sourcePath), workspaceRoot, path);
  const root = outsideWorkspace ? approvedTempRoot : workspaceRoot;

  try {
    // Resolve symlinks before comparing roots; lexical prefixes are not an authorization boundary.
    const [canonicalRoot, canonicalPath] = await Promise.all([
      fsPromises.realpath(root),
      fsPromises.realpath(sourcePath),
    ]);
    if (!isWithin(canonicalPath, canonicalRoot, path)) return { status: 'error' };
    const handle = await fsPromises.open(canonicalPath, fsConstants.O_RDONLY | fsConstants.O_NOFOLLOW);
    try {
      const stats = await handle.stat();
      if (!stats.isFile() || stats.size > MAX_IMAGE_BYTES) return { status: 'error' };
      const header = Buffer.alloc(12);
      const { bytesRead } = await handle.read(header, 0, header.length, 0);
      if (!hasImageSignature(header.subarray(0, bytesRead))) return { status: 'error' };
      return {
        status: 'ready',
        path: outsideWorkspace ? canonicalPath : path.resolve(sourcePath),
        outsideWorkspace,
      };
    } finally {
      await handle.close();
    }
  } catch (error) {
    if (error?.code === 'ENOENT') return { status: 'missing' };
    if (error?.code === 'EACCES' || error?.code === 'EPERM' || error?.code === 'ELOOP') {
      return { status: 'error' };
    }
    throw error;
  }
};

/**
 * 在 express 应用上注册会话 Markdown 图片授权路由。
 *
 * 通过 dependencies 注入：fsPromises/path/os/crypto、validateDirectoryPath
 * （目录校验）、buildOpenCodeUrl 与 getOpenCodeAuthHeaders（访问 OpenCode API）、
 * approvedTempRoot（工作区外唯一允许的临时目录，默认 os.tmpdir() 下的 opencode）。
 */
export const registerMarkdownImageGrantRoutes = (app, dependencies) => {
  const {
    fsPromises,
    path,
    os,
    crypto,
    validateDirectoryPath,
    buildOpenCodeUrl,
    getOpenCodeAuthHeaders,
    approvedTempRoot = path.join(os.tmpdir(), 'opencode'),
  } = dependencies;

  // POST /api/ompchamber/sessions/:sessionId/markdown-image-grants
  // 请求体 { directory, messageId, sources[] }。校验参数与目录后拉取助手消息，
  // 仅对消息 Markdown 中真实引用的来源逐个安检；工作区外文件经
  // mintOutsideFileGrant 换发路径绑定的 raw 授权。逐来源返回
  // { source, status: 'ready'|'missing'|'error', path?, outsideFileGrant?, expiresAt? }，
  // 单个来源失败不影响其余来源（部分成功）；消息拉取失败整体返回 503。
  app.post(
    '/api/ompchamber/sessions/:sessionId/markdown-image-grants',
    express.json({ limit: '32kb' }),
    async (req, res) => {
      const sessionId = asString(req.params.sessionId);
      const messageId = asString(req.body?.messageId);
      const sources = Array.isArray(req.body?.sources)
        ? [...new Set(req.body.sources.map(asString).filter(Boolean))]
        : [];
      if (!sessionId || !messageId || sources.length === 0 || sources.length > MAX_IMAGE_SOURCES) {
        return res.status(400).json({ error: 'sessionId, messageId, and 1-12 sources are required' });
      }
      const validatedDirectory = await validateDirectoryPath(asString(req.body?.directory));
      if (!validatedDirectory.ok) {
        return res.status(400).json({ error: validatedDirectory.error || 'Invalid directory' });
      }

      try {
        const message = await fetchMessage({
          sessionId,
          messageId,
          directory: validatedDirectory.directory,
          buildOpenCodeUrl,
          getOpenCodeAuthHeaders,
        });
        if (!message || message.info?.id !== messageId || message.info?.role !== 'assistant') {
          return res.status(404).json({ error: 'Assistant message not found' });
        }
        // Assistant text is authoritative: a remote client cannot mint grants for unreferenced paths.
        const referenced = markdownImageSources(message);
        const results = [];
        for (const source of sources) {
          if (!referenced.has(source)) {
            results.push({ source, status: 'error' });
            continue;
          }
          try {
            const inspected = await inspectImage({
              source,
              directory: validatedDirectory.directory,
              approvedTempRoot,
              fsPromises,
              path,
            });
            if (inspected.status !== 'ready') {
              results.push({ source, status: inspected.status });
              continue;
            }
            // Reuse the existing path-bound raw-file grant instead of creating another asset lifecycle.
            const grant = inspected.outsideWorkspace
              ? await mintOutsideFileGrant(inspected.path, {
                scopes: ['raw'],
                fsPromises,
                path,
                crypto,
              })
              : null;
            results.push({
              source,
              status: 'ready',
              path: inspected.path,
              outsideFileGrant: grant?.outsideFileGrant,
              expiresAt: grant?.expiresAt,
            });
          } catch {
            results.push({ source, status: 'error' });
          }
        }
        return res.json({ results });
      } catch (error) {
        console.warn('[MarkdownImageGrants] failed to prepare images:', error?.message || error);
        return res.status(503).json({ error: 'Failed to prepare session images' });
      }
    },
  );
};
