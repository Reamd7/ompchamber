import { describe, expect, mock, test } from 'bun:test';

import type { MarkdownWorkerRequest } from './markdown-worker-protocol';

/**
 * Hang safety for the markdown Shiki worker client
 * (openchamber/openchamber#2587, follow-up on #2618).
 *
 * Catastrophic Oniguruma backtracking is synchronous inside the worker, so the
 * only recovery is terminating it from this thread. Two properties matter and
 * neither is observable from the timeout constant alone: the hung request must
 * resolve `null` after the worker is terminated, and the block that caused it
 * must not be retried — a retry respawns a worker (Shiki + Oniguruma init) and
 * burns another full budget of a core on every render and every scroll past it.
 */

const TEST_TIMEOUT_MS = 50;

mock.module('./markdown-worker-timeout', () => ({ HIGHLIGHT_REQUEST_TIMEOUT_MS: TEST_TIMEOUT_MS }));

/**
 * The module imports the worker as an inline constructor, so the stub replaces
 * that constructor directly. `mode` picks the behavior per test: silent
 * accepts everything and answers nothing; replay stays silent on the first
 * instance and answers on every later one, so a request that was only queued
 * behind the hung one can be observed being replayed against the replacement
 * worker.
 */
class StubWorker {
  static mode: 'silent' | 'replay' = 'silent';
  static created = 0;
  static terminated = 0;
  static messages: MarkdownWorkerRequest[] = [];

  onmessage: ((event: MessageEvent) => void) | null = null;
  onerror: (() => void) | null = null;
  onmessageerror: (() => void) | null = null;

  constructor() {
    StubWorker.created += 1;
  }

  postMessage(message: MarkdownWorkerRequest): void {
    StubWorker.messages.push(message);
    const answers = StubWorker.mode === 'replay' && StubWorker.created > 1;
    if (!answers || message.type !== 'highlight') return;
    // Real macrotask defer, not a fake-timer clock: the module under test arms
    // its own timeout budget on the platform timer, and this test races the
    // answer against that budget on purpose.
    setTimeout(() => {
      this.onmessage?.(new MessageEvent('message', {
        data: { type: 'highlight', id: message.id, html: '<pre>ok</pre>' },
      }));
    }, 0);
  }

  terminate(): void {
    StubWorker.terminated += 1;
  }
}

mock.module('./markdown-shiki.worker.ts?worker&inline', () => ({ default: StubWorker }));

/**
 * bun test has no `window` or `Worker`; defining the properties directly
 * installs the stubs without asserting they are the platform globals. The
 * constructor itself is stubbed above; the globals only satisfy the module's
 * worker-support guard.
 */
const resetWorkerStub = (mode: 'silent' | 'replay'): void => {
  StubWorker.mode = mode;
  StubWorker.created = 0;
  StubWorker.terminated = 0;
  StubWorker.messages.length = 0;
  Object.defineProperty(globalThis, 'window', { value: {}, configurable: true, writable: true });
  Object.defineProperty(globalThis, 'Worker', { value: StubWorker, configurable: true, writable: true });
};

describe('markdown-worker hang safety', () => {
  test('a hung block resolves null, terminates the worker, and is not retried', async () => {
    resetWorkerStub('silent');
    const { highlightCodeInWorker, resetMarkdownWorkerClientCacheForTests } = await import('./markdown-worker');
    resetMarkdownWorkerClientCacheForTests();

    const code = 'const label = `Account ${index + 1}`;';

    const first = await highlightCodeInWorker(code, 'javascript');
    expect(first).toBeNull();
    expect(StubWorker.terminated).toBe(1);

    const createdAfterFirst = StubWorker.created;
    const messagesAfterFirst = StubWorker.messages.length;

    // Same content again: the timed-out key is memoized as failed, so nothing
    // reaches a worker and none is spawned.
    const second = await highlightCodeInWorker(code, 'javascript');
    expect(second).toBeNull();
    expect(StubWorker.created).toBe(createdAfterFirst);
    expect(StubWorker.messages.length).toBe(messagesAfterFirst);
  });

  test('a timeout fails only the offending request and replays the queued one', async () => {
    resetWorkerStub('replay');
    const { highlightCodeInWorker, resetMarkdownWorkerClientCacheForTests } = await import('./markdown-worker');
    resetMarkdownWorkerClientCacheForTests();

    const results = await Promise.all([
      highlightCodeInWorker('const a = `one`;', 'javascript'),
      highlightCodeInWorker('const b = `two`;', 'javascript'),
    ]);

    // Whichever request owns the first timer is the offender; the other was
    // merely queued behind it and must survive on the replacement worker
    // rather than being cancelled with it.
    expect(results.filter((value) => value === null)).toHaveLength(1);
    expect(results.filter((value) => value === '<pre>ok</pre>')).toHaveLength(1);
    expect(StubWorker.terminated).toBe(1);
    expect(StubWorker.created).toBe(2);
  });
});
