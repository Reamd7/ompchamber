/**
 * SessionTreeDialog row assembly (spec 04 §5.4 GAP-04): the flat
 * {leafId, nodes} fork-lineage snapshot becomes depth-first rows (siblings
 * ordered oldest→newest so forks read top-down); parent edges outside the
 * node set (pruned lineage, cross-directory parents — server projects them
 * to null) become roots instead of dangling references.
 */
import { describe, expect, test } from 'bun:test';
import { buildBranchRows } from '../SessionTreeDialog';
import type { OmpSessionTreeNode } from '@/lib/api/omp';

const node = (id: string, parentId: string | null, created: number): OmpSessionTreeNode => ({
  id,
  parentId,
  title: id,
  time: { created, updated: created },
});

describe('buildBranchRows', () => {
  test('depth-first rows, siblings ordered by creation, depths nested', () => {
    const rows = buildBranchRows({
      leafId: 'ses_a2',
      nodes: [
        node('ses_root', null, 1),
        node('ses_b', 'ses_root', 3),
        node('ses_a', 'ses_root', 2),
        node('ses_a2', 'ses_a', 4),
      ],
    });
    expect(rows.map((row) => row.node.id)).toEqual(['ses_root', 'ses_a', 'ses_a2', 'ses_b']);
    expect(rows.map((row) => row.depth)).toEqual([0, 1, 2, 1]);
  });

  test('parents outside the node set resolve to roots (server nulls foreign lineage)', () => {
    const rows = buildBranchRows({
      leafId: 'ses_x',
      nodes: [node('ses_x', 'ses_elsewhere', 5), node('ses_root', null, 1)],
    });
    expect(rows.map((row) => row.node.id)).toEqual(['ses_root', 'ses_x']);
    expect(rows.every((row) => row.depth === 0 || rows[0]?.node.id === 'ses_root')).toBe(true);
  });

  test('empty snapshot yields no rows', () => {
    expect(buildBranchRows({ leafId: null, nodes: [] })).toEqual([]);
  });
});
