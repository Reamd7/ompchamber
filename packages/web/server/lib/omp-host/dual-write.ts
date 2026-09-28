/**
 * 双写（dual-write）场景下的外部变更检测（docs/plan.md §8，阶段 4）。
 *
 * 宿主与外部 omp 写入方（TUI、另一宿主进程）共享 transcript 文件且
 * 没有跨进程锁。fileSignature 抓取文件的廉价可观测身份（size +
 * mtimeMs + 最后一条 entry id），供物化边界分类外部变更：
 *   - append：体积增长且尾 entry id 变化 → 良性追加，交给 tail-sync；
 *   - truncate/rewrite：size/mtime 相对记录身份移动，或尾 id 回退 →
 *     dirty，需要一次有界重载（或冷读）恢复新鲜度。
 * 分类刻意保守（plan D8）：尾 id 只是快速提示而非证明——TUI 的
 * rewriteEntries() 可能在改写中间内容的同时保留最后一条 id，所以
 * 任何 size/mtime 移动都判 dirty。这是"检测 + 吸收 + 复查"，不是 CAS。
 */
// Dual-write detection (docs/plan.md §8, phase 4).
//
// The host shares transcript files with external omp writers (TUI, another
// host process) without any cross-process lock. `fileSignature` captures the
// cheap observable identity (size + mtimeMs + last entry id) so materialize
// boundaries can classify external changes:
//
//   - append   — size grew, tail entry id changed → benign; tail-sync sees it.
//   - truncate/rewrite — size or mtime moved against the recorded identity,
//     or the tail id regressed → dirty.
//
// `classifyExternalChange` is intentionally conservative (plan D8): the tail
// id is a fast hint, never proof — a TUI rewriteEntries() can preserve the
// last id while mutating middle content, so any size/mtime movement marks
// the record dirty and only a bounded reload (or cold reads) restores
// freshness. This is detection + absorb + recheck, not a CAS.

import fs from 'node:fs';

/** transcript 文件的廉价身份快照：体积、mtime 与捕获时的尾 entry id。 */
export interface FileSignature {
  /** 文件体积（字节）。 */
  size: number;
  /** 文件 mtime（毫秒）。 */
  mtimeMs: number;
  /** Last persisted entry id at capture time (fast hint only). */
  /** 中文补充：捕获时刻最后一条已持久化 entry 的 id（仅作快速提示，不作证明）。 */
  tailEntryId: string | null;
}

/** 外部变更的三态判定：无变化、良性追加、需要重载的脏写。 */
export type ExternalChange = 'unchanged' | 'append' | 'dirty';

/** Capture the cheap identity of a transcript file; null when unreadable. */
/** 捕获 transcript 文件的廉价身份；文件不可读（如已消失）时返回 null。 */
export function fileSignature(filePath: string, tailEntryId: string | null = null): FileSignature | null {
  try {
    const stat = fs.statSync(filePath);
    return { size: stat.size, mtimeMs: stat.mtimeMs, tailEntryId };
  } catch {
    return null;
  }
}

/**
 * Compare a recorded signature against the file's current cheap identity.
 * The tail read is lazy: it only runs when size/mtime moved and growth
 * needs the tail-id hint to distinguish append from rewrite (plan §8.1
 * step 2). A grown file whose tail id is unchanged — or unreadable — is a
 * rewrite the fast hints cannot prove benign: dirty, never append.
 */
/**
 * 中文补充：把记录过的签名与文件当前身份做保守比较。尾读取是惰性
 * 的——仅当 size/mtime 移动且需要尾 id 提示来区分"追加"与"重写"时
 * 才执行（plan §8.1 步骤 2）。增长但尾 id 未变（或读不出来）的文件
 * 是快速提示无法证明良性的重写：判 dirty，绝不判 append。
 */
export function classifyExternalChange(recorded: FileSignature | null, filePath: string): ExternalChange {
  if (!recorded) return 'unchanged';
  const current = fileSignature(filePath);
  if (!current) return 'dirty'; // vanished/unreadable — treat as dirty, not clean
  if (current.size === recorded.size && current.mtimeMs === recorded.mtimeMs) return 'unchanged';
  if (current.size > recorded.size) {
    const currentTailEntryId = tailEntryIdOf(filePath);
    // Growth with a forward-moving tail is the common external append. A
    // null recorded tail means materialize could not read one (empty or
    // torn transcript) — growth stays the only provable shape.
    if (recorded.tailEntryId === null || (currentTailEntryId !== null && currentTailEntryId !== recorded.tailEntryId)) {
      return 'append';
    }
  }
  // Same-size mtime movement, shrink, a stationary tail with changed size,
  // or a torn tail the reader cannot prove — a rewrite the fast hints
  // cannot clear.
  return 'dirty';
}

/** Read the trailing entry id from a JSONL transcript (bounded tail read). */
/**
 * 中文补充：从 JSONL transcript 的尾部窗口（默认 8192 字节）读取最后
 * 一条 entry 的 id。任何读取或解析失败（文件缺失、空文件、撕裂的尾
 * 行、非字符串 id）都返回 null，绝不抛出。
 */
export function tailEntryIdOf(filePath: string, tailBytes = 8192): string | null {
  try {
    const stat = fs.statSync(filePath);
    const fd = fs.openSync(filePath, 'r');
    try {
      const start = Math.max(0, stat.size - tailBytes);
      const length = stat.size - start;
      const buffer = Buffer.alloc(length);
      const read = fs.readSync(fd, buffer, 0, length, start);
      const text = buffer.subarray(0, read).toString('utf8');
      // 丢弃空行后取最后一行；没有非空行说明文件为空。
      const lines = text.split('\n').filter((line) => line.trim().length > 0);
      const last = lines[lines.length - 1];
      if (!last) return null;
      // SAFETY: JSON.parse yields any; transcript lines are {id} objects and
      // the prototype tag below proves the string arm before use — anything
      // else (junk, primitives, non-string ids) decodes to null.
      const parsed = JSON.parse(last) as { id?: unknown } | null;
      const id = parsed?.id;
      return Object.prototype.toString.call(id) === '[object String]' ? String(id) : null;
    } finally {
      fs.closeSync(fd);
    }
  } catch {
    return null;
  }
}
