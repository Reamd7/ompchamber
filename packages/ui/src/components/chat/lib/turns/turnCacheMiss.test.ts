import { describe, expect, test } from 'bun:test';
import type { Message, Part } from '@/lib/opencode/wire';

import { projectTurnRecords } from './projectTurnRecords';
import type { ChatMessageEntry } from './types';

const entry = (id: string, role: 'user' | 'assistant', parentID: string | undefined, tokens?: { input: number; output: number; reasoning: number; cache: { read: number; write: number } }): ChatMessageEntry => ({
    info: {
        id,
        role,
        sessionID: 'ses_1',
        ...(parentID !== undefined ? { parentID } : {}),
        ...(tokens ? { tokens } : {}),
        time: { created: 1, ...(role === 'assistant' ? { completed: 2 } : {}) },
    } as Message,
    parts: [] as Part[],
});

const warm = { input: 100, output: 50, reasoning: 0, cache: { read: 4_096, write: 0 } };
const cold = { input: 1_900, output: 50, reasoning: 0, cache: { read: 0, write: 500 } };
const reusing = { input: 100, output: 50, reasoning: 0, cache: { read: 4_096, write: 10 } };

const turn = (userId: string, assistantId: string, tokens?: { input: number; output: number; reasoning: number; cache: { read: number; write: number } }): ChatMessageEntry[] => [
    entry(userId, 'user', undefined),
    entry(assistantId, 'assistant', userId, tokens),
];

describe('projectTurnRecords cache-miss annotation (05 §5.9)', () => {
    test('marks the turn whose request went warm→cold, not its predecessor', () => {
        const result = projectTurnRecords([
            ...turn('user_1', 'assistant_1', warm),
            ...turn('user_2', 'assistant_2', cold),
        ]);
        const first = result.indexes.turnById.get('user_1');
        const second = result.indexes.turnById.get('user_2');
        expect(first?.cacheMiss).toBe(undefined);
        // reprocessed = cacheWrite + input = 500 + 1900.
        expect(second?.cacheMiss).toEqual({ reprocessedTokens: 2_400 });
    });

    test('no marker while the cache survives (any reuse) or the turn is the first', () => {
        const result = projectTurnRecords([
            ...turn('user_1', 'assistant_1', cold), // first turn: no predecessor
            ...turn('user_2', 'assistant_2', reusing), // prefix survived
        ]);
        expect(result.turns.every((record) => record.cacheMiss === undefined)).toBe(true);
    });

    test('turns without billed tokens never carry a marker and do not advance the chain', () => {
        const result = projectTurnRecords([
            ...turn('user_1', 'assistant_1', warm),
            ...turn('user_2', 'assistant_2', undefined), // streaming/empty assistant
            ...turn('user_3', 'assistant_3', cold),
        ]);
        const turns = result.turns;
        expect(turns[1]?.cacheMiss).toBe(undefined);
        // The marker lands on the first turn that actually made a cold request.
        expect(turns[2]?.cacheMiss).toEqual({ reprocessedTokens: 2_400 });
    });

    test('multi-message turns compare the turn opening request against the previous turn tail', () => {
        const result = projectTurnRecords([
            entry('user_1', 'user', undefined),
            entry('assistant_1a', 'assistant', 'user_1', { input: 1, output: 1, reasoning: 0, cache: { read: 0, write: 9_000 } }),
            entry('assistant_1b', 'assistant', 'user_1', warm),
            entry('user_2', 'user', undefined),
            entry('assistant_2', 'assistant', 'user_2', cold),
        ]);
        const first = result.indexes.turnById.get('user_1');
        const second = result.indexes.turnById.get('user_2');
        expect(first?.cacheMiss).toBe(undefined);
        expect(second?.cacheMiss).toEqual({ reprocessedTokens: 2_400 });
    });

    test('reprojection keeps record identity when the marker is unchanged (copy-on-write)', () => {
        const messages = [
            ...turn('user_1', 'assistant_1', warm),
            ...turn('user_2', 'assistant_2', cold),
        ];
        const first = projectTurnRecords(messages);
        const second = projectTurnRecords(messages, { previousProjection: first });
        expect(second.turns[1]).toBe(first.turns[1]);
        expect(second.turns[1]?.cacheMiss).toEqual({ reprocessedTokens: 2_400 });
    });

    test('a marker appearing on reprojection swaps the record for a fresh one', () => {
        // Project while turn 2 is still empty (no marker), then complete it.
        const user1 = entry('user_1', 'user', undefined);
        const assistant1 = entry('assistant_1', 'assistant', 'user_1', warm);
        const user2 = entry('user_2', 'user', undefined);
        const assistant2Streaming = entry('assistant_2', 'assistant', 'user_2');
        const first = projectTurnRecords([user1, assistant1, user2, assistant2Streaming]);
        expect(first.turns[1]?.cacheMiss).toBe(undefined);

        const assistant2Done = entry('assistant_2', 'assistant', 'user_2', cold);
        const second = projectTurnRecords([user1, assistant1, user2, assistant2Done], { previousProjection: first });
        expect(second.turns[1]).not.toBe(first.turns[1]);
        expect(second.turns[1]?.cacheMiss).toEqual({ reprocessedTokens: 2_400 });
        // The untouched predecessor record stays identity-stable.
        expect(second.turns[0]).toBe(first.turns[0]);
    });
});
