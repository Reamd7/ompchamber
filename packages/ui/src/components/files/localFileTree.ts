/**
 * local:// file-tree row assembly (spec 04 artifacts browse).
 *
 * Flat (sessionID, ref) rows become depth-first tree rows — directories
 * before files, both alphabetical, matching the files tree's reading order.
 * Shared by the context-panel session tree; refs never contain absolute
 * paths (R7).
 */

import type { OmpArtifactsFileRow } from '@/lib/api/omp';

export interface LocalFileRow {
  kind: 'dir' | 'file';
  /** Full ref ('scratch/notes.md') for files; directory prefix for dirs. */
  ref: string;
  name: string;
  depth: number;
  size?: number;
  modifiedAt?: number;
}

export const buildLocalFileRows = (
  files: readonly OmpArtifactsFileRow[],
  collapsedDirs: ReadonlySet<string>,
): LocalFileRow[] => {
  interface NamedFile {
    ref: string;
    size: number;
    modifiedAt: number;
    name: string;
  }
  interface DirNode {
    name: string;
    ref: string;
    depth: number;
    dirs: Map<string, DirNode>;
    files: NamedFile[];
  }
  const root: DirNode = { name: '', ref: '', depth: -1, dirs: new Map(), files: [] };
  for (const file of files) {
    const segments = file.ref.split('/');
    let cursor = root;
    for (let index = 0; index < segments.length - 1; index += 1) {
      const segment = segments[index];
      if (segment.length === 0) continue;
      let next = cursor.dirs.get(segment);
      if (!next) {
        next = {
          name: segment,
          ref: cursor.ref ? `${cursor.ref}/${segment}` : segment,
          depth: cursor.depth + 1,
          dirs: new Map(),
          files: [],
        };
        cursor.dirs.set(segment, next);
      }
      cursor = next;
    }
    cursor.files.push({ ...file, name: segments[segments.length - 1] ?? file.ref });
  }
  const byName = (a: NamedFile | DirNode, b: NamedFile | DirNode) => a.name.localeCompare(b.name);
  const rows: LocalFileRow[] = [];
  const walk = (node: DirNode) => {
    for (const dir of [...node.dirs.values()].sort(byName)) {
      rows.push({ kind: 'dir', ref: dir.ref, name: dir.name, depth: dir.depth });
      if (!collapsedDirs.has(dir.ref)) walk(dir);
    }
    for (const file of [...node.files].sort(byName)) {
      rows.push({
        kind: 'file',
        ref: node.ref ? `${node.ref}/${file.name}` : file.name,
        name: file.name,
        depth: node.depth + 1,
        size: file.size,
        modifiedAt: file.modifiedAt,
      });
    }
  };
  walk(root);
  return rows;
};

export const formatLocalFileBytes = (bytes: number): string => {
  if (bytes < 1024) return `${bytes} B`;
  const kib = bytes / 1024;
  if (kib < 1024) return `${kib.toFixed(1)} KiB`;
  return `${(kib / 1024).toFixed(1)} MiB`;
};
