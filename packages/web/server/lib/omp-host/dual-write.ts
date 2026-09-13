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

export interface FileSignature {
  size: number;
  mtimeMs: number;
  /** Last persisted entry id at capture time (fast hint only). */
  tailEntryId: string | null;
}

export type ExternalChange = 'unchanged' | 'append' | 'dirty';

/** Capture the cheap identity of a transcript file; null when unreadable. */
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
