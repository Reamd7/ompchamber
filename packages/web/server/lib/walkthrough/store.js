/**
 * Walkthrough 的磁盘存储层：内容寻址的缓存条目（entries/）+ 每仓库每来源
 * 一个的可变指针（pointers/）。缓存条目不可变，以"当前 diff + 模型 + 语言 +
 * 版本"为键，命中即精确；指针回答"这里最后一次展示的是哪份"。带条数/总字节
 * 上限的 LRU 淘汰，指向被淘汰条目的指针读作"无 walkthrough"。所有读写失败
 * 都返回 null/false 而不抛错——坏的缓存文件绝不能拖垮功能本身。
 */
import crypto from 'crypto';
import fs from 'fs';
import fsp from 'fs/promises';
import os from 'os';
import path from 'path';
import { PROMPT_VERSION, WALKTHROUGH_VERSION } from './schema.js';

// Two artifacts with two different jobs.
//
// Cache entries are content-addressed and immutable: the key is derived from
// the *current* diff, so a hit means "this walkthrough was written about
// exactly this code". There is no freshness question to ask of an entry —
// staleness is a miss.
//
// The pointer is mutable and keyed by repository + source only. It answers the
// questions the cache cannot: which walkthrough was the last one here, what was
// it written about, and has the code moved since. It is also what feeds the
// previous walkthrough into a regeneration.

/** 数据目录根：OMPCHAMBER_DATA_DIR 环境变量优先，否则 ~/.config/ompchamber。 */
const DATA_DIR = process.env.OMPCHAMBER_DATA_DIR
  ? path.resolve(process.env.OMPCHAMBER_DATA_DIR)
  : path.join(os.homedir(), '.config', 'ompchamber');

/** walkthrough 存储根目录（数据目录下的 walkthroughs/）。 */
const WALKTHROUGH_DIR = path.join(DATA_DIR, 'walkthroughs');
/** 缓存条目目录，每条一个以 cacheKey 命名的 json 文件。 */
const ENTRIES_DIR = path.join(WALKTHROUGH_DIR, 'entries');
/** 指针目录，每"仓库+来源"一个哈希命名的 json 文件。 */
const POINTERS_DIR = path.join(WALKTHROUGH_DIR, 'pointers');

/** 缓存条目数量上限，超出按 atime 淘汰最久未用的条目。 */
const MAX_ENTRIES = 200;
/** 缓存总字节上限（50MB），与条数上限共同触发淘汰。 */
const MAX_TOTAL_BYTES = 50 * 1024 * 1024;
/** 单个存储文件的最大字节数（4MB），超过视为异常直接当作不存在。 */
const MAX_FILE_BYTES = 4 * 1024 * 1024;

/** 计算字符串的 sha256 十六进制摘要，用于缓存键与指针文件名。 */
const sha256 = (value) => crypto.createHash('sha256').update(value).digest('hex');

/** 递归创建目录；失败只记日志并返回 false，绝不抛错。 */
const ensureDir = (dir) => {
  try {
    fs.mkdirSync(dir, { recursive: true });
    return true;
  } catch (error) {
    console.error('[walkthrough] failed to create store directory:', error?.message || error);
    return false;
  }
};

// Atomic so a crash mid-write leaves the previous entry intact rather than a
// half-written file that later fails to parse.
/** 原子写 JSON：先写临时文件再 rename，中途崩溃留下的是完好的旧文件而非半截文件；失败返回 false。 */
const writeJsonAtomic = (filePath, value) => {
  if (!ensureDir(path.dirname(filePath))) return false;
  const tmp = `${filePath}.${process.pid}.${Date.now()}.tmp`;
  try {
    fs.writeFileSync(tmp, JSON.stringify(value), 'utf8');
    fs.renameSync(tmp, filePath);
    return true;
  } catch (error) {
    console.error('[walkthrough] failed to write store file:', error?.message || error);
    try {
      fs.unlinkSync(tmp);
    } catch {
      // Nothing else to do; the temp file is already orphaned.
    }
    return false;
  }
};

/** 读取并解析 JSON 文件；缺失、不可读、超过大小上限或损坏一律返回 null，不抛错。 */
const readJson = (filePath) => {
  try {
    const stat = fs.statSync(filePath);
    if (!stat.isFile() || stat.size > MAX_FILE_BYTES) return null;
    return JSON.parse(fs.readFileSync(filePath, 'utf8'));
  } catch {
    // Missing, unreadable, or corrupt all mean the same thing to callers: no
    // usable cached walkthrough. Never throw — a bad cache file must not break
    // the feature.
    return null;
  }
};

/**
 * Content-addressed key. Every input that can change the output is in here:
 * change any of them and you get a miss rather than a stale hit.
 */
/**
 * 内容寻址缓存键：把 walkthrough/prompt 版本、仓库、来源、模型、语言与排序
 * 后的文件+hunk id 清单一起哈希——任何能改变输出的输入变化都会换键（miss），
 * 因此永远不会读到内容不符的旧条目（stale hit）。
 */
export function buildCacheKey({ repoRoot, sourceKey, providerID, modelID, language, files }) {
  const canonical = JSON.stringify({
    walkthroughVersion: WALKTHROUGH_VERSION,
    promptVersion: PROMPT_VERSION,
    repoRoot,
    sourceKey,
    providerID,
    modelID,
    // Without this, switching language hits the entry written in the previous
    // one and the panel answers a request to translate with the untranslated
    // text it already had.
    language,
    files: [...files]
      .map((file) => ({ path: file.path, status: file.status, hunkIds: file.hunks.map((hunk) => hunk.id) }))
      .sort((a, b) => (a.path < b.path ? -1 : a.path > b.path ? 1 : 0)),
  });
  return sha256(canonical);
}

/** 缓存条目的磁盘路径：<cacheKey>.json。 */
const entryPath = (cacheKey) => path.join(ENTRIES_DIR, `${cacheKey}.json`);
/** 指针的磁盘路径：以"仓库根 + NUL + sourceKey"的 sha256 命名，规避路径非法字符。 */
const pointerPath = (repoRoot, sourceKey) => path.join(POINTERS_DIR, `${sha256(`${repoRoot}\0${sourceKey}`)}.json`);

/** 校验读出的 JSON 是否为当前版本的 walkthrough 条目；版本不符视为 miss。 */
const isWalkthroughEntry = (value) => Boolean(
  value
  && typeof value === 'object'
  && value.walkthroughVersion === WALKTHROUGH_VERSION
  && value.walkthrough
  && Array.isArray(value.walkthrough.chapters),
);

/** 按 cacheKey 读取缓存条目；文件缺失或校验不通过返回 null。 */
export function readCachedWalkthrough(cacheKey) {
  const value = readJson(entryPath(cacheKey));
  return isWalkthroughEntry(value) ? value : null;
}

/** 原子写入缓存条目（自动补 walkthroughVersion），成功后触发 LRU 淘汰。 */
export function writeCachedWalkthrough(cacheKey, entry) {
  const written = writeJsonAtomic(entryPath(cacheKey), {
    walkthroughVersion: WALKTHROUGH_VERSION,
    ...entry,
  });
  if (written) evictEntries();
  return written;
}

/** 读取某仓库+来源的指针；结构不合法（无字符串 cacheKey）返回 null。 */
export function readPointer(repoRoot, sourceKey) {
  const value = readJson(pointerPath(repoRoot, sourceKey));
  if (!value || typeof value !== 'object' || typeof value.cacheKey !== 'string') return null;
  return value;
}

/** 原子写入某仓库+来源的指针。 */
export function writePointer(repoRoot, sourceKey, pointer) {
  return writeJsonAtomic(pointerPath(repoRoot, sourceKey), pointer);
}

/**
 * Bound the cache by count and total size, dropping least-recently-used
 * entries. Pointers are tiny and are left alone; a pointer to an evicted entry
 * simply reads as "no walkthrough", which is the truthful answer.
 */
/** 按条数与总字节数做 LRU 淘汰：按 atime 从旧到新删除直到两项都回到上限内；删除失败的文件跳过，下次写入再试。 */
function evictEntries() {
  let files;
  try {
    files = fs.readdirSync(ENTRIES_DIR)
      .filter((name) => name.endsWith('.json'))
      .map((name) => {
        const full = path.join(ENTRIES_DIR, name);
        try {
          const stat = fs.statSync(full);
          return { full, size: stat.size, atime: stat.atimeMs };
        } catch {
          return null;
        }
      })
      .filter(Boolean);
  } catch {
    return;
  }

  let totalBytes = files.reduce((sum, file) => sum + file.size, 0);
  if (files.length <= MAX_ENTRIES && totalBytes <= MAX_TOTAL_BYTES) return;

  files.sort((a, b) => a.atime - b.atime);
  let count = files.length;
  for (const file of files) {
    if (count <= MAX_ENTRIES && totalBytes <= MAX_TOTAL_BYTES) break;
    try {
      fs.unlinkSync(file.full);
      count -= 1;
      totalBytes -= file.size;
    } catch {
      // Skip files we cannot remove; the next write retries.
    }
  }
}

// Housekeeping runs off the request path and never synchronously.
//
// The Electron desktop app hosts this server inside the main process, so a
// blocking loop here stalls IPC and the window, not just one request. Worse,
// the paths being checked are user repositories: a worktree on an unplugged
// drive or an unreachable network share can make a single existence check hang
// for seconds. Async calls wait without holding the loop, and the cap keeps a
// pathological directory from turning into a long tail of work.
/** 单次 prune 最多处理的指针数，防止病态目录变成拖不完的长尾。 */
const PRUNE_LIMIT = 500;

/**
 * Drop pointers for repositories that no longer exist. Only ever removes
 * entries whose subject is provably gone.
 */
/** 删除仓库目录已确定不存在（ENOENT）的指针；"不可达"不等于"已删除"，绝不因网络/权限问题误删。返回删除数。 */
export async function pruneMissingRepositories() {
  let names;
  try {
    names = (await fsp.readdir(POINTERS_DIR)).filter((name) => name.endsWith('.json'));
  } catch {
    return 0;
  }

  let removed = 0;
  for (const name of names.slice(0, PRUNE_LIMIT)) {
    const full = path.join(POINTERS_DIR, name);
    let repoRoot = null;
    try {
      const value = JSON.parse(await fsp.readFile(full, 'utf8'));
      repoRoot = value && typeof value.repoRoot === 'string' ? value.repoRoot : null;
    } catch {
      continue;
    }
    if (!repoRoot) continue;

    try {
      await fsp.stat(repoRoot);
      continue;
    } catch (error) {
      // Unreachable is not the same as gone. Only a definite "no such file"
      // justifies deleting: a disconnected share or a permissions error must
      // not cost the user their walkthroughs.
      if (error?.code !== 'ENOENT') continue;
    }

    try {
      await fsp.unlink(full);
      removed += 1;
    } catch {
      // Leave it; the next prune retries.
    }
  }
  return removed;
}

/** 测试导出：暴露目录与上限常量供测试断言使用。 */
export const __testing = { WALKTHROUGH_DIR, ENTRIES_DIR, POINTERS_DIR, MAX_ENTRIES, MAX_TOTAL_BYTES };
