import { describe, expect, test } from 'bun:test';

import { parseOmpCustomText, tierFor } from './customTypeTiers';

describe('tierFor (05 §5.8.2 tier table)', () => {
    test('T1 — visible card types', () => {
        for (const customType of [
            'advisor',
            'irc:incoming',
            'irc:autoreply',
            'irc:relay',
            'async-result',
            'skill-prompt',
            'lsp-late-diagnostic',
            'live-delegation',
            'collab-prompt',
            'background-tan-dispatch',
            'launch-completion',
        ]) {
            expect(tierFor(customType)).toBe('T1');
        }
    });

    test('T2 — folding divider types', () => {
        expect(tierFor('compactionSummary')).toBe('T2');
        expect(tierFor('branchSummary')).toBe('T2');
        expect(tierFor('handoff')).toBe('T2');
        expect(tierFor('modelChange')).toBe('T2');
        expect(tierFor('modeChange')).toBe('T2');
    });

    test('T3 — hidden display:false types, including prefix families', () => {
        for (const customType of [
            'eager-todo-prelude',
            'ultrathink-notice',
            'ttsr-injection',
            'goal-continuation',
            'gemini-tool-call-reminder',
        ]) {
            expect(tierFor(customType)).toBe('T3');
        }
        expect(tierFor('prewalk-subagent')).toBe('T3');
        expect(tierFor('plan-mode-injection')).toBe('T3');
    });

    test('T5 — collapsed developer notes', () => {
        expect(tierFor('developer')).toBe('T5');
    });

    test('T4 — unregistered and extension types fall back', () => {
        expect(tierFor('some-extension-sendMessage')).toBe('T4');
        expect(tierFor('brand-new-sdk-type')).toBe('T4');
        expect(tierFor('')).toBe('T4');
    });
});

describe('parseOmpCustomText (projection prefix contract)', () => {
    test('splits the label from the body', () => {
        expect(parseOmpCustomText('[omp:irc:incoming] hello from irc')).toEqual({
            customType: 'irc:incoming',
            body: 'hello from irc',
        });
        expect(parseOmpCustomText('[omp:compactionSummary] Summarized the chat.')).toEqual({
            customType: 'compactionSummary',
            body: 'Summarized the chat.',
        });
    });

    test('tolerates an empty body after the label space', () => {
        expect(parseOmpCustomText('[omp:advisor] ')).toEqual({ customType: 'advisor', body: '' });
    });

    test('rejects non-omp text, bare labels, and whitespace inside the type token', () => {
        expect(parseOmpCustomText('regular assistant text')).toBe(null);
        expect(parseOmpCustomText('[omp] unlabeled fallback')).toBe(null);
        expect(parseOmpCustomText('[omp:two words] body')).toBe(null);
        expect(parseOmpCustomText('')).toBe(null);
    });
});
