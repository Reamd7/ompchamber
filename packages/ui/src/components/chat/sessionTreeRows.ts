import type { OmpSessionTreeNode, OmpSessionTreeSnapshot } from '@/lib/api/omp';

interface BranchRow {
    node: OmpSessionTreeNode;
    depth: number;
}

/** Depth-first rows, siblings ordered oldest→newest so forks read top-down. */
export const buildBranchRows = (snapshot: OmpSessionTreeSnapshot): BranchRow[] => {
    const byParent = new Map<string, OmpSessionTreeNode[]>();
    const roots: OmpSessionTreeNode[] = [];
    const known = new Set(snapshot.nodes.map((node) => node.id));
    for (const node of snapshot.nodes) {
        if (node.parentId && known.has(node.parentId)) {
            const siblings = byParent.get(node.parentId) ?? [];
            siblings.push(node);
            byParent.set(node.parentId, siblings);
        } else {
            roots.push(node);
        }
    }
    const byCreated = (a: OmpSessionTreeNode, b: OmpSessionTreeNode) =>
        (a.time.created ?? 0) - (b.time.created ?? 0);
    roots.sort(byCreated);
    for (const siblings of byParent.values()) siblings.sort(byCreated);

    const rows: BranchRow[] = [];
    const walk = (node: OmpSessionTreeNode, depth: number) => {
        rows.push({ node, depth });
        for (const child of byParent.get(node.id) ?? []) walk(child, depth + 1);
    };
    for (const root of roots) walk(root, 0);
    return rows;
};
