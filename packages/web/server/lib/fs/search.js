/**
 * 文件系统模糊搜索模块：提供按文件名/相对路径模糊匹配的文件搜索运行时。
 *
 * 由 createFsSearchRuntime 工厂创建，依赖（fsPromises、path、spawn、
 * resolveGitBinaryForSpawn）全部由调用方注入，便于测试替换。
 * 搜索采用广度优先遍历：分批并发读取目录、跳过依赖与隐藏目录、
 * 可选通过 git check-ignore 过滤被忽略条目，最后用模糊打分算法排序返回。
 */

/** BFS 遍历中每批并发读取的目录数量上限，避免瞬时打开过多文件句柄。 */
const FILE_SEARCH_MAX_CONCURRENCY = 5;
/** 永远跳过的目录名集合（比较时转小写）：依赖、构建产物、缓存等大目录。 */
const FILE_SEARCH_EXCLUDED_DIRS = new Set([
  'node_modules',
  '.git',
  'dist',
  'build',
  '.next',
  '.turbo',
  '.cache',
  'coverage',
  'tmp',
  'logs',
]);

/**
 * 把绝对路径转换成相对 rootPath、以 '/' 分隔的搜索友好路径。
 * 相对部分为空时回退为 basename，仍为空则原样返回 targetPath。
 */
const normalizeRelativeSearchPath = (rootPath, targetPath, path) => {
  const relative = path.relative(rootPath, targetPath) || path.basename(targetPath);
  return relative.split(path.sep).join('/') || targetPath;
};

/**
 * 判断目录是否应从遍历中跳过：未开启 includeHidden 时跳过点开头的隐藏目录；
 * 命中 FILE_SEARCH_EXCLUDED_DIRS（忽略大小写）的目录总是跳过。
 */
const shouldSkipSearchDirectory = (name, includeHidden) => {
  if (!name) {
    return false;
  }
  if (!includeHidden && name.startsWith('.')) {
    return true;
  }
  return FILE_SEARCH_EXCLUDED_DIRS.has(name.toLowerCase());
};

/** 读取目录项（withFileTypes），任何错误（不存在、无权限等）都吞掉并返回空数组。 */
const listDirectoryEntries = async (dirPath, fsPromises) => {
  try {
    return await fsPromises.readdir(dirPath, { withFileTypes: true });
  } catch {
    return [];
  }
};

/**
 * 对已小写归一的查询与候选路径打模糊匹配分。
 * 候选完整包含查询子串时得高分（前缀或边界字符额外加分，位置越靠后、
 * 路径越长扣分越多）；否则逐字符贪婪匹配：连续命中与靠前位置加分、
 * 间隔扣分。任一字符找不到返回 null 表示不匹配；空查询返回 0 分。
 */
const fuzzyMatchScoreNormalized = (normalizedQuery, candidate) => {
  if (!normalizedQuery) return 0;

  const q = normalizedQuery;
  const c = candidate.toLowerCase();
  if (c.includes(q)) {
    const idx = c.indexOf(q);
    let bonus = 0;
    if (idx === 0) {
      bonus = 20;
    } else {
      const prev = c[idx - 1];
      if (prev === '/' || prev === '_' || prev === '-' || prev === '.' || prev === ' ') {
        bonus = 15;
      }
    }
    return 100 + bonus - Math.min(idx, 20) - Math.floor(c.length / 5);
  }

  let score = 0;
  let lastIndex = -1;
  let consecutive = 0;

  for (let i = 0; i < q.length; i += 1) {
    const ch = q[i];
    if (!ch || ch === ' ') continue;

    const idx = c.indexOf(ch, lastIndex + 1);
    if (idx === -1) {
      return null;
    }

    const gap = idx - lastIndex - 1;
    if (gap === 0) {
      consecutive += 1;
    } else {
      consecutive = 0;
    }

    score += 10;
    score += Math.max(0, 18 - idx);
    score -= Math.min(gap, 10);

    if (idx === 0) {
      score += 12;
    } else {
      const prev = c[idx - 1];
      if (prev === '/' || prev === '_' || prev === '-' || prev === '.' || prev === ' ') {
        score += 10;
      }
    }

    score += consecutive > 0 ? 12 : 0;
    lastIndex = idx;
  }

  score += Math.max(0, 24 - Math.floor(c.length / 3));
  return score;
};

/**
 * 创建文件搜索运行时。
 * @param {object} deps 注入依赖：fsPromises 文件系统 API、path 路径模块、
 *   spawn 子进程工厂（用于 git check-ignore）、resolveGitBinaryForSpawn
 *   返回可用的 git 可执行名。
 * @returns {{ searchFilesystemFiles: Function }} 暴露给路由层的搜索入口。
 */
export const createFsSearchRuntime = ({ fsPromises, path, spawn, resolveGitBinaryForSpawn }) => {
  /**
   * 从 rootPath 出发广度优先搜索文件。
   * @param {string} rootPath 搜索根目录（绝对路径）。
   * @param {object} options 搜索选项：limit 返回条数上限；query 查询串
   *   （空串表示匹配全部，按遍历顺序返回）；includeHidden 是否包含隐藏条目；
   *   respectGitignore 是否用 git check-ignore 过滤（默认开启，失败时静默降级）。
   * @returns {Promise<Array<{name: string, path: string, relativePath: string,
   *   extension: string|undefined}>>} 模糊匹配时按得分降序（同分依次按
   *   路径长度、字典序）排序并截断到 limit 的候选列表；为排序稳定，
   *   匹配模式下会先收集约 3 倍于 limit 的候选再截断。
   */
  const searchFilesystemFiles = async (rootPath, options) => {
    const { limit, query, includeHidden, respectGitignore } = options;
    const includeHiddenEntries = Boolean(includeHidden);
    const normalizedQuery = query.trim().toLowerCase();
    const matchAll = normalizedQuery.length === 0;
    const queue = [rootPath];
    const visited = new Set([rootPath]);
    const shouldRespectGitignore = respectGitignore !== false;
    const collectLimit = matchAll ? limit : Math.max(limit * 3, 200);
    const candidates = [];

    while (queue.length > 0 && candidates.length < collectLimit) {
      const batch = queue.splice(0, FILE_SEARCH_MAX_CONCURRENCY);

      const dirResults = await Promise.all(
        batch.map(async (dir) => {
          if (!shouldRespectGitignore) {
            return { dir, dirents: await listDirectoryEntries(dir, fsPromises), ignoredPaths: new Set() };
          }

          try {
            const dirents = await listDirectoryEntries(dir, fsPromises);
            const pathsToCheck = dirents.map((dirent) => dirent.name).filter(Boolean);
            if (pathsToCheck.length === 0) {
              return { dir, dirents, ignoredPaths: new Set() };
            }

            const result = await new Promise((resolve) => {
              const child = spawn(resolveGitBinaryForSpawn(), ['check-ignore', '--', ...pathsToCheck], {
                cwd: dir,
                windowsHide: true,
                stdio: ['ignore', 'pipe', 'pipe'],
              });

              let stdout = '';
              child.stdout.on('data', (data) => { stdout += data.toString(); });
              child.on('close', () => resolve(stdout));
              child.on('error', () => resolve(''));
            });

            const ignoredNames = new Set(
              String(result)
                .split('\n')
                .map((name) => name.trim())
                .filter(Boolean)
            );

            return { dir, dirents, ignoredPaths: ignoredNames };
          } catch {
            return { dir, dirents: await listDirectoryEntries(dir, fsPromises), ignoredPaths: new Set() };
          }
        })
      );

      for (const { dir: currentDir, dirents, ignoredPaths } of dirResults) {
        for (const dirent of dirents) {
          const entryName = dirent.name;
          if (!entryName || (!includeHiddenEntries && entryName.startsWith('.'))) {
            continue;
          }

          if (shouldRespectGitignore && ignoredPaths.has(entryName)) {
            continue;
          }

          const entryPath = path.join(currentDir, entryName);

          if (dirent.isDirectory()) {
            if (shouldSkipSearchDirectory(entryName, includeHiddenEntries)) {
              continue;
            }
            if (!visited.has(entryPath)) {
              visited.add(entryPath);
              queue.push(entryPath);
            }
            continue;
          }

          if (!dirent.isFile()) {
            continue;
          }

          const relativePath = normalizeRelativeSearchPath(rootPath, entryPath, path);
          const extension = entryName.includes('.') ? entryName.split('.').pop()?.toLowerCase() : undefined;

          if (matchAll) {
            candidates.push({
              name: entryName,
              path: entryPath,
              relativePath,
              extension,
              score: 0,
            });
          } else {
            const score = fuzzyMatchScoreNormalized(normalizedQuery, relativePath);
            if (score !== null) {
              candidates.push({
                name: entryName,
                path: entryPath,
                relativePath,
                extension,
                score,
              });
            }
          }

          if (candidates.length >= collectLimit) {
            queue.length = 0;
            break;
          }
        }

        if (candidates.length >= collectLimit) {
          break;
        }
      }
    }

    if (!matchAll) {
      candidates.sort((a, b) => {
        if (b.score !== a.score) return b.score - a.score;
        if (a.relativePath.length !== b.relativePath.length) {
          return a.relativePath.length - b.relativePath.length;
        }
        return a.relativePath.localeCompare(b.relativePath);
      });
    }

    return candidates.slice(0, limit).map(({ name, path: filePath, relativePath, extension }) => ({
      name,
      path: filePath,
      relativePath,
      extension,
    }));
  };

  return {
    searchFilesystemFiles,
  };
};
