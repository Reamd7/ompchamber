import { beforeEach, describe, expect, mock, test } from 'bun:test';

type ConfigResponse = { data: Record<string, unknown> };

(mock as unknown as { restore?: () => void }).restore?.();

const configResolvers: Array<(response: ConfigResponse) => void> = [];
let configCalls = 0;
let runtimeKey = 'test-runtime';
const promptAsyncCalls: unknown[][] = [];
const promptAsyncResults: Array<unknown> = [];
const pathGetResults: Array<unknown> = [];
const commandCalls: unknown[][] = [];
let modelRolesEnabled = false;

const promptAsyncMock = mock(async (...args: unknown[]) => {
  promptAsyncCalls.push(args);
  const next = promptAsyncResults.shift();
  if (next instanceof Error) throw next;
  return next ?? { response: new Response(null, { status: 200 }) };
});
const summarizeResults: Array<unknown> = [];
const summarizeMock = mock(async () => {
  const next = summarizeResults.shift();
  if (next instanceof Error) throw next;
  return next ?? { response: new Response(null, { status: 200 }) };
});
const commandMock = mock(async (...args: unknown[]) => {
  commandCalls.push(args);
  return { response: new Response(null, { status: 200 }) };
});

const pathGetMock = mock(async () => {
  const next = pathGetResults.shift();
  if (next instanceof Error) throw next;
  return next ?? { data: { directory: '/workspace/project' } };
});

mock.module('@/lib/omp/capabilityGate', () => ({
  isOmpModelRolesEnabled: () => modelRolesEnabled,
}));
mock.module('@/lib/opencode/wire', () => ({
  createOpencodeClient: mock(() => ({
    config: {
      get: mock(() => {
        configCalls += 1;
        return new Promise<ConfigResponse>((resolve) => {
          configResolvers.push(resolve);
        });
      }),
    },
    session: {
      promptAsync: promptAsyncMock,
      command: commandMock,
      summarize: summarizeMock,
    },
    path: {
      get: pathGetMock,
    },
  })),
}));

mock.module('@/contexts/runtimeAPIRegistry', () => ({
  getRegisteredRuntimeAPIs: mock(() => null),
}));

mock.module('@/lib/runtime-url', () => ({
  getRuntimeUrlResolver: mock(() => ({
    api: (path: string) => path,
  })),
}));

mock.module('@/lib/runtime-switch', () => ({
  getRuntimeApiBaseUrl: mock(() => ''),
  getRuntimeKey: mock(() => runtimeKey),
}));

mock.module('@/lib/runtime-fetch', () => ({
  runtimeFetch: mock(async () => new Response(JSON.stringify([]), {
    headers: { 'Content-Type': 'application/json' },
  })),
}));

mock.module('@/lib/startupTrace', () => ({
  markStartupTrace: mock(() => undefined),
}));

const { opencodeClient } = await import(`./client?cache-test=${Date.now()}`);

beforeEach(() => {
  runtimeKey = 'test-runtime';
  promptAsyncCalls.length = 0;
  promptAsyncResults.length = 0;
  summarizeResults.length = 0;
  pathGetResults.length = 0;
  commandCalls.length = 0;
  modelRolesEnabled = false;
});

describe('opencodeClient directory availability', () => {
  test('distinguishes a missing directory from an unavailable path probe', async () => {
    pathGetResults.push({ error: { code: 'ENOENT', message: 'no such file or directory' } });
    expect(await opencodeClient.getDirectoryAvailability('/private/deleted-worktree')).toBe('missing');

    pathGetResults.push(new Error('offline'));
    expect(await opencodeClient.getDirectoryAvailability('/private/deleted-worktree')).toBe('unknown');
  });
});

describe('opencodeClient getConfig cache', () => {
  test('cleared stale in-flight requests do not repopulate cache or delete newer in-flight requests', async () => {
    const first = opencodeClient.getConfig('/workspace/project');
    expect(configCalls).toBe(1);

    opencodeClient.clearConfigCache();

    const second = opencodeClient.getConfig('/workspace/project');
    expect(configCalls).toBe(2);

    configResolvers[0]?.({ data: { model: 'old/model' } });
    expect(await first).toEqual({ model: 'old/model' });

    const third = opencodeClient.getConfig('/workspace/project');
    expect(configCalls).toBe(2);

    configResolvers[1]?.({ data: { model: 'new/model' } });
    expect(await second).toEqual({ model: 'new/model' });
    expect(await third).toEqual({ model: 'new/model' });

    const cached = await opencodeClient.getConfig('/workspace/project');
    expect(cached).toEqual({ model: 'new/model' });
    expect(configCalls).toBe(2);
  });
});

describe('opencodeClient omp model-role request matrix', () => {
  test('legacy row retains model + variant; modelRoles.v1 row omits both', async () => {
    await opencodeClient.sendMessage({
      id: 'ses_legacy', providerID: 'legacy-provider', modelID: 'legacy-model', variant: 'high', text: 'legacy',
    });
    const legacy = promptAsyncCalls.at(-1)?.[0] as Record<string, unknown>;
    expect(legacy.model).toEqual({ providerID: 'legacy-provider', modelID: 'legacy-model' });
    expect(legacy.variant).toBe('high');

    modelRolesEnabled = true;
    await opencodeClient.sendMessage({
      id: 'ses_roles', providerID: 'display-provider', modelID: 'display-model', variant: 'xhigh', text: 'roles',
    });
    const roles = promptAsyncCalls.at(-1)?.[0] as Record<string, unknown>;
    expect(roles.model).toBe(undefined);
    expect(roles.variant).toBe(undefined);
    expect(roles.providerID).toBe(undefined);
    expect(roles.modelID).toBe(undefined);
    expect(roles.defaultModel).toBe(undefined);
  });

  test('captures capability before async attachment preparation; next request sees the new row', async () => {
    modelRolesEnabled = true;
    const inFlight = opencodeClient.sendMessage({
      id: 'ses_inflight', providerID: 'display-provider', modelID: 'display-model', variant: 'high', text: 'one',
      files: [{ type: 'file', mime: 'text/markdown', filename: 'a.md', url: 'data:text/markdown,hello' }],
    });
    modelRolesEnabled = false;
    await inFlight;
    const captured = promptAsyncCalls.at(-1)?.[0] as Record<string, unknown>;
    expect(captured.model).toBe(undefined);
    expect(captured.variant).toBe(undefined);

    await opencodeClient.sendMessage({
      id: 'ses_next', providerID: 'legacy-next', modelID: 'model-next', variant: 'low', text: 'two',
    });
    const next = promptAsyncCalls.at(-1)?.[0] as Record<string, unknown>;
    expect(next.model).toEqual({ providerID: 'legacy-next', modelID: 'model-next' });
    expect(next.variant).toBe('low');
  });

  test('applies the same request matrix to slash commands', async () => {
    await opencodeClient.sendCommand({
      id: 'ses_command_legacy', providerID: 'legacy-provider', modelID: 'legacy-model', variant: 'high', command: 'review',
    });
    const legacy = commandCalls.at(-1)?.[0] as Record<string, unknown>;
    expect(legacy.model).toBe('legacy-provider/legacy-model');
    expect(legacy.variant).toBe('high');

    modelRolesEnabled = true;
    await opencodeClient.sendCommand({
      id: 'ses_command_roles', providerID: 'display-provider', modelID: 'display-model', variant: 'xhigh', command: 'review',
    });
    const roles = commandCalls.at(-1)?.[0] as Record<string, unknown>;
    expect(roles.model).toBe(undefined);
    expect(roles.variant).toBe(undefined);
    expect(roles.providerID).toBe(undefined);
    expect(roles.modelID).toBe(undefined);
    expect(roles.defaultModel).toBe(undefined);
  });
});

describe('opencodeClient prompt retry behavior', () => {
  const sendPrompt = (providerID = 'anthropic') => opencodeClient.sendMessage({
    id: 'ses_1',
    providerID,
    modelID: 'claude-sonnet',
    text: 'hello',
  });

  test('does not retry 504 prompt responses because the POST may already be accepted', async () => {
    promptAsyncResults.push({ response: new Response('gateway timeout', { status: 504 }) });

    let error: unknown = null;
    try {
      await sendPrompt('anthropic-504');
    } catch (caught) {
      error = caught;
    }

    expect(promptAsyncCalls.length).toBe(1);
    expect(error instanceof Error ? error.message : String(error)).toContain('Failed to send message (504)');
  });

  test('does not retry transport failures because the tunnel may have lost only the response', async () => {
    promptAsyncResults.push(new TypeError('Failed to fetch'));

    let error: unknown = null;
    try {
      await sendPrompt('anthropic-network');
    } catch (caught) {
      error = caught;
    }

    expect(promptAsyncCalls.length).toBe(1);
    expect(error instanceof Error ? error.message : String(error)).toContain('Failed to fetch');
  });

  test('does not fabricate an HTTP 500 when the SDK swallows a transport failure into result.error', async () => {
    // The SDK catches thrown fetch errors and returns { error, response: undefined }.
    // That is a transport failure, not a server 500 — it must surface as a
    // descriptive transport error, never as "Failed to send message (500): {}".
    promptAsyncResults.push({ error: new TypeError('relay tunnel reset: plaintext frame on established channel'), response: undefined });

    let error: unknown = null;
    try {
      await sendPrompt('anthropic-transport');
    } catch (caught) {
      error = caught;
    }

    expect(promptAsyncCalls.length).toBe(1);
    const message = error instanceof Error ? error.message : String(error);
    expect(message).not.toContain('Failed to send message (500)');
    expect(message).toContain('transport failure');
    expect(message).toContain('relay tunnel reset');
    expect((error as Error & { status?: number }).status).toBe(undefined);
  });

  test('does not retry 503 prompt responses because proxy errors can be ambiguous too', async () => {
    promptAsyncResults.push({ response: new Response('starting', { status: 503 }) });

    let error: unknown = null;
    try {
      await sendPrompt('anthropic-503');
    } catch (caught) {
      error = caught;
    }

    expect(promptAsyncCalls.length).toBe(1);
    expect(error instanceof Error ? error.message : String(error)).toContain('Failed to send message (503)');
  });

  test('does not dispatch after the runtime changes while preparing attachments', async () => {
    runtimeKey = 'runtime-a';
    const pending = opencodeClient.sendMessage({
      id: 'ses_runtime_race',
      providerID: 'runtime-race-provider',
      modelID: 'model-a',
      text: 'hello',
      runtimeKey: 'runtime-a',
      files: [{
        type: 'file',
        mime: 'text/markdown',
        filename: 'notes.md',
        url: 'data:text/markdown,hello',
      }],
    });

    runtimeKey = 'runtime-b';

    let error: unknown = null;
    try {
      await pending;
    } catch (caught) {
      error = caught;
    }

    expect(error).toBeInstanceOf(Error);
    expect(error instanceof Error ? error.message : String(error)).toContain('runtime changed');
    expect(promptAsyncCalls).toHaveLength(0);
  });
});

describe('opencodeClient summarizeSession error surfacing', () => {
  test('extracts the nested engine message from an OpenCode error body', async () => {
    summarizeResults.push({
      error: { name: 'UnknownError', data: { message: 'Nothing to compact (session too small)' } },
      response: { status: 500 },
    });

    let error: unknown = null;
    try {
      await opencodeClient.summarizeSession('ses_compact', 'provider', 'model', '/workspace/project');
    } catch (caught) {
      error = caught;
    }

    const message = error instanceof Error ? error.message : String(error);
    expect(message).toBe('session.summarize failed (500): Nothing to compact (session too small)');
    expect(message).not.toContain('{');
  });
});
