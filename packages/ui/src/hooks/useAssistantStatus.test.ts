import { describe, expect, test } from 'bun:test';
import type { Message, Part } from '@/lib/opencode/wire'

import { createParsedStatus, getActiveAssistantContext } from './useAssistantStatus';

const userMessage = (id: string, providerID: string, modelID: string): Message => ({
    id,
    role: 'user',
    sessionID: 'ses_1',
    time: { created: 1 },
    model: { providerID, modelID },
} as Message);

const assistantMessage = (id: string, parentID: string): Message => ({
    id,
    role: 'assistant',
    sessionID: 'ses_1',
    parentID,
    time: { created: 2 },
} as Message);

describe('getActiveAssistantContext', () => {
    test('uses the active assistant parent model instead of the latest user selection', () => {
        const activeParent = userMessage('user_1', 'anthropic', 'claude-opus-4-1');
        const assistant = assistantMessage('assistant_1', activeParent.id);
        const laterSelection = userMessage('user_2', 'openai', 'gpt-5.6-sol');

        expect(getActiveAssistantContext([activeParent, assistant, laterSelection])).toEqual({
            assistantId: assistant.id,
            model: {
                providerId: 'anthropic',
                modelId: 'claude-opus-4-1',
            },
        });
    });

    test('switches models only when a newer assistant links to the newer user message', () => {
        const firstUser = userMessage('user_1', 'anthropic', 'claude-opus-4-1');
        const firstAssistant = assistantMessage('assistant_1', firstUser.id);
        const secondUser = userMessage('user_2', 'openai', 'gpt-5.6-sol');
        const secondAssistant = assistantMessage('assistant_2', secondUser.id);

        expect(getActiveAssistantContext([firstUser, firstAssistant, secondUser, secondAssistant])).toEqual({
            assistantId: secondAssistant.id,
            model: {
                providerId: 'openai',
                modelId: 'gpt-5.6-sol',
            },
        });
    });

    test('does not guess a model when the parent message is unavailable', () => {
        const assistant = assistantMessage('assistant_1', 'missing_user');

        expect(getActiveAssistantContext([assistant])).toEqual({
            assistantId: assistant.id,
            model: null,
        });
    });
});
describe('createParsedStatus tool intent (ch 12: working-message slot)', () => {
    const runningTool = (intent?: string): Part => ({
        id: 'prt_1',
        sessionID: 'ses_1',
        messageID: 'msg_1',
        type: 'tool',
        callID: 'call_1',
        tool: 'grep',
        state: intent === undefined
            ? { status: 'running', input: {}, time: { start: 1 } }
            : { status: 'running', input: {}, time: { start: 1 }, metadata: { intent } },
    });

    test('running tool with intent shows the intent, not the tool phrase', () => {
        const result = createParsedStatus([runningTool('locating the auth guard')], 'generic-key');
        expect(result.statusText).toBe('locating the auth guard');
        expect(result.activeToolIntent).toBe('locating the auth guard');
        expect(result.activeToolName).toBe('grep');
    });

    test('running tool without intent falls back to the tool phrase', () => {
        const result = createParsedStatus([runningTool()], 'generic-key');
        expect(result.statusText).toBe('searching content');
        expect(result.activeToolIntent).toBeUndefined();
    });

    test('long intent truncates to 60 chars with an ellipsis', () => {
        const long = 'a'.repeat(80);
        const result = createParsedStatus([runningTool(long)], 'generic-key');
        expect(result.statusText).toBe(`${'a'.repeat(60)}…`);
    });

    test('blank intent is ignored (whitespace-only does not override the phrase)', () => {
        const result = createParsedStatus([runningTool('   ')], 'generic-key');
        expect(result.activeToolIntent).toBeUndefined();
        expect(result.statusText).toBe('searching content');
    });
});
