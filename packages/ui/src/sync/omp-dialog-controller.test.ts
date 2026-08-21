import { beforeEach, describe, expect, test } from 'bun:test';
import type { OmpDialogRespondResult, OmpDialogRespondResult as Respond, OmpDialogsAPI, OmpPendingDialog } from '@/lib/api/omp';
import { createOmpDialogController, type OmpDialogController } from './omp-dialog-controller';
import { useOmpDialogStore } from './useOmpDialogStore';

const approvalDialog = (id: string): OmpPendingDialog => ({
  id,
  sessionId: 'ses_1',
  createdAt: 1000,
  kind: 'approval',
  approval: { prompt: 'Allow bash?', toolName: 'bash' },
}) as OmpPendingDialog;

interface ApiLog {
  respond: Array<{ dialogId: string; result: Respond }>;
  presented: string[];
  snapshots: number;
  released: string[];
}

const createStubApi = (script: {
  respondOutcome?: 'ok' | 'conflict' | 'error' | 'unavailable';
  presentedOk?: boolean;
  snapshotDialogs?: OmpPendingDialog[];
  settingsWriteOk?: boolean;
}): { api: OmpDialogsAPI; log: ApiLog } => {
  const log: ApiLog = { respond: [], presented: [], snapshots: 0, released: [] };
  return {
    log,
    api: {
      getSnapshot: async () => {
        log.snapshots += 1;
        return { ok: true, dialogs: script.snapshotDialogs ?? [] };
      },
      acquireLease: async () => ({ ok: false, unavailable: true }),
      releaseLease: async () => ({ ok: true }),
      presented: async (_directory, dialogId) => {
        log.presented.push(dialogId);
        return script.presentedOk === false ? { ok: false, unavailable: false } : { ok: true, presentedAt: 5 };
      },
      respond: async (_directory, dialogId, result) => {
        log.respond.push({ dialogId, result });
        if (script.respondOutcome === 'conflict') {
          return { ok: false, unavailable: false, status: 409, error: 'already-settled', outcome: 'responded' };
        }
        if (script.respondOutcome === 'error') {
          return { ok: false, unavailable: false, status: 500, error: 'boom' };
        }
        if (script.respondOutcome === 'unavailable') {
          return { ok: false, unavailable: true };
        }
        return { ok: true, outcome: 'responded' };
      },
      abort: async () => ({ ok: true }),
    },
  };
};

const seedDialog = (dialog: OmpPendingDialog): void => {
  useOmpDialogStore.setState((state) => ({
    directories: {
      ...state.directories,
      '/repo': {
        dialogs: { ...(state.directories['/repo']?.dialogs ?? {}), [dialog.id]: dialog },
        ui: { ...(state.directories['/repo']?.ui ?? {}), [dialog.id]: { respondInflight: false, presentedAckSent: false } },
        tombstones: state.directories['/repo']?.tombstones ?? {},
      },
    },
  }));
};

const dialogExists = (dialogId: string): boolean =>
  useOmpDialogStore.getState().directories['/repo']?.dialogs[dialogId] !== undefined;

const settingsWrites: Array<{ directory: string; changes: Record<string, unknown> }> = [];

const buildController = (script: Parameters<typeof createStubApi>[0]): { controller: OmpDialogController; log: ApiLog } => {
  const { api, log } = createStubApi(script);
  const controller = createOmpDialogController({
    api,
    getRuntimeKey: () => 'rt_1',
    settingsWrite: async (directory, changes) => {
      settingsWrites.push({ directory, changes });
      return script.settingsWriteOk === false ? { ok: false, error: 'write-failed' } : { ok: true };
    },
  });
  return { controller, log };
};

beforeEach(() => {
  settingsWrites.length = 0;
  useOmpDialogStore.setState({ directories: {}, runtimeKey: 'rt_1' });
});

describe('omp dialog controller — respond discipline', () => {
  test('success settles locally (optimistic; SSE echo idempotent via tombstone)', async () => {
    seedDialog(approvalDialog('dlg_a'));
    const { controller, log } = buildController({});
    const result = await controller.respond('/repo', 'dlg_a', { kind: 'select', value: 'Deny' });
    expect(result.ok).toBe(true);
    expect(dialogExists('dlg_a')).toBe(false);
    expect(log.respond).toEqual([{ dialogId: 'dlg_a', result: { kind: 'select', value: 'Deny' } }]);
  });

  test('409 conflict reconciles from the snapshot then settles — card never lingers', async () => {
    seedDialog(approvalDialog('dlg_a'));
    const { controller, log } = buildController({ respondOutcome: 'conflict' });
    const result = await controller.respond('/repo', 'dlg_a', { kind: 'select', value: 'Approve' });
    expect(result).toEqual({ ok: false, conflict: true });
    expect(log.snapshots).toBe(1);
    expect(dialogExists('dlg_a')).toBe(false);
  });

  test('transport failure keeps the dialog open with the error surfaced and inflight cleared', async () => {
    seedDialog(approvalDialog('dlg_a'));
    const { controller } = buildController({ respondOutcome: 'error' });
    const result = await controller.respond('/repo', 'dlg_a', { kind: 'select', value: 'Approve' });
    expect(result).toEqual({ ok: false, conflict: false, error: 'boom' });
    const ui = useOmpDialogStore.getState().directories['/repo']?.ui['dlg_a'];
    expect(ui?.respondInflight).toBe(false);
    expect(ui?.respondError).toBe('boom');
    expect(dialogExists('dlg_a')).toBe(true);
  });

  test('concurrent responds are serialized by the inflight guard', async () => {
    seedDialog(approvalDialog('dlg_a'));
    const { controller, log } = buildController({});
    const first = controller.respond('/repo', 'dlg_a', { kind: 'select', value: 'Approve' });
    const second = controller.respond('/repo', 'dlg_a', { kind: 'select', value: 'Deny' });
    await Promise.all([first, second]);
    expect(log.respond).toHaveLength(1);
  });
});

describe('omp dialog controller — presented-ack', () => {
  test('sent exactly once per activation; failure re-arms for the next attempt', async () => {
    seedDialog(approvalDialog('dlg_a'));
    const { controller, log } = buildController({ presentedOk: false });
    await controller.presented('/repo', 'dlg_a');
    await controller.presented('/repo', 'dlg_a');
    expect(log.presented).toHaveLength(2); // failed → re-armed → retried
    // A successful activation: exactly one ack, then no-ops.
    const { controller: okController, log: okLog } = buildController({});
    // reset ack state by marking unsent (store action)
    useOmpDialogStore.getState().markPresentedAck('rt_1', '/repo', 'dlg_a', false);
    await okController.presented('/repo', 'dlg_a');
    await okController.presented('/repo', 'dlg_a');
    expect(okLog.presented).toHaveLength(1);
  });
});

describe('omp dialog controller — always-allow transaction (R10 order)', () => {
  test('settings write succeeds → Approve respond follows; payload keys exact', async () => {
    seedDialog(approvalDialog('dlg_a'));
    const { controller, log } = buildController({});
    const result = await controller.alwaysAllowAndApprove('/repo', 'dlg_a', 'bash');
    expect(result.ok).toBe(true);
    expect(settingsWrites).toEqual([
      { directory: '/repo', changes: { 'tools.approval.bash': 'allow' } },
    ]);
    expect(log.respond).toEqual([{ dialogId: 'dlg_a', result: { kind: 'select', value: 'Approve' } }]);
  });

  test('settings write fails → NO respond, dialog stays open (half-commit forbidden)', async () => {
    seedDialog(approvalDialog('dlg_a'));
    const { controller, log } = buildController({ settingsWriteOk: false });
    const result = await controller.alwaysAllowAndApprove('/repo', 'dlg_a', 'bash');
    expect(result).toEqual({ ok: false, conflict: false, error: 'write-failed' });
    expect(log.respond).toHaveLength(0);
    expect(dialogExists('dlg_a')).toBe(true);
  });

  test('write ok + respond 409 → converges settled; the write is NOT repeated', async () => {
    seedDialog(approvalDialog('dlg_a'));
    const { controller, log } = buildController({ respondOutcome: 'conflict' });
    const result = await controller.alwaysAllowAndApprove('/repo', 'dlg_a', 'bash');
    expect(result).toEqual({ ok: false, conflict: true });
    expect(settingsWrites).toHaveLength(1);
    expect(log.respond).toHaveLength(1);
    expect(dialogExists('dlg_a')).toBe(false);
  });
});
