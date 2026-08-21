import { describe, expect, test } from 'bun:test';

import { parseAskToolDetails } from './askToolDetails';

describe('parseAskToolDetails', () => {
    test('parses a flat single-question answer (SDK AskToolDetails)', () => {
        const model = parseAskToolDetails({
            question: 'Ship it?',
            options: ['Yes', 'No'],
            multi: false,
            selectedOptions: ['Yes'],
            customInput: 'only after tests',
            note: 'n',
            timedOut: true,
        });
        expect(model).toEqual({
            kind: 'answers',
            entries: [{
                id: '0',
                question: 'Ship it?',
                options: ['Yes', 'No'],
                multi: false,
                selectedOptions: ['Yes'],
                customInput: 'only after tests',
                note: 'n',
                timedOut: true,
            }],
        });
    });

    test('parses a multi-question results array keeping entry ids', () => {
        const model = parseAskToolDetails({
            results: [
                { id: 'q1', question: 'First?', options: ['A', 'B'], multi: false, selectedOptions: ['A'] },
                { id: 'q2', question: 'Second?', options: ['C'], multi: true, selectedOptions: [], timedOut: true },
            ],
        });
        expect(model?.kind).toBe('answers');
        if (model?.kind !== 'answers') return;
        expect(model.entries.map((entry) => entry.id)).toEqual(['q1', 'q2']);
        expect(model.entries[1].timedOut).toBe(true);
        expect(model.entries[1].multi).toBe(true);
    });

    test('parses the chat-redirect outcome', () => {
        expect(parseAskToolDetails({ chatRedirect: true, questions: ['Keep going?'] })).toEqual({
            kind: 'chatRedirect',
            questions: ['Keep going?'],
        });
    });

    test('falls back to selected labels when the options array is omitted', () => {
        const model = parseAskToolDetails({ question: 'Pick', selectedOptions: ['B'] });
        expect(model).toEqual({
            kind: 'answers',
            entries: [{
                id: '0',
                question: 'Pick',
                options: [],
                multi: false,
                selectedOptions: ['B'],
                timedOut: false,
            }],
        });
    });

    test('returns null for non-ask shapes so ToolPart keeps generic rendering', () => {
        expect(parseAskToolDetails(undefined)).toBeNull();
        expect(parseAskToolDetails('text output')).toBeNull();
        expect(parseAskToolDetails({})).toBeNull();
        // other tools' structured details (e.g. read/task) are not ask answers
        expect(parseAskToolDetails({ filePath: '/x', lines: 12 })).toBeNull();
        expect(parseAskToolDetails({ results: [] })).toBeNull();
        expect(parseAskToolDetails({ results: [{ id: 'q1' }] })).toBeNull();
    });
});
