/**
 * 统一 diff 的解析层：把 `git diff` 输出拆成文件与可寻址的 hunk，并生成
 * "scope:路径:内容哈希" 形式的稳定 hunk id。模型叙事锚定这些 id，客户端再用
 * id 找回渲染代码，过期检测就是"id 在当前 diff 中已不存在"——三者都依赖同一
 * 套 id 定义，因此 id 的生成算法只存在于这里，客户端只拿到结果。
 */
import crypto from 'crypto';

// Parsing a unified diff into addressable hunks lives here and only here. The
// model anchors its narrative to hunk ids, the client resolves those ids back
// to rendered code, and staleness is "an id the current diff no longer has" —
// all three break the moment two implementations disagree about what an id is,
// so the client is never given the algorithm, only the results.

/** 文件头行（diff --git ...）的识别正则。 */
const FILE_HEADER = /^diff --git /;
/** hunk 头行（@@ -a,b +c,d @@ ...）的识别正则，捕获新旧起始行号与行数。 */
const HUNK_HEADER = /^@@\s+-(\d+)(?:,(\d+))?\s+\+(\d+)(?:,(\d+))?\s+@@(.*)$/;

/** 计算字符串的 sha1 前 8 位十六进制摘要，用作 hunk id 的内容指纹。 */
const shortHash = (value) => crypto.createHash('sha1').update(value).digest('hex').slice(0, 8);

/** 从 "diff --git a/old b/new" 行解析两侧路径（兼容含空格时的引号形式）；不匹配返回 null。 */
const parsePathsFromFileHeader = (line) => {
  // `diff --git a/old b/new`, with either side quoted when it contains spaces.
  const match = /^diff --git (?:"?a\/(.+?)"?) (?:"?b\/(.+?)"?)$/.exec(line);
  if (!match) return null;
  return { oldPath: match[1], newPath: match[2] };
};

/** 由文件头附属行（new file mode / deleted file mode / rename from）推断变更状态，缺省为 modified。 */
const statusFromHeaderLines = (lines) => {
  if (lines.some((line) => line.startsWith('new file mode'))) return 'added';
  if (lines.some((line) => line.startsWith('deleted file mode'))) return 'deleted';
  if (lines.some((line) => line.startsWith('rename from'))) return 'renamed';
  return 'modified';
};

/** 判断文件头附属行是否声明了二进制内容。 */
const isBinaryHeader = (lines) => lines.some((line) => line.startsWith('Binary files ') || line.startsWith('GIT binary patch'));

/**
 * Split a unified diff covering any number of files into files and hunks.
 *
 * @param {string} patch raw `git diff` output
 * @param {string} scope opaque namespace for the ids (e.g. 'staged', 'branch').
 *   Two scopes of the same repository can contain byte-identical hunks; the
 *   scope keeps their ids distinct so a walkthrough written against staged
 *   changes never silently resolves against unstaged ones.
 * @returns {{files: Array<{path: string, oldPath: string|null, status: string, binary: boolean, hunks: Array<object>}>}}
 */
/**
 * 把覆盖任意数量文件的统一 diff 拆成 files 与 hunks。scope 进入每个 hunk id，
 * 使同一仓库两个 scope 下字节相同的 hunk 拥有不同 id（针对 staged 写的停靠点
 * 不会静默锚到 unstaged 的相同内容）。文件内字节级重复的 hunk 以出现次数后缀
 * 去重；hunk id 覆盖头与正文，任何编辑（即使行号不变）都会得到新 id。
 */
export function parseDiffFiles(patch, scope = 'diff') {
  const text = typeof patch === 'string' ? patch : '';
  if (!text.trim()) return { files: [] };

  const lines = text.split(/\r?\n/);
  const files = [];
  let current = null;
  let headerLines = [];
  let hunk = null;

  /** 结算当前 hunk：计算内容摘要与去重后缀，组装 hunk 对象（含可直接渲染的 patch 文本）压入当前文件。 */
  const closeHunk = () => {
    if (!current || !hunk) return;
    const body = hunk.lines.join('\n');
    // The id covers the header and the body, so any edit to the hunk — even one
    // that keeps its line numbers — produces a different id. That is what makes
    // "this stop is stale" detectable without diffing narratives.
    const digest = shortHash(`${hunk.header}\n${body}`);
    const seen = current.hunkDigests.get(digest) ?? 0;
    current.hunkDigests.set(digest, seen + 1);
    // A file can legitimately contain byte-identical hunks (repeated boilerplate
    // edits). Disambiguate by occurrence so ids stay unique without becoming
    // positional for the common case.
    const suffix = seen === 0 ? '' : `-${seen + 1}`;

    current.hunks.push({
      id: `${scope}:${current.path}:${digest}${suffix}`,
      header: hunk.header,
      oldStart: hunk.oldStart,
      oldLines: hunk.oldLines,
      newStart: hunk.newStart,
      newLines: hunk.newLines,
      added: hunk.added,
      deleted: hunk.deleted,
      patch: `${current.headerText}\n${hunk.header}\n${body}\n`,
      body,
    });
    hunk = null;
  };

  /** 结算当前文件：先结算 hunk，补二进制标记、清理临时字段后压入结果列表。 */
  const closeFile = () => {
    closeHunk();
    if (!current) return;
    current.binary = current.binary || isBinaryHeader(headerLines);
    delete current.hunkDigests;
    files.push(current);
    current = null;
  };

  for (const line of lines) {
    if (FILE_HEADER.test(line)) {
      closeFile();
      headerLines = [line];
      const paths = parsePathsFromFileHeader(line);
      current = {
        path: paths?.newPath || paths?.oldPath || '',
        oldPath: paths && paths.oldPath !== paths.newPath ? paths.oldPath : null,
        status: 'modified',
        binary: false,
        headerText: line,
        hunks: [],
        hunkDigests: new Map(),
      };
      continue;
    }

    if (!current) continue;

    const hunkMatch = HUNK_HEADER.exec(line);
    if (hunkMatch) {
      closeHunk();
      current.status = statusFromHeaderLines(headerLines);
      current.headerText = headerLines.join('\n');
      hunk = {
        header: line,
        oldStart: Number.parseInt(hunkMatch[1], 10),
        oldLines: hunkMatch[2] === undefined ? 1 : Number.parseInt(hunkMatch[2], 10),
        newStart: Number.parseInt(hunkMatch[3], 10),
        newLines: hunkMatch[4] === undefined ? 1 : Number.parseInt(hunkMatch[4], 10),
        added: 0,
        deleted: 0,
        lines: [],
      };
      continue;
    }

    if (!hunk) {
      headerLines.push(line);
      continue;
    }

    hunk.lines.push(line);
    if (line.startsWith('+')) hunk.added += 1;
    else if (line.startsWith('-')) hunk.deleted += 1;
  }

  closeFile();

  return {
    files: files.filter((file) => file.path),
  };
}

/**
 * Flatten parsed files into an id-keyed index for resolution and staleness
 * checks.
 */
/** 把解析结果拍平为 id -> hunk（附所属文件路径与状态）的索引，供锚点解析与过期检测使用。 */
export function indexHunks(files) {
  const index = new Map();
  for (const file of files) {
    for (const hunk of file.hunks) {
      index.set(hunk.id, { ...hunk, path: file.path, status: file.status });
    }
  }
  return index;
}

/**
 * Every hunk id in the diff, in file-then-position order. Used to compute the
 * "not covered by any stop" tail.
 */
/** 按文件与出现顺序列出全部 hunk id，用于计算"未被任何停靠点覆盖"的尾部。 */
export function listHunkIds(files) {
  const ids = [];
  for (const file of files) {
    for (const hunk of file.hunks) ids.push(hunk.id);
  }
  return ids;
}
