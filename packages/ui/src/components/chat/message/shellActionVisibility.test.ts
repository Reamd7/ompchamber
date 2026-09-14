import { describe, expect, test } from 'bun:test';
import type { Message, Part } from '@/lib/opencode/wire';

import { normalizeUserDisplayParts } from './normalizeUserDisplayParts';
import { isHiddenUserMessage } from './hiddenUserMessage';
import { isShellActionPart } from './partUtils';

// The omp `!`/`$` execution row: a user-role message whose only part is a
// synthetic text part carrying the shellAction card payload (omp-host
// projectExecutionMessage).
const shellPart = (): Part => {
    // SAFETY: test fixture — the wire part shape omp-host emits for `!` rows;
    // `shellAction` is an extension field the base Part union does not declare.
    return {
        id: 'prt_1_0',
        sessionID: 'ses_1',
        messageID: 'msg_1',
        type: 'text',
        text: '[omp:bash] $ ps [exit 0]\n  PID TTY TIME CMD\n',
        synthetic: true,
        shellAction: { command: 'ps', output: '  PID TTY TIME CMD\n', status: 'completed' },
    } as Part;
};

const userEntry = (parts: Part[]) => ({
    // SAFETY: test fixture — only the fields isHiddenUserMessage reads.
    info: { id: 'msg_1', role: 'user', time: { created: 1 } } as Message,
    parts,
});

describe('normalizeUserDisplayParts shellAction carrier', () => {
    test('keeps a synthetic text part carrying a shellAction payload', () => {
        const parts = normalizeUserDisplayParts([shellPart()]);
        expect(parts).toHaveLength(1);
        expect(isShellActionPart(parts[0])).toBe(true);
    });

    test('still drops a synthetic text part without shellAction', () => {
        // SAFETY: test fixture — minimal text part; the filter only reads
        // type/text/synthetic.
        const plain = { type: 'text', text: '[omp:bash] transport marker', synthetic: true } as Part;
        expect(normalizeUserDisplayParts([plain])).toHaveLength(0);
    });
});

describe('isHiddenUserMessage shell rows', () => {
    test('a `!` execution row is visible (the card is its whole content)', () => {
        expect(isHiddenUserMessage(userEntry([shellPart()]), { planModeEnabled: false })).toBe(false);
    });

    test('other fully synthetic user rows stay hidden', () => {
        // SAFETY: test fixture — minimal text part; the hidden check only
        // reads type/text/synthetic.
        const marker = { type: 'text', text: 'internal transport note', synthetic: true } as Part;
        expect(isHiddenUserMessage(userEntry([marker]), { planModeEnabled: false })).toBe(true);
    });
});
