import { beforeEach, describe, expect, test } from 'bun:test';

import type { OmpDialogRespondResult, OmpPendingDialog } from '@/lib/api/omp';
import type { OmpDialogController } from '@/sync/omp-dialog-controller';
import { useOmpDialogStore } from '@/sync/useOmpDialogStore';

import { buildSnapshot, handleTrayAction, type TrayAction } from './useTraySync';

const approvalDialog = (id: string, sessionId: string, createdAt: number, toolName?: string): OmpPendingDialog => ({
  id,
  sessionId,
  createdAt,
  kind: 'approval',
  approval: { prompt: 'Run a command', ...(toolName ? { toolName } : {}) },
});

const askDialog = (id: string, sessionId: string, createdAt: number): OmpPendingDialog => ({
  id,
  sessionId,
  createdAt,
  kind: 'ask',
  ask: {
    questions: [{ id: 'q1', question: 'Proceed?', options: [{ label: 'Yes' }] }],
    timeoutMs: 60000,
  },
});

const selectDialog = (id: string, sessionId: string, createdAt: number): OmpPendingDialog => ({
  id,
  sessionId,
  createdAt,
  kind: 'select',
  select: { title: 'Pick one', options: ['A', 'B'] },
});

const seedDirectories = (directories: Record<string, OmpPendingDialog[]>): void => {
  const slices: Record<string, { dialogs: Record<string, OmpPendingDialog>; ui: Record<string, never>; tombstones: Record<string, never> }> = {};
  for (const [directory, dialogs] of Object.entries(directories)) {
    slices[directory] = {
      dialogs: Object.fromEntries(dialogs.map((dialog) => [dialog.id, dialog])),
      ui: {},
      tombstones: {},
    };
  }
  useOmpDialogStore.setState({ directories: slices });
};

describe('useTraySync buildSnapshot omp dialogs', () => {
  beforeEach(() => {
    useOmpDialogStore.setState({ directories: {} });
  });

  test('aggregates approval/ask across directories, oldest first with id tiebreak, other kinds excluded', () => {
    seedDirectories({
      '/repo-a': [
        askDialog('dlg_a1', 'ses_ALPHA123', 10),
        approvalDialog('dlg_a2', 'ses_BETA456', 30, 'Bash'),
        selectDialog('dlg_a3', 'ses_ALPHA123', 15),
      ],
      '/repo-b': [
        approvalDialog('dlg_b2', 'ses_GAMMA789', 20),
        approvalDialog('dlg_b1', 'ses_GAMMA789', 20),
      ],
    });

    const snapshot = buildSnapshot('Local OMPChamber');

    expect(snapshot.ompDialogs.map((d) => d.dialogId)).toEqual(['dlg_a1', 'dlg_b1', 'dlg_b2', 'dlg_a2']);
    expect(snapshot.ompDialogs.map((d) => d.kind)).toEqual(['ask', 'approval', 'approval', 'approval']);

    const [askRow, plainApproval, , toolApproval] = snapshot.ompDialogs;
    expect(askRow.directory).toBe('/repo-a');
    expect(askRow.sessionId).toBe('ses_ALPHA123');
    expect(askRow.toolName).toBe(undefined);
    expect(askRow.label).toBe('Session ALPHA123 — questions waiting');
    expect(askRow.approveLabel).toBe('Approve');
    expect(askRow.denyLabel).toBe('Deny');

    expect(plainApproval.directory).toBe('/repo-b');
    expect(plainApproval.toolName).toBe(undefined);
    expect(plainApproval.label).toBe('Session GAMMA789 — approval needed');

    expect(toolApproval.toolName).toBe('Bash');
    expect(toolApproval.label).toBe('Session BETA456 — Bash needs approval');
  });

  test('empty dialog store yields no omp tray rows without disturbing the rest of the snapshot', () => {
    const snapshot = buildSnapshot('Local OMPChamber');
    expect(snapshot.sessions).toEqual([]);
    expect(snapshot.approvals).toEqual([]);
    expect(snapshot.instanceName).toBe('Local OMPChamber');
    expect(snapshot.dockBadgeCount).toBe(0);
  });
});

describe('useTraySync handleTrayAction omp routing', () => {
  const makeController = (results: Array<{ directory: string; dialogId: string; result: OmpDialogRespondResult }>): OmpDialogController => ({
    respond: async (directory, dialogId, result) => {
      results.push({ directory, dialogId, result });
      return { ok: true };
    },
    presented: async () => {},
    abort: async () => {},
    reconcile: async () => {},
    alwaysAllowAndApprove: async () => ({ ok: true }),
    writeAlwaysAllow: async () => ({ ok: true }),
  });

  // The routing promise chain is all microtasks; one yield drains it without
  // a real timer. The stub records synchronously, so this only future-proofs
  // the assertion.
  const flush = async (): Promise<void> => {
    await Promise.resolve();
  };

  test('routes Approve/Deny tray clicks to the omp dialog controller as select responds', async () => {
    const calls: Array<{ directory: string; dialogId: string; result: OmpDialogRespondResult }> = [];
    const controller = makeController(calls);

    const denyAction: TrayAction = {
      type: 'respond-omp-dialog',
      directory: '/repo-a',
      sessionId: 'ses_ALPHA123',
      dialogId: 'dlg_1',
      response: 'Deny',
    };
    handleTrayAction(denyAction, controller);

    const approveAction: TrayAction = {
      type: 'respond-omp-dialog',
      directory: '/repo-b',
      sessionId: 'ses_BETA456',
      dialogId: 'dlg_2',
      response: 'Approve',
    };
    handleTrayAction(approveAction, controller);

    await flush();

    expect(calls).toEqual([
      { directory: '/repo-a', dialogId: 'dlg_1', result: { kind: 'select', value: 'Deny' } },
      { directory: '/repo-b', dialogId: 'dlg_2', result: { kind: 'select', value: 'Approve' } },
    ]);
  });
});
