/**
 * localFileTree row assembly (spec 04 artifacts browse): flat (sessionID,
 * ref) file rows become depth-first tree rows — directories before files,
 * both alphabetical, matching the files tree's reading order. Collapse hides
 * a directory's subtree entirely.
 */
import { describe, expect, test } from 'bun:test';
import { buildLocalFileRows } from '../localFileTree';
import type { OmpArtifactsFileRow } from '@/lib/api/omp';

const file = (ref: string, modifiedAt = 0): OmpArtifactsFileRow => ({
  ref,
  size: 10,
  modifiedAt,
});

describe('buildLocalFileRows', () => {
  test('directories sort before files, both alphabetical, depths nested', () => {
    const rows = buildLocalFileRows(
      [file('PLAN.md'), file('scratch/b.md'), file('notes/a.md'), file('scratch/a.md')],
      new Set(),
    );
    expect(rows.map((row) => `${row.kind}:${row.ref}`)).toEqual([
      'dir:notes',
      'file:notes/a.md',
      'dir:scratch',
      'file:scratch/a.md',
      'file:scratch/b.md',
      'file:PLAN.md',
    ]);
    expect(rows.map((row) => row.depth)).toEqual([0, 1, 0, 1, 1, 0]);
  });

  test('collapsed directory hides its entire subtree', () => {
    const rows = buildLocalFileRows(
      [file('scratch/a.md'), file('scratch/deep/b.md'), file('PLAN.md')],
      new Set(['scratch']),
    );
    expect(rows.map((row) => row.ref)).toEqual(['scratch', 'PLAN.md']);
  });

  test('empty input yields no rows', () => {
    expect(buildLocalFileRows([], new Set())).toEqual([]);
  });
});
