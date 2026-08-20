import { describe, expect, test } from 'bun:test';
import React from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import type { Message, Part } from '@/lib/opencode/wire';

import { I18nProvider } from '@/lib/i18n';
import { useSessionUIStore } from '@/sync/session-ui-store';
import { useOmpSessionStore } from '@/sync/useOmpSessionStore';
import { createEmptyOmpDirectoryState, type OmpDirectoryState } from '@/sync/omp-event-reducer';
import {
    cardChromeFrom,
    OmpCustomCardView,
    OmpCustomMessage,
    parseOmpCustomMessage,
    SummaryDividerView,
} from './OmpCustomMessage';
import { CacheMissDivider, TurnUsageRow } from './TurnUsageRow';
import { resolveTurnUsage } from './turnUsage';
const wireMessage = (id: string, text: string, metadata?: Record<string, unknown>): { info: Message; parts: Part[] } => ({
    info: {
        id,
        role: 'assistant',
        sessionID: 'ses_1',
        time: { created: 100, completed: 100 },
        ...(metadata ? { metadata } : {}),
    } as Message,
    parts: [{ id: `${id}_p1`, type: 'text', text, synthetic: true } as Part],
});

const render = (element: React.ReactElement): string =>
    renderToStaticMarkup(<I18nProvider>{element}</I18nProvider>);

/**
 * Deterministic session→directory binding: resolve the directory the same way
 * the components do, then key the omp store slice by exactly that value (the
 * path may be normalized per platform).
 */
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

const seedOmpSlice = (directory: string, slice: Partial<OmpDirectoryState>): void => {
    useOmpSessionStore.setState({
        directories: { [directory]: { ...createEmptyOmpDirectoryState(), ...slice } },
    });
};

describe('parseOmpCustomMessage', () => {
    test('classifies a single synthetic omp text part', () => {
        const { info, parts } = wireMessage('msg_1', '[omp:irc:incoming] hi');
        expect(parseOmpCustomMessage(info, parts)).toEqual({
            tier: 'T1',
            customType: 'irc:incoming',
            body: 'hi',
            messageId: 'msg_1',
            sessionId: 'ses_1',
            tokensBefore: undefined,
            warning: undefined,
            fromId: undefined,
        });
    });

    test('T4 types return null so the default text rendering stays', () => {
        const { info, parts } = wireMessage('msg_1', '[omp:some-extension] body');
        expect(parseOmpCustomMessage(info, parts)).toBe(null);
    });

    test('multi-part messages and plain text are not omp customs', () => {
        const { info, parts } = wireMessage('msg_1', 'plain assistant text');
        expect(parseOmpCustomMessage(info, parts)).toBe(null);
        expect(parseOmpCustomMessage(info, [...parts, { id: 'p2', type: 'text', text: 'more' } as Part])).toBe(null);
    });

    test('divider metadata rides along from info.metadata', () => {
        const { info, parts } = wireMessage('msg_d', '[omp:compactionSummary] summary text', {
            ompRole: 'compactionSummary',
            tokensBefore: 48_000,
            warning: 'dead end',
        });
        const parsed = parseOmpCustomMessage(info, parts);
        expect(parsed?.tier).toBe('T2');
        expect(parsed?.tokensBefore).toBe(48_000);
        expect(parsed?.warning).toBe('dead end');
    });
});

describe('T1 card chrome derivation (cardChromeFrom)', () => {
    test('advisor picks the worst severity and the first advisor name', () => {
        const chrome = cardChromeFrom('advisor', {
            customType: 'advisor',
            details: {
                notes: [
                    { note: 'a', severity: 'nit', advisor: 'code-reviewer' },
                    { note: 'b', severity: 'blocker', advisor: 'security' },
                    { note: 'c', severity: 'concern' },
                ],
            },
        });
        expect(chrome.severity).toBe('blocker');
        expect(chrome.author).toBe('code-reviewer');
    });

    test('irc author prefers details.from and falls back to attribution', () => {
        expect(cardChromeFrom('irc:incoming', {
            customType: 'irc:incoming',
            attribution: 'irc',
            details: { from: 'alice', message: 'hi' },
        }).author).toBe('alice');
        expect(cardChromeFrom('irc:relay', {
            customType: 'irc:relay',
            attribution: 'irc-relay',
        }).author).toBe('irc-relay');
    });

    test('async-result jobId from the single job or the first batched job', () => {
        expect(cardChromeFrom('async-result', {
            customType: 'async-result',
            details: { jobId: 'job_9', type: 'bash' },
        }).jobId).toBe('job_9');
        expect(cardChromeFrom('async-result', {
            customType: 'async-result',
            details: { jobs: [{ jobId: 'job_a' }, { jobId: 'job_b' }] },
        }).jobId).toBe('job_a');
    });



    test('card chrome joins omp details by wireMessageID through the resolved directory', () => {
        // renderToStaticMarkup reads zustand's initial-state server snapshot,
        // so the live join is verified on the exact state the component's
        // hooks read: directory resolution, store selector, chrome derivation.
        const directory = bindSessionDirectory('ses_1');
        seedOmpSlice(directory, {
            customDetails: {
                msg_1: { customType: 'irc:incoming', details: { from: 'alice', message: 'hello' } },
            },
        });
        const joined = useOmpSessionStore.getState().directories[directory]?.customDetails.msg_1 ?? null;
        expect(joined).not.toBe(null);
        const markup = render(<OmpCustomCardView chrome={cardChromeFrom('irc:incoming', joined)} body="hello" />);
        expect(markup).toContain('data-omp-custom-card="irc:incoming"');
        expect(markup).toContain('alice');
        expect(markup).toContain('hello');
        expect(markup).not.toContain('[omp:');
    });
    test('T1 degrades to plain text when no details joined', () => {
        bindSessionDirectory('ses_none');
        useOmpSessionStore.setState({ directories: {} });
        const { info, parts } = wireMessage('msg_2', '[omp:advisor] consider X');
        const parsed = parseOmpCustomMessage(info, parts);
        const markup = render(<OmpCustomMessage data={parsed!} />);
        expect(markup).toContain('data-omp-custom-plain="advisor"');
        expect(markup).toContain('consider X');
        expect(markup).not.toContain('data-omp-custom-card');
    });

    test('T3 renders nothing', () => {
        const { info, parts } = wireMessage('msg_3', '[omp:eager-todo-prelude] hidden');
        const parsed = parseOmpCustomMessage(info, parts);
        expect(parsed?.tier).toBe('T3');
        expect(render(<OmpCustomMessage data={parsed!} />)).toBe('');
    });
});

describe('T2 summary divider (collapse/expand)', () => {
    const dividerProps = {
        onToggle: () => undefined,
        label: 'compacted',
        summary: 'The conversation above was summarized.',
        tokensBefore: 48_000,
        warning: 'dead end ahead',
    };

    test('collapsed by default: label + tokens + warning badge, no summary body', () => {
        const markup = render(<SummaryDividerView {...dividerProps} expanded={false} />);
        expect(markup).toContain('aria-expanded="false"');
        expect(markup).toContain('compacted');
        expect(markup).toContain('from 48K tokens');
        expect(markup).toContain('⚠');
        expect(markup).not.toContain('The conversation above was summarized.');
    });

    test('expanded shows the summary detail with warning and tokens line', () => {
        const markup = render(<SummaryDividerView {...dividerProps} expanded />);
        expect(markup).toContain('aria-expanded="true"');
        expect(markup).toContain('The conversation above was summarized.');
        expect(markup).toContain('dead end ahead');
        expect(markup).toContain('from 48K tokens');
    });

    test('the stateful divider mounts collapsed by default', () => {
        const { info, parts } = wireMessage('msg_d', '[omp:branchSummary] folded branch', {
            ompRole: 'branchSummary',
            fromId: 'msg_prev',
        });
        const parsed = parseOmpCustomMessage(info, parts);
        const markup = render(<OmpCustomMessage data={parsed!} />);
        // Disclosure starts closed: summary body hidden behind the control.
        expect(markup).toContain('aria-expanded="false"');
        expect(markup).not.toContain('folded branch');
        // The control itself carries the toggle affordance.
        expect(markup).toContain('role="button"');
        expect(markup).toContain('Expand summary');
    });

    test('T2 entry renders the divider for compactionSummary metadata', () => {
        const { info, parts } = wireMessage('msg_d', '[omp:compactionSummary] summary body', {
            ompRole: 'compactionSummary',
            tokensBefore: 9_500,
        });
        const parsed = parseOmpCustomMessage(info, parts);
        const markup = render(<OmpCustomMessage data={parsed!} />);
        expect(markup).toContain('data-omp-summary-divider');
        expect(markup).toContain('compacted');
        expect(markup).toContain('from 9.5K tokens');
    });
});

describe('TurnUsageRow', () => {
    test('telemetry join resolves the turn line the row renders', () => {
        // SSR reads zustand's initial-state snapshot, so verify the data path
        // the component's hook feeds: directory → telemetry[session] → entry
        // by messageID → resolveTurnUsage.
        const directory = bindSessionDirectory('ses_1');
        seedOmpSlice(directory, {
            telemetry: {
                ses_1: [
                    {
                        messageID: 'msg_u',
                        usage: { input: 900, output: 500, cacheRead: 0, cacheWrite: 100 },
                        ttftMs: 1_250,
                        durationMs: 2_000,
                        timestamp: new Date(2026, 7, 20, 9, 5, 7).getTime(),
                    },
                ],
            },
        });
        const entry = useOmpSessionStore.getState()
            .directories[directory]?.telemetry.ses_1?.find((turn) => turn.messageID === 'msg_u');
        const info = {
            id: 'msg_u',
            role: 'assistant',
            sessionID: 'ses_1',
            tokens: { input: 1, output: 2, reasoning: 0, cache: { read: 0, write: 0 } },
        } as Message;
        const resolved = resolveTurnUsage(entry, info);
        expect(resolved?.hasTelemetry).toBe(true);
        expect(resolved?.line).toBe('2026-08-20 09:05:07  ⤵ 1K  ⤴ 500  ⏱ 1.3s  ⚡ 250.0/s');
    });

    test('degrades to tokens-only from wire info.tokens and marks it', () => {
        bindSessionDirectory('ses_1');
        useOmpSessionStore.setState({ directories: {} });
        const info = {
            id: 'msg_u',
            role: 'assistant',
            sessionID: 'ses_1',
            tokens: { input: 100, output: 200, reasoning: 0, cache: { read: 3_000, write: 50 } },
        } as Message;
        const markup = render(<TurnUsageRow sessionId="ses_1" messageId="msg_u" wireInfo={info} />);
        expect(markup).toContain('data-turn-usage-degraded="true"');
        expect(markup).toContain('⤵ 150');
        expect(markup).toContain('💾 3K');
        expect(markup).not.toContain('⏱');
        expect(markup).not.toContain('⚡');
    });

    test('renders nothing without billed usage', () => {
        bindSessionDirectory('ses_1');
        useOmpSessionStore.setState({ directories: {} });
        const info = { id: 'msg_u', role: 'assistant', sessionID: 'ses_1' } as Message;
        expect(render(<TurnUsageRow sessionId="ses_1" messageId="msg_u" wireInfo={info} />)).toBe('');
    });
});

describe('CacheMissDivider', () => {
    test('renders the slim cache-miss rule with compact token count', () => {
        const markup = render(<CacheMissDivider reprocessedTokens={50_900} />);
        expect(markup).toContain('data-omp-cache-miss="50900"');
        expect(markup).toContain('⊘ cache miss · 51K tokens');
    });
});
