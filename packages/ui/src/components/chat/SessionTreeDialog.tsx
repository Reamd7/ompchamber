import React from 'react';
import {
    Dialog,
    DialogContent,
    DialogDescription,
    DialogHeader,
    DialogTitle,
} from '@/components/ui/dialog';
import { Icon } from '@/components/icon/Icon';
import { getCurrentIntlLocale, useI18n } from '@/lib/i18n';
import { useSessionUIStore } from '@/sync/session-ui-store';
import { createOmpTreeAPI, type OmpSessionTreeNode, type OmpSessionTreeSnapshot } from '@/lib/api/omp';
import { cn } from '@/lib/utils';

/**
 * Session branch tree (spec 04 §5.4 GAP-04) — the TreeDialog consumer of
 * `GET /api/omp/sessions/{id}/tree`. The server projection is the session's
 * fork lineage: the ancestor chain plus every descendant fork, as a flat
 * `{leafId, nodes}` array the dialog assembles into rows.
 *
 * Selection semantics (GAP-05 folded here): the tree domain does not expose a
 * navigate channel yet (no POST …/tree/navigate — the two-stage ask contract
 * has no server route), so selecting a branch switches the active session to
 * that node and re-pulls its timeline through the normal session-switch
 * machinery (`setCurrentSession` → `fetchMessagesForSession`). Wire navigate
 * in when the engine grows it.
 */

const treeApi = createOmpTreeAPI();

type TreeState =
    | { phase: 'loading' }
    | { phase: 'error' }
    | { phase: 'ready'; snapshot: OmpSessionTreeSnapshot };

interface SessionTreeDialogProps {
    open: boolean;
    onOpenChange: (open: boolean) => void;
    sessionId: string | null;
    directory: string | null;
}

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

export const SessionTreeDialog: React.FC<SessionTreeDialogProps> = ({
    open,
    onOpenChange,
    sessionId,
    directory,
}) => {
    const { t } = useI18n();
    const setCurrentSession = useSessionUIStore((state) => state.setCurrentSession);
    const [state, setState] = React.useState<TreeState>({ phase: 'loading' });
    // Generation token: a session/directory switch mid-fetch must not let the
    // stale response clobber the new tree.
    const fetchEpoch = React.useRef(0);

    React.useEffect(() => {
        if (!open || !sessionId || !directory) return;
        const epoch = ++fetchEpoch.current;
        setState({ phase: 'loading' });
        void treeApi.getSessionTree(sessionId, { directory }).then((result) => {
            if (fetchEpoch.current !== epoch) return;
            setState(result.ok ? { phase: 'ready', snapshot: result.data } : { phase: 'error' });
        });
    }, [open, sessionId, directory]);

    const rows = state.phase === 'ready' ? buildBranchRows(state.snapshot) : [];
    const hasForks = rows.length > 1;

    const selectBranch = (nodeId: string) => {
        if (!directory || nodeId === sessionId) {
            onOpenChange(false);
            return;
        }
        // Timeline re-pull rides the session switch (fetchMessagesForSession).
        setCurrentSession(nodeId, directory);
        onOpenChange(false);
    };

    return (
        <Dialog open={open} onOpenChange={onOpenChange}>
            <DialogContent className="max-h-[70vh] sm:max-w-[540px] flex flex-col">
                <DialogHeader>
                    <DialogTitle className="flex items-center gap-2">
                        <Icon name="git-branch" className="size-4" />
                        {t('chat.sessionTree.title')}
                    </DialogTitle>
                    <DialogDescription>{t('chat.sessionTree.description')}</DialogDescription>
                </DialogHeader>
                <div className="min-h-0 flex-1 overflow-y-auto" role="tree">
                    {state.phase === 'loading' ? (
                        <div className="flex items-center justify-center gap-2 py-6 text-muted-foreground">
                            <Icon name="refresh" className="size-4 animate-spin" />
                            <span className="typography-meta">{t('chat.sessionTree.loading')}</span>
                        </div>
                    ) : state.phase === 'error' ? (
                        <div className="px-1 py-6 text-center typography-meta text-muted-foreground">
                            {t('chat.sessionTree.error')}
                        </div>
                    ) : !hasForks ? (
                        <div className="px-1 py-6 text-center typography-meta text-muted-foreground">
                            {t('chat.sessionTree.empty')}
                        </div>
                    ) : rows.map(({ node, depth }) => {
                        const isCurrent = node.id === sessionId;
                        const isLeaf = state.phase === 'ready' && state.snapshot.leafId === node.id;
                        const label = node.title?.trim() || t('chat.sessionTree.untitled');
                        const updated = node.time.updated
                            ? new Date(node.time.updated).toLocaleString(getCurrentIntlLocale(), {
                                month: 'short',
                                day: 'numeric',
                                hour: 'numeric',
                                minute: '2-digit',
                            })
                            : null;
                        return (
                            <button
                                key={node.id}
                                type="button"
                                role="treeitem"
                                aria-selected={isCurrent}
                                aria-label={t('chat.sessionTree.openBranch', { title: label })}
                                onClick={() => selectBranch(node.id)}
                                className={cn(
                                    'flex w-full items-center gap-2 rounded-lg px-2 py-2 text-left hover:bg-interactive-selection',
                                    isCurrent && 'bg-interactive-selection',
                                )}
                                style={{ paddingLeft: `${0.5 + depth * 1.25}rem` }}
                            >
                                <Icon
                                    name={depth === 0 ? 'git-commit' : 'git-branch'}
                                    className="size-3.5 shrink-0 text-muted-foreground"
                                />
                                <span className="min-w-0 flex-1 truncate typography-ui-label">{label}</span>
                                {updated ? (
                                    <span className="shrink-0 typography-meta text-muted-foreground">{updated}</span>
                                ) : null}
                                {isLeaf ? (
                                    <span className="shrink-0 typography-meta text-muted-foreground">
                                        {t('chat.sessionTree.leaf')}
                                    </span>
                                ) : null}
                                {isCurrent ? (
                                    <span className="shrink-0 typography-meta font-medium text-primary">
                                        {t('chat.sessionTree.current')}
                                    </span>
                                ) : null}
                            </button>
                        );
                    })}
                </div>
            </DialogContent>
        </Dialog>
    );
};
