import React from 'react';
import { describe, expect, test } from 'bun:test';
import { renderToStaticMarkup } from 'react-dom/server';
import type { ToolPart as ToolPartType } from '@/lib/opencode/wire';

import { I18nProvider } from '@/lib/i18n';
import { AskAnswerCard } from './AskAnswerCard';
import { parseAskToolDetails } from './askToolDetails';

/**
 * The stub mirrors the wire tool part the omp projection emits for a settled
 * ask tool call (packages/web/server/lib/omp-host/projection.js): the SDK
 * AskToolDetails ride in state.metadata.details.
 */
const stubAskToolPart = (details: unknown): ToolPartType => ({
    id: 'prt_1_2',
    sessionID: 's1',
    messageID: 'msg_1',
    type: 'tool',
    callID: 'call_1',
    tool: 'ask',
    state: {
        status: 'completed',
        input: { questions: [] },
        output: 'User answers:\nYes',
        title: 'ask',
        metadata: { details },
        time: { start: 1, end: 2 },
    },
});

const renderCard = (part: ToolPartType): string => {
    const model = parseAskToolDetails((part.state as { metadata?: { details?: unknown } }).metadata?.details);
    expect(model).not.toBeNull();
    return renderToStaticMarkup(
        <I18nProvider>
            <AskAnswerCard model={model!} />
        </I18nProvider>,
    );
};

describe('AskAnswerCard', () => {
    test('renders per-question answers with selected options and the timed-out annotation', () => {
        const html = renderCard(stubAskToolPart({
            results: [
                {
                    id: 'q1',
                    question: 'Ship the release?',
                    options: ['Yes', 'No', 'Later'],
                    multi: false,
                    selectedOptions: ['Yes'],
                },
                {
                    id: 'q2',
                    question: 'Notify users?',
                    options: ['Now', 'Tomorrow'],
                    multi: true,
                    selectedOptions: ['Tomorrow'],
                    timedOut: true,
                },
            ],
        }));

        expect(html).toContain('Ship the release?');
        expect(html).toContain('Notify users?');
        expect(html).toContain('2 questions');
        // selected chips carry the selection styling, unselected stay muted
        expect(html.match(/border-primary/g)?.length).toBe(2);
        expect(html).toContain('text-muted-foreground/70">No');
        // timed-out answer carries the auto-selected annotation
        expect(html).toContain('auto-selected after timeout — not a user choice');
    });

    test('renders custom input and note lines for a single-question answer', () => {
        const html = renderCard(stubAskToolPart({
            question: 'Which approach?',
            options: ['Fast', 'Safe'],
            multi: false,
            selectedOptions: [],
            customInput: 'mix of both',
            note: 'see spec',
        }));

        expect(html).toContain('Which approach?');
        expect(html).toContain('Custom answer: ');
        expect(html).toContain('mix of both');
        expect(html).toContain('Note: ');
        expect(html).toContain('see spec');
        expect(html).not.toContain('auto-selected');
    });

    test('marks a multi-select "select none" answer when nothing was chosen', () => {
        const html = renderCard(stubAskToolPart({
            question: 'Disable anything?',
            options: ['Lint', 'Tests'],
            multi: true,
            selectedOptions: [],
        }));

        expect(html).toContain('No answer');
        expect(html).not.toContain('border-primary');
    });

    test('renders the chat-redirect outcome with the unanswered questions', () => {
        const html = renderCard(stubAskToolPart({
            chatRedirect: true,
            questions: ['Keep going?', 'Or stop here?'],
        }));

        expect(html).toContain('User chose to chat about this instead of answering');
        expect(html).toContain('Keep going?');
        expect(html).toContain('Or stop here?');
    });

    test('non-ask tool parts do not parse into an answer card', () => {
        // A read tool's details shape must leave the ask renderer inactive.
        expect(parseAskToolDetails({ filePath: '/x', lines: 42 })).toBeNull();
    });
});
