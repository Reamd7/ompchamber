import { describe, expect, test, afterAll } from 'vitest';
import { RingEventBus, WireEventBus, OmpEventBus } from './events.ts';
import type { OmpHostEngine } from './engine.ts';
// registerEndpoints test doubles: partial engine objects implementing exactly
// the mounting surface each test exercises.
import { projectDividerMessage, projectConversation } from './projection.ts';
import { buildCapabilities, ompFeatures } from './omp-parity.ts';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { execFileSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';

// SAFETY: OmpHostEngine is a nominal class carrying private state a literal
// cannot construct; each double below implements exactly the mounting surface
// its test exercises, and registerEndpoints reads nothing beyond it. This is
// the single test-double seam — no other cast bridges into the engine type.
const asEngineDouble = <T,>(double: T): OmpHostEngine => double as OmpHostEngine;

const now = 1_700_000_000_000;

describe('RingEventBus', () => {
  test('wire bus keeps every event durable and replayable', () => {
    const bus = new WireEventBus({ capacity: 8 });
    bus.emit('a', { x: 1 }, 'dir');
    bus.emit('b', { x: 2 }, 'dir');
    const seen = [];
    bus.subscribeSince(1, (entry) => seen.push(entry.envelope.type), { directory: 'dir' });
    expect(seen).toEqual(['b']);
  });

  test('volatile events reach live subscribers but never replay', () => {
    const bus = new RingEventBus({ capacity: 8 });
    const seen = [];
    bus.subscribeSince(0, (entry) => seen.push(entry.envelope.type));
    bus.emit('d1', {}, 'dir', { durable: true });
    bus.emit('v1', {}, 'dir', { durable: false });
    bus.emit('d2', {}, 'dir', { durable: true });
    // Live subscribers see volatile entries; the ring keeps only durable ones.
    expect(seen).toEqual(['d1', 'v1', 'd2']);
    const replayed = [];
    bus.subscribeSince(0, (entry) => replayed.push(entry.envelope.type));
    expect(replayed).toEqual(['d1', 'd2']); // fresh subscriber: ring replay only, volatile never resurrects
    expect(bus.replay.map((entry) => entry.envelope.type)).toEqual(['d1', 'd2']);
  });
});

describe('OmpEventBus', () => {
  test('envelope carries id/type/directory/sessionID/schemaVersion/createdAt and clean payload', () => {
    const bus = new OmpEventBus({ schemaVersion: '1.1' });
    const envelope = bus.publish('omp.model.changed', { model: { provider: 'p', id: 'm' } }, {
      directory: 'dir-a',
      sessionID: 's1',
      durable: true,
    });
    expect(envelope.type).toBe('omp.model.changed');
    expect(envelope.directory).toBe('dir-a');
    expect(envelope.sessionID).toBe('s1');
    expect(envelope.schemaVersion).toBe('1.1');
    expect(typeof envelope.createdAt).toBe('number');
    expect(envelope.payload).toEqual({ model: { provider: 'p', id: 'm' } });
    expect(Object.keys(envelope)).not.toContain('properties');
  });

  test('directory scoping filters subscribers', () => {
    const bus = new OmpEventBus();
    bus.publish('omp.notice.raised', { level: 'info' }, { directory: 'a', durable: true });
    const seen = [];
    bus.subscribeSince(0, (entry) => seen.push(entry.envelope.directory), { directory: 'b' });
    expect(seen).toEqual([]);
  });

  test('replayState detects restart and gap', () => {
    const bus = new OmpEventBus({ capacity: 3 });
    for (let i = 0; i < 5; i += 1) {
      bus.publish(`omp.notice.raised`, { i }, { directory: 'a', durable: true });
    }
    expect(bus.replayState(99).status).toBe('restart'); // client id ahead of tail
    expect(bus.replayState(0).status).toBe('ok');
    expect(bus.replayState(1).status).toBe('gap'); // evicted below ring head
    expect(bus.replayState(3).status).toBe('ok');
  });

  test('resync control frames are never stored in the ring', () => {
    const bus = new OmpEventBus();
    bus.publish('omp.stream.resync', { scope: ['model'] }, { directory: 'a', durable: false });
    expect(bus.replay.length).toBe(0);
  });
});

describe('divider projection (spec 05 §5.5)', () => {
  test('compactionSummary projects as a deterministic assistant divider with metadata', () => {
    const projected = projectDividerMessage(
      { role: 'compactionSummary', summary: 'summarized turn', tokensBefore: 9000, warning: 'freed little', timestamp: now },
      { sessionID: 's1', agent: 'build' },
    );
    expect(projected.info.role).toBe('assistant');
    expect(projected.info.metadata).toEqual({ ompRole: 'compactionSummary', tokensBefore: 9000, warning: 'freed little' });
    expect(projected.parts[0].text).toBe('[omp:compactionSummary] summarized turn');
    expect(projected.parts[0].synthetic).toBe(true);
    expect(projected.info.summary).toBe(true);
  });

  test('branchSummary carries fromId metadata', () => {
    const projected = projectDividerMessage(
      { role: 'branchSummary', summary: 'branched', fromId: 'e7', timestamp: now },
      { sessionID: 's1' },
    );
    expect(projected.info.metadata).toEqual({ ompRole: 'branchSummary', fromId: 'e7' });
    expect(projected.info.summary).toBe(true);
  });

  test('projectConversation routes divider roles and keeps pairing intact', () => {
    const out = projectConversation(
      [
        { role: 'user', content: 'hi', timestamp: now },
        { role: 'assistant', content: [{ type: 'text', text: 'answer' }], model: 'p/m', timestamp: now + 1 },
        { role: 'compactionSummary', summary: 'c1', tokensBefore: 1, timestamp: now + 2 },
        { role: 'custom', customType: 'advisor', content: [{ type: 'text', text: 'note' }], display: true, timestamp: now + 3 },
      ],
      { sessionID: 's1', agent: 'build' },
    );
    const texts = out.map((m) => m.parts.find((p) => p.type === 'text')?.text);
    expect(texts).toEqual(['hi', 'answer', '[omp:compactionSummary] c1', '[omp:advisor] note']);
    // Standalone segments keep the pairing conversation deterministic; the
    // execution/mention projections themselves assert below.
  });

  test('execution and file-mention roles project as user-side standalone segments (05 §5.10)', () => {
    const out = projectConversation(
      [
        { role: 'user', content: 'go', timestamp: now },
        { role: 'assistant', content: [{ type: 'text', text: 'ran' }], model: 'p/m', timestamp: now + 1 },
        { role: 'bashExecution', command: 'ls', output: 'a b', exitCode: 0, cancelled: false, timestamp: now + 2 },
        { role: 'pythonExecution', code: 'print(1)', output: '1', exitCode: 0, cancelled: false, timestamp: now + 3 },
        { role: 'fileMention', files: [{ path: 'x.ts', lineCount: 5 }], timestamp: now + 4 },
      ],
      { sessionID: 's1', agent: 'build' },
    );
    const byRole = out.map((m) => m.info);
    const bash = byRole.find((info) => info.metadata?.ompRole === 'bash');
    expect(bash.role).toBe('user');
    expect(bash.parentID).toBeUndefined();
    const py = byRole.find((info) => info.metadata?.ompRole === 'python');
    expect(py.role).toBe('user');
    expect(py.metadata.exitCode).toBe(0);
    const mention = out.find((m) => m.info.metadata?.ompRole === 'file-mention');
    expect(mention.parts[0].text).toContain('└ Read x.ts (5 lines)');
    // Deterministic ids: same input twice → same ids.
    const again = projectConversation(
      [{ role: 'bashExecution', command: 'ls', output: 'a b', exitCode: 0, cancelled: false, timestamp: now + 2 }],
      { sessionID: 's1', agent: 'build' },
    );
    expect(again[0].info.id).toBe(bash.id);
  });
});

const pointerCleanup = [];
afterAll(() => {
  for (const dir of pointerCleanup) {
    try {
      fs.rmSync(dir, { recursive: true, force: true, maxRetries: 5 });
    } catch {
      // best-effort teardown on Windows file locks
    }
  }
});

/**
 * Real Settings for the pointer tests. Sibling suites mock.module the SDK
 * package specifier (stub Settings without loadIsolated); the source-path
 * import pierces that registry so these integration assertions always run
 * against the genuine loader.
 */
const loadRealSettings = async () => {
  const sdk = await import('@oh-my-pi/pi-coding-agent');
  if (typeof sdk.Settings?.loadIsolated === 'function') return sdk.Settings;
  // Package exports block subpaths; import the source file directly.
  const { pathToFileURL } = await import('node:url');
  const settingsFile = path.join(
    process.cwd(), 'node_modules', '@oh-my-pi', 'pi-coding-agent', 'src', 'config', 'settings.ts',
  );
  return (await import(pathToFileURL(settingsFile).href)).Settings;
};

describe('defaultModelPointer (spec 01 §5.3/GAP-03)', () => {
  test('points at modelRoles.default, never the first-sorted model', async () => {
    const Settings = await loadRealSettings();
    const { defaultModelPointer } = await import('./endpoints.ts');
    const agentDir = fs.mkdtempSync(path.join(os.tmpdir(), 'omp-ptr-'));
    const projectDir = fs.mkdtempSync(path.join(os.tmpdir(), 'omp-ptr-proj-'));
    pointerCleanup.push(agentDir, projectDir);
    fs.mkdirSync(path.join(projectDir, '.omp'), { recursive: true });
    fs.writeFileSync(
      path.join(projectDir, '.omp', 'config.yml'),
      'modelRoles:\n  default: p-z/role-model\n',
    );
    const boot = await Settings.loadIsolated({ cwd: projectDir, agentDir });
    const engine = { settingsStoreReady: async () => ({ settingsFor: async () => boot }) };
    const pointer = await defaultModelPointer(engine);
    boot.cancelPendingSaves?.();
    expect(pointer).toEqual({ model: 'p-z/role-model' });
  });
  test('omits the pointer when no role default resolves or the store is absent', async () => {
    const Settings = await loadRealSettings();
    const { defaultModelPointer } = await import('./endpoints.ts');
    const agentDir = fs.mkdtempSync(path.join(os.tmpdir(), 'omp-ptr-2-'));
    pointerCleanup.push(agentDir);
    const boot = await Settings.loadIsolated({ cwd: agentDir, agentDir });
    const pointer = await defaultModelPointer({ settingsStoreReady: async () => ({ settingsFor: async () => boot }) });
    boot.cancelPendingSaves?.();
    expect(pointer).toEqual({});
    expect(await defaultModelPointer({ settingsStoreReady: async () => null })).toEqual({});
    expect(await defaultModelPointer({ settingsStoreReady: async () => { throw new Error('boom'); } })).toEqual({});
  });
});

describe('registerEndpoints mounting smoke (wiring regression guard)', () => {
  test('mounts every route group against a stub engine without throwing', async () => {
    const { registerEndpoints } = await import('./endpoints.ts');
    const routes = [];
    const route = (method, pattern, handler) => routes.push({ method, pattern, handler });
    const stubEngine = asEngineDouble({
      ompBus: new OmpEventBus(),
      dialogs: { mount: (r) => { r('GET', '/omp/dialogs-stub', async () => undefined); } },
      modesDomain: {},
      uriDomain: { mount: (r) => { r('GET', '/omp/uri-stub', async () => undefined); } },
      settingsStoreReady: async () => null,
      settingsStore: null,
      customAgents: new Map(),
      ready: async () => {},
      availableModels: () => [],
    });
    expect(() => {
      const { sseHandler } = registerEndpoints(route, stubEngine, { version: 'test' });
      expect(typeof sseHandler).toBe('function');
    }).not.toThrow();
    // The parity surface mounts its core routes.
    const patterns = routes.map((r) => r.pattern);
    for (const expected of [
      '/omp/capabilities',
      '/omp/events',
      '/omp/models',
      '/omp/settings',
      '/omp/sessions/{id}/telemetry',
      '/agent-dir',
    ]) {
      expect(patterns).toContain(expected);
    }
  });

  test('route store wrapper forwards the full write surface (invalidateDerived)', async () => {
    // Regression: registerModelSettingsRoutes receives a piecemeal wrapper,
    // not the engine's store. applySettingsChanges calls invalidateDerived()
    // after global-scope writes; a wrapper missing it turned every global
    // PUT into a 500 settings-write-failed (TypeError swallowed by the
    // generic catch).
    const { registerEndpoints } = await import('./endpoints.ts');
    const routes = [];
    const route = (method, pattern, handler) => routes.push({ method, pattern, handler });
    const calls = [];
    const roles = {};
    const bootStub = {
      setModelRole: (role, value) => { roles[role] = value; },
      getModelRole: (role) => roles[role],
      flush: async () => {},
    };
    const storeStub = {
      settingsFor: async () => ({ get: () => undefined, getCwd: () => '/stub' }),
      getRevision: () => 1,
      bumpRevision: () => 2,
      chainWrites: async (_key, task) => task(),
      invalidateDerived: async () => { calls.push('invalidateDerived'); },
      disposeAll: async () => [],
      boot: bootStub,
      bootDirectory: '/stub',
    };
    const stubEngine = asEngineDouble({
      ompBus: new OmpEventBus(),
      dialogs: { mount: () => {} },
      modesDomain: {},
      uriDomain: { mount: () => {} },
      settingsStoreReady: async () => storeStub,
      settingsStore: storeStub,
      customAgents: new Map(),
      ready: async () => {},
      availableModels: () => [],
    });
    registerEndpoints(route, stubEngine, { version: 'test' });
    const put = routes.find((r) => r.method === 'PUT' && r.pattern === '/omp/settings').handler;
    const response = await put(new Request('http://host/omp/settings', {
      method: 'PUT',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ directory: '/stub', changes: { 'modelRoles.smol': 'prov/x' } }),
    }));
    expect(response.status).toBe(200);
    expect(calls).toContain('invalidateDerived');
  });

  test('DELETE /session/{sessionID} answers 200 with literal true (wire 200: boolean)', async () => {
    // Regression: the handler returned json({}). The shared UI client treats
    // the body as the OpenCode boolean confirmation (client.ts deleteSession
    // checks `=== true`), so every managed-runtime delete reported failure
    // even though the engine had already deleted the session.
    const { registerEndpoints } = await import('./endpoints.ts');
    const routes = [];
    const route = (method, pattern, handler) => routes.push({ method, pattern, handler });
    const calls = [];
    const stubEngine = asEngineDouble({
      ompBus: new OmpEventBus(),
      dialogs: { mount: () => {} },
      modesDomain: {},
      uriDomain: { mount: () => {} },
      settingsStoreReady: async () => null,
      settingsStore: null,
      customAgents: new Map(),
      ready: async () => {},
      availableModels: () => [],
      deleteSession: async (args) => { calls.push(args); return null; },
    });
    registerEndpoints(route, stubEngine, { version: 'test' });
    const deleteRoute = routes.find((r) => r.method === 'DELETE' && r.pattern === '/session/{sessionID}');
    const url = new URL('http://host/session/ses_1?directory=/repo');
    const response = await deleteRoute.handler(new Request(url.toString()), {
      url,
      headers: new Headers(),
      params: { sessionID: 'ses_1' },
    });
    expect(response.status).toBe(200);
    expect(await response.json()).toBe(true);
    expect(calls).toHaveLength(1);
    expect(calls[0].sessionID).toBe('ses_1');
  });

  test('session abort route forwards to engine.abort and answers 200 boolean', async () => {
    // Regression: the vendored client POSTs /session/{sessionID}/abort for
    // every stop affordance (composer stop button, Esc shortcut, mobile
    // pill). The route was never registered, so the omp host answered 404
    // and abortCurrentOperation silently swallowed the error — generation
    // could not be stopped on the embedded engine.
    const { registerEndpoints } = await import('./endpoints.ts');
    const routes = [];
    const route = (method, pattern, handler) => routes.push({ method, pattern, handler });
    const abortCalls = [];
    const stubEngine = asEngineDouble({
      ompBus: new OmpEventBus(),
      dialogs: { mount: () => {} },
      modesDomain: {},
      uriDomain: { mount: () => {} },
      settingsStoreReady: async () => null,
      settingsStore: null,
      customAgents: new Map(),
      ready: async () => {},
      availableModels: () => [],
      abort: async (args) => { abortCalls.push(args); return true; },
    });
    registerEndpoints(route, stubEngine, { version: 'test' });
    const abortRoute = routes.find(
      (r) => r.method === 'POST' && r.pattern === '/session/{sessionID}/abort',
    );
    expect(abortRoute).toBeDefined();
    const url = new URL('http://host/session/ses_1/abort?directory=/repo');
    const response = await abortRoute.handler(new Request(url.toString(), { method: 'POST' }), {
      url,
      headers: new Headers(),
      params: { sessionID: 'ses_1' },
    });
    expect(response.status).toBe(200);
    expect(await response.json()).toBe(true);
    expect(abortCalls).toEqual([{ sessionID: 'ses_1', directory: '/repo' }]);
  });

  test('PATCH /session/{sessionID} answers 404 when the engine refuses a mis-addressed update', async () => {
    // Regression (mis-addressed archive incident): engine.updateSession used
    // to fabricate a synthesized session for any directory, so an archive
    // addressed to a directory that owns neither transcript nor registry
    // entry answered 200 while no listing could ever observe the write.
    const { registerEndpoints } = await import('./endpoints.ts');
    const routes = [];
    const route = (method, pattern, handler) => routes.push({ method, pattern, handler });
    const calls = [];
    const stubEngine = asEngineDouble({
      ompBus: new OmpEventBus(),
      dialogs: { mount: () => {} },
      modesDomain: {},
      uriDomain: { mount: () => {} },
      settingsStoreReady: async () => null,
      settingsStore: null,
      customAgents: new Map(),
      ready: async () => {},
      availableModels: () => [],
      updateSession: async (args) => { calls.push(args); return null; },
    });
    registerEndpoints(route, stubEngine, { version: 'test' });
    const patchRoute = routes.find((r) => r.method === 'PATCH' && r.pattern === '/session/{sessionID}');
    const response = await patchRoute.handler(new Request('http://host/session/ses_1', {
      method: 'PATCH',
      body: JSON.stringify({ directory: '/elsewhere', time: { archived: 123 } }),
    }), {
      url: new URL('http://host/session/ses_1'),
      headers: new Headers(),
      params: { sessionID: 'ses_1' },
    });
    expect(response.status).toBe(404);
    expect(calls).toEqual([{
      sessionID: 'ses_1', directory: '/elsewhere', title: undefined, metadata: undefined, timeArchived: 123,
    }]);
  });

  test('GET /experimental/session scopes by directory and stays global without one', async () => {
    // Scoped callers (directory bootstrap, per-directory refresh) contract:
    // only the named directory's sessions. Answering with every directory's
    // sessions seeded foreign records into every directory child store.
    const { registerEndpoints } = await import('./endpoints.ts');
    const routes = [];
    const route = (method, pattern, handler) => routes.push({ method, pattern, handler });
    const stubEngine = asEngineDouble({
      ompBus: new OmpEventBus(),
      dialogs: { mount: () => {} },
      modesDomain: {},
      uriDomain: { mount: () => {} },
      settingsStoreReady: async () => null,
      settingsStore: null,
      customAgents: new Map(),
      ready: async () => {},
      availableModels: () => [],
      listAllSessions: async () => new Map([
        ['/repo', [{ id: 'ses_repo', directory: '/repo', time: { updated: 2 } }]],
        ['/other', [{ id: 'ses_other', directory: '/other', time: { updated: 1 } }]],
      ]),
    });
    registerEndpoints(route, stubEngine, { version: 'test' });
    const listRoute = routes.find((r) => r.method === 'GET' && r.pattern === '/experimental/session');

    const scopedUrl = new URL('http://host/experimental/session?directory=/repo');
    const scoped = await listRoute.handler(new Request(scopedUrl.toString()), { url: scopedUrl, headers: new Headers(), params: {} });
    expect((await scoped.json()).map((session) => session.id)).toEqual(['ses_repo']);

    const globalUrl = new URL('http://host/experimental/session');
    const global = await listRoute.handler(new Request(globalUrl.toString()), { url: globalUrl, headers: new Headers(), params: {} });
    expect((await global.json()).map((session) => session.id)).toEqual(['ses_repo', 'ses_other']);
  });
});

describe('capabilities (spec 05 §5.2.3, master R2)', () => {
  test('advertises event schema and feature flags', () => {
    const caps = buildCapabilities();
    expect(caps.eventSchema).toBe('1.0');
    expect(caps.features.events).toBe(true);
    // Landed domains are on; upstream-gated surfaces stay off (R12/R14/R15).
    expect(caps.features['dialogs.v1']).toBe(true);
    expect(caps.features['queue.v1']).toBe(false);
    expect(caps.features['jobs.v1']).toBe(false);
    expect(caps.features['mcp.executable']).toBe(false);
    expect(caps.features['commands.v1']).toBe(true);
    expect(Object.keys(ompFeatures())).toContain('modelRoles.v1');
  });
});

describe('coverage CI guard (scripts/check-event-coverage.mjs)', () => {
  const repoRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '../../../../..');
  const guard = path.join(repoRoot, 'scripts/check-event-coverage.mjs');

  test('passes on the current tree', () => {
    const stdout = execFileSync('node', [guard, '--skip-name-scan'], { encoding: 'utf8' });
    expect(stdout).toMatch(/OK — 24 SDK members covered/);
  });

  test('fails when the SDK union grows an unregistered member', () => {
    const fixture = fs.mkdtempSync(path.join(os.tmpdir(), 'omp-events-'));
    fs.mkdirSync(path.join(fixture, 'src/session'), { recursive: true });
    fs.writeFileSync(
      path.join(fixture, 'src/session/agent-session-events.ts'),
      [
        'import type { AgentEvent } from "@oh-my-pi/pi-agent-core";',
        'export type AgentSessionEvent =',
        '  | Exclude<AgentEvent, { type: "agent_end" }>',
        '  | { type: "brand_new_sdk_event"; payload: string };',
      ].join('\n'),
    );
    let output = '';
    try {
      output = execFileSync('node', [guard, '--sdk-dist', fixture, '--skip-name-scan'], { encoding: 'utf8' });
      throw new Error('guard should have failed');
    } catch (error) {
      const text = `${error.stderr ?? ''}${error.stdout ?? ''}${output ?? ''}`;
      expect(text).toMatch(/SDK event "brand_new_sdk_event" has no disposition entry/);
    } finally {
      fs.rmSync(fixture, { recursive: true, force: true });
    }
  });
});
