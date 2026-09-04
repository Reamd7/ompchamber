import { describe, expect, test } from 'bun:test';
import type { Message, Part } from '@/lib/opencode/wire'

import {
    buildTaskSummaryEntriesFromSession,
    formatAgentDuration,
    parseTaskMetadataBlock,
    prepareTaskToolOutput,
    readTaskAgentRows,
    readTaskSessionIdFromRecord,
    readTaskSessionIdFromOutput,
} from './taskToolModel';
import { TOOL_OUTPUT_MAX_CHARS } from '../toolRenderers';

describe('taskToolModel', () => {
    test('reads the current OpenCode running-state identity contract', () => {
        expect(readTaskSessionIdFromRecord({ sessionId: 'child-live' })).toBe('child-live');
        expect(readTaskSessionIdFromRecord({})).toBe(undefined);
    });

    test('reads authoritative session and summary metadata', () => {
        const output = 'result\n<task_metadata>{"sessionID":"child-1","calls":[{"id":"tool-1","tool":"read","title":"a.ts"}]}</task_metadata>';
        expect(parseTaskMetadataBlock(output)).toEqual({
            sessionId: 'child-1',
            summaryEntries: [{ id: 'tool-1', tool: 'read', state: { status: undefined, title: 'a.ts', input: undefined } }],
        });
        expect(readTaskSessionIdFromOutput(output)).toBe('child-1');
    });

    test('projects tool calls while excluding nested task and todo bookkeeping', () => {
        const message = {
            info: { id: 'message-1', role: 'assistant' } as Message,
            parts: [
                { id: 'read-1', type: 'tool', tool: 'read', state: { status: 'completed', input: { filePath: 'a.ts' } } },
                { id: 'task-1', type: 'tool', tool: 'task', state: { status: 'running' } },
                { id: 'todo-1', type: 'tool', tool: 'todowrite', state: { status: 'completed' } },
            ] as unknown as Part[],
        };

        expect(buildTaskSummaryEntriesFromSession([message])).toEqual([{
            id: 'read-1',
            tool: 'read',
            state: { status: 'completed', title: undefined, input: { filePath: 'a.ts' } },
        }]);
    });

    test('strips task metadata and caps oversized task output before markdown rendering', () => {
        const oversized = 'x'.repeat(TOOL_OUTPUT_MAX_CHARS + 5_000);
        const output = `${oversized}\n<task_metadata>{"sessionID":"child-1"}</task_metadata>`;
        const prepared = prepareTaskToolOutput(output);

        expect(prepared.length).toBeLessThan(oversized.length);
        expect(prepared).toContain('output truncated');
        expect(prepared).not.toContain('task_metadata');
    });

    test('leaves normal task output untouched', () => {
        expect(prepareTaskToolOutput('done\n<task_metadata>{"sessionID":"child-1"}</task_metadata>')).toBe('done');
        expect(prepareTaskToolOutput(undefined)).toBe('');
    });

    test('parses live AgentProgress snapshots into sorted agent rows', () => {
        const metadata = {
            details: {
                progress: [
                    { index: 1, id: 'b', agent: 'task', status: 'running', task: 'resolve conflicts', currentTool: 'bash', tokens: 1500, durationMs: 3400, retryState: { attempt: 2, maxAttempts: 5 } },
                    { index: 0, id: 'a', agent: 'scout', status: 'completed', description: 'scan docs', tokens: 120, durationMs: 800 },
                ],
                results: [],
            },
        };
        expect(readTaskAgentRows(metadata)).toEqual([
            { key: 'a', agent: 'scout', status: 'completed', label: 'scan docs', detail: undefined, tokens: 120, durationMs: 800, retryAttempt: undefined, retryMax: undefined, retryExhausted: undefined },
            { key: 'b', agent: 'task', status: 'running', label: 'resolve conflicts', detail: 'bash', tokens: 1500, durationMs: 3400, retryAttempt: 2, retryMax: 5, retryExhausted: undefined },
        ]);
    });

    test('settled results replace stale progress and derive terminal status', () => {
        const metadata = {
            details: {
                progress: [{ index: 0, id: 'a', agent: 'scout', status: 'running' }],
                results: [
                    { index: 1, id: 'b', agent: 'task', exitCode: 1, error: 'boom', tokens: 10, durationMs: 5 },
                    { index: 0, id: 'a', agent: 'scout', exitCode: 0, aborted: true, tokens: 3, durationMs: 1 },
                ],
            },
        };
        expect(readTaskAgentRows(metadata)).toEqual([
            { key: 'a', agent: 'scout', status: 'aborted', label: undefined, detail: undefined, tokens: 3, durationMs: 1, retryAttempt: undefined, retryMax: undefined, retryExhausted: undefined, outputPath: undefined },
            { key: 'b', agent: 'task', status: 'failed', label: undefined, detail: 'boom', tokens: 10, durationMs: 5, retryAttempt: undefined, retryMax: undefined, retryExhausted: undefined, outputPath: undefined },
        ]);
    });

    test('flags retry exhaustion and ignores malformed rows', () => {
        const metadata = {
            details: {
                progress: [
                    { index: 0, id: 'a', agent: 'scout', status: 'running', retryFailure: { attempt: 3 } },
                    { index: 1, agent: 42 },
                    'junk',
                    null,
                ],
            },
        };
        expect(readTaskAgentRows(metadata)).toEqual([
            { key: 'a', agent: 'scout', status: 'running', label: undefined, detail: undefined, tokens: undefined, durationMs: undefined, retryAttempt: 3, retryExhausted: true, outputPath: undefined },
        ]);
        expect(readTaskAgentRows(undefined)).toEqual([]);
        expect(readTaskAgentRows({ details: { progress: 'nope' } })).toEqual([]);
    });

    test('settled rows carry the durable output artifact path', () => {
        const rows = readTaskAgentRows({
            details: {
                results: [
                    { index: 0, id: 'a', agent: 'task', exitCode: 0, tokens: 5, durationMs: 10, outputPath: 'agent://task-a0.md' },
                    { index: 1, id: 'b', agent: 'scout', exitCode: 0 },
                ],
            },
        });
        expect(rows[0]?.outputPath).toBe('agent://task-a0.md');
        expect(rows[1]?.outputPath).toBeUndefined();
    });

    test('formats durations the way the TUI renderer prints them', () => {
        expect(formatAgentDuration(400)).toBe('400ms');
        expect(formatAgentDuration(5400)).toBe('5.4s');
        expect(formatAgentDuration(125_000)).toBe('2m05s');
    });
});
