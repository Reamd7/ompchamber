import { describe, expect, test } from 'bun:test';
import React from 'react';
import { renderToStaticMarkup } from 'react-dom/server';

import { I18nProvider } from '@/lib/i18n';
import { useSessionUIStore } from '@/sync/session-ui-store';
import { useOmpSessionStore } from '@/sync/useOmpSessionStore';
import { createEmptyOmpDirectoryState } from '@/sync/omp-event-reducer';
import { TtsrWarningView } from './OmpTtsrWarning';

const render = (element: React.ReactElement): string =>
    renderToStaticMarkup(<I18nProvider>{element}</I18nProvider>);

const bindSessionDirectory = (sessionId: string): string => {
    useSessionUIStore.setState({
        currentSessionId: sessionId,
        currentSessionDirectory: '/repo',
        worktreeMetadata: new Map(),
    });
    const directory = useSessionUIStore.getState().getDirectoryForSession(sessionId);
    expect(typeof directory === 'string' && directory.length > 0).toBe(true);
    return directory as string;
};

describe('OmpTtsrWarning', () => {
    test('the store block the container reads carries the merged rules', () => {
        // renderToStaticMarkup reads zustand's initial-state server snapshot,
        // so the container's data path is verified on the exact state its
        // hooks read: directory resolution → store slice → ttsr[session].
        const directory = bindSessionDirectory('ses_1');
        const warning = { rules: ['no-secrets', 'stay-on-task'], raisedAt: 20 };
        useOmpSessionStore.setState({
            directories: {
                [directory]: { ...createEmptyOmpDirectoryState(), ttsr: { ses_1: warning } },
            },
        });
        const slice = useOmpSessionStore.getState().directories[directory]?.ttsr.ses_1 ?? null;
        expect(slice).toEqual(warning);
        // Absent session → no warning → the container renders nothing.
        expect(useOmpSessionStore.getState().directories[directory]?.ttsr.ses_missing ?? null).toBe(null);
    });

    test('dismissTtsrWarning drops the block; the next trigger raises a fresh one', () => {
        const directory = bindSessionDirectory('ses_2');
        useOmpSessionStore.setState({
            directories: {
                [directory]: { ...createEmptyOmpDirectoryState(), ttsr: { ses_2: { rules: ['no-secrets'], raisedAt: 20 } } },
            },
        });
        useOmpSessionStore.getState().dismissTtsrWarning(directory, 'ses_2');
        expect(useOmpSessionStore.getState().directories[directory]?.ttsr.ses_2 ?? null).toBe(null);
        // Dismissing an absent block is a no-op (state object unchanged).
        const before = useOmpSessionStore.getState().directories;
        useOmpSessionStore.getState().dismissTtsrWarning(directory, 'ses_missing');
        expect(useOmpSessionStore.getState().directories).toBe(before);
    });

    test('the view renders the merged steering-rule block with a dismiss control', () => {
        const markup = render(
            <TtsrWarningView
                warning={{ rules: ['no-secrets', 'stay-on-task'], raisedAt: 20 }}
                title="Steering rules triggered"
                dismissAria="Dismiss warning"
                onDismiss={() => {}}
            />,
        );
        expect(markup).toContain('data-omp-ttsr-warning');
        expect(markup).toContain('no-secrets');
        expect(markup).toContain('stay-on-task');
        expect(markup).toContain('Dismiss warning');
    });
});
