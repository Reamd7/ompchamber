import { beforeEach, describe, expect, test } from 'bun:test';
import React from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import type { OmpPendingDialog } from '@/lib/api/omp';
import { I18nProvider } from '@/lib/i18n';
import { useOmpDialogStore } from '@/sync/useOmpDialogStore';
import { OmpApprovalDialog } from '../OmpApprovalDialog';
import { OmpAskDialogModal } from '../OmpAskDialogModal';

const render = (element: React.ReactElement): string =>
  renderToStaticMarkup(<I18nProvider>{element}</I18nProvider>);

const seed = (dialog: OmpPendingDialog, ui?: { respondInflight?: boolean }): void => {
  useOmpDialogStore.setState((state) => ({
    directories: {
      ...state.directories,
      '/repo': {
        dialogs: { ...(state.directories['/repo']?.dialogs ?? {}), [dialog.id]: dialog },
        ui: {
          ...(state.directories['/repo']?.ui ?? {}),
          [dialog.id]: { respondInflight: ui?.respondInflight ?? false, presentedAckSent: true },
        },
        tombstones: state.directories['/repo']?.tombstones ?? {},
      },
    },
  }));
};

beforeEach(() => {
  useOmpDialogStore.setState({ directories: {}, runtimeKey: 'rt' });
});

describe('OmpApprovalDialog — render contract (spec 03 §5.3.1)', () => {
  const approval: Extract<OmpPendingDialog, { kind: 'approval' }> = {
    id: 'dlg_a',
    sessionId: 'ses_1',
    createdAt: 100,
    kind: 'approval',
    approval: { prompt: 'Allow bash to run:\n\trm -rf /tmp/x --dry', toolName: 'bash', tier: 'write' },
  };

  test('body renders the server prompt verbatim (TUI checksum anchor)', () => {
    seed(approval);
    const markup = render(<OmpApprovalDialog directory="/repo" dialog={approval} />);
    expect(markup).toContain('Allow bash to run:');
    expect(markup).toContain('rm -rf /tmp/x --dry');
    expect(markup).toContain('bash');
  });

  test('button set is exactly Approve/Deny (no third action button)', () => {
    seed(approval);
    const markup = render(<OmpApprovalDialog directory="/repo" dialog={approval} />);
    expect(markup).toContain('Approve');
    expect(markup).toContain('Deny');
    // The advanced action is an icon-only overflow button, never a labeled
    // third action next to Approve/Deny.
    expect((markup.match(/>Approve</g) ?? []).length).toBe(1);
    expect((markup.match(/>Deny</g) ?? []).length).toBe(1);
  });

  test('inflight disables both buttons', () => {
    seed(approval, { respondInflight: true });
    const markup = render(<OmpApprovalDialog directory="/repo" dialog={approval} />);
    const disabled = (markup.match(/disabled/g) ?? []).length;
    expect(disabled).toBeGreaterThanOrEqual(2);
  });
});

describe('OmpAskDialogModal — render contract (spec 03 §5.4.1)', () => {
  const askMulti: Extract<OmpPendingDialog, { kind: 'ask' }> = {
    id: 'dlg_b',
    sessionId: 'ses_1',
    createdAt: 200,
    kind: 'ask',
    ask: {
      questions: [{
        id: 'q1',
        question: 'Which database?',
        options: [
          { label: 'Postgres', description: 'relational' },
          { label: 'SQLite' },
        ],
        multi: true,
        recommended: 'Postgres',
      }],
      timeoutMs: 0,
    },
  };

  const askSingle: Extract<OmpPendingDialog, { kind: 'ask' }> = {
    id: 'dlg_c',
    sessionId: 'ses_1',
    createdAt: 300,
    kind: 'ask',
    ask: {
      questions: [{
        id: 'q1',
        question: 'Proceed?',
        options: [{ label: 'Yes' }, { label: 'No' }],
      }],
      timeoutMs: 0,
    },
  };

  test('multi renders checkbox shapes + Recommended badge on the right option', () => {
    seed(askMulti);
    const markup = render(<OmpAskDialogModal directory="/repo" dialog={askMulti} />);
    expect(markup).toContain('Which database?');
    expect(markup).toContain('role="checkbox"');
    expect(markup).toContain('Recommended');
    // The badge sits on the Postgres option row, not SQLite's.
    const postgresRow = markup.split('SQLite')[0];
    expect(postgresRow).toContain('Recommended');
    expect(markup).toContain('Other (type your own)');
    expect(markup).toContain('Chat about this');
  });

  test('single renders radio shapes; submit is disabled until answered', () => {
    seed(askSingle);
    const markup = render(<OmpAskDialogModal directory="/repo" dialog={askSingle} />);
    expect(markup).toContain('role="radio"');
    const submitIndex = markup.lastIndexOf('Submit');
    expect(submitIndex).toBeGreaterThan(0);
    expect(markup.slice(Math.max(0, submitIndex - 220), submitIndex)).toContain('disabled');
  });

  test('multi submit is enabled immediately ("select none" is legal)', () => {
    seed(askMulti);
    const markup = render(<OmpAskDialogModal directory="/repo" dialog={askMulti} />);
    const submitIndex = markup.lastIndexOf('Submit');
    expect(markup.slice(Math.max(0, submitIndex - 220), submitIndex)).not.toContain('disabled');
  });
});
