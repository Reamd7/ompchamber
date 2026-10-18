/**
 * Red→Green regression tests for the three reported breakages:
 *
 * 1. Slash command channel — POST /session/{id}/command dropped `arguments`
 *    and the leading '/' (endpoints.ts sent the bare command name to
 *    engine.prompt, so the model received literal "e2e-echo" and the args
 *    vanished). Contract after fix: the server reconstructs
 *    "/<command> <arguments>" (OpenCode wire semantics: command name without
 *    slash + arguments string); 'skill:<name>' keeps its skill: form so the
 *    engine's skill path (#tryRunSkillCommand → parseSkillInvocation) hits;
 *    the route answers 200 with the projected user message.
 * 2. Plan mode activation — entering plan mode only wrote a mode_change entry
 *    and attached the review hook; the SDK session itself was never switched
 *    into plan state (no setPlanModeState / setActiveToolsByName), so the
 *    model kept the full toolset and preparePlanForReview threw
 *    "Plan mode is not active.". Contract after fix: the mode route awaits
 *    full SDK activation — setPlanModeState({enabled:true,...}) BEFORE the
 *    tool partition, then setActiveToolsByName with the plan toolset.
 * 3. Interactive ask roster — the ask tool never entered the SDK tool roster
 *    (E2E-observed: even lease-held UI sessions enumerate tools without ask,
 *    and headless-materialized sessions freeze it out permanently).
 *    Contract after fix: createAgentSession always receives
 *    interactivePrompts:true; execution stays fail-closed through the tool
 *    UI context (lease-driven), which existing domain-dialogs tests pin.
 *
 * Harness seam: same as omp-host.lifecycle.test.ts — bun's mock.module must
 * register before the engine module loads, so the engine and host imports are
 * dynamic inside createHarness (a test module-loading boundary; static
 * imports would bind the un-mocked SDK).
 */
// 三个上报问题的 Red→Green 回归测试（接缝同 omp-host.lifecycle.test.ts：
// bun 的 mock.module 必须先于 engine 模块加载注册，故 engine/host 在
// createHarness 内动态导入——静态导入会绑到未 mock 的 SDK 面）。
import { describe, expect, mock, test } from 'bun:test';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import type { OmpHostEngine } from './engine.ts';

// 真实 SDK 面：mock 展开在其上，仅替换被桩成员。
const realSdk = await import('@oh-my-pi/pi-coding-agent');
const realRuntimeInit = await import('@oh-my-pi/pi-coding-agent/modes/runtime-init');

/** 顶层 mock 状态（createHarness 每次整体重置）。 */
const mockState = { agentDir: '', files: [] as Array<{ path: string; cwd: string }> };

/** createAgentSession 收到的 options（interactivePrompts 断言用）。 */
const createdOptions: Array<{ hasUI?: boolean; interactivePrompts?: boolean; cwd?: string }> = [];

/** 假会话上 plan/prompt 相关 SDK 调用的记录器。 */
interface PlanRecorders {
  planStateCalls: Array<{ enabled: boolean; planFilePath?: string } | undefined>;
  activeToolsCalls: string[][];
  planModeContextCalls: number;
  restorePresentationCalls: number;
  promptTexts: string[];
  /** 引擎挂上的 xd://propose 处理器（驱动评审链）。 */
  proposalHandlers: Array<(title: string) => Promise<unknown>>;
}

/** 本套件假会话（lifecycle 形状 + plan/prompt 记录器）。 */
interface RedGreenFixtureSession {
  sessionManager: {
    getSessionName: () => string;
    getArtifactsDir: () => string;
    onSessionNameChanged: () => () => void;
    appendModeChange: () => string;
    buildSessionContext: () => { messages: unknown[] };
  };
  model: { provider: string; id: string };
  messages: unknown[];
  isStreaming: boolean;
  subscribe: () => () => void;
  prompt: (text?: string) => Promise<boolean>;
  executeBash: () => Promise<unknown>;
  isBashRunning: boolean;
  getTodoPhases: () => [];
  steer: () => Promise<void>;
  abort: () => Promise<void>;
  dispose: () => Promise<void>;
  setModel: () => Promise<unknown>;
  setThinkingLevel: () => void;
  maybeStartTitleGeneration: () => void;
  // plan-mode SDK surface（修复后引擎应触达的成员）。
  getEnabledToolNames: () => string[];
  getMountedXdevToolNames: () => string[];
  hasBuiltInTool: (name: string) => boolean;
  setPlanModeState: (state: { enabled: boolean; planFilePath?: string } | undefined) => void;
  getPlanModeState: () => { enabled: boolean; planFilePath?: string } | undefined;
  setActiveToolsByName: (names: string[]) => Promise<void>;
  restoreNonMCPToolPresentation: (enabled: string[], mounted: string[]) => Promise<void>;
  sendPlanModeContext: (options?: { deliverAs?: string }) => Promise<void>;
  /** setPlanProposalHandler 存儲的处理器（测试驱动 xd://propose 用）。 */
  proposalHandlers: Array<(title: string) => Promise<unknown>>;
  preparePlanForReview: (title: string) => Promise<{ content: unknown[]; details: { planFilePath: string; title: string; planExists: boolean } }>;
  setPlanReferencePath: (planFilePath: string) => void;
  markPlanReferenceSent: () => void;
  setPlanProposalHandler: (handler: unknown) => void;
  resolveRoleModelWithThinking: (role: string) => { model?: unknown; thinkingLevel?: unknown };
  setModelTemporary: () => Promise<void>;
  configuredThinkingLevel: () => undefined;
}

/** 逐会话记录器（断言读取）。 */
const recordersBySession = new Map<string, PlanRecorders>();

/** harness 内共享的假会话表（按 sessionID）。 */
const sessionFixtures = new Map<string, RedGreenFixtureSession>();

/** 构造带 plan/prompt 记录器的假会话。 */
const makeFixtureSession = (id: string, dir: string): RedGreenFixtureSession => {
  const recorders: PlanRecorders = {
    planStateCalls: [],
    activeToolsCalls: [],
    planModeContextCalls: 0,
    restorePresentationCalls: 0,
    promptTexts: [],
    proposalHandlers: [],
  };
  recordersBySession.set(id, recorders);
  return {
    sessionManager: {
      getSessionName: () => `name-${id}`,
      getArtifactsDir: () => path.join(dir, id),
      onSessionNameChanged: () => () => {},
      appendModeChange: () => `mode_${id}_1`,
      buildSessionContext: () => ({ messages: [] }),
    },
    model: { provider: 'p1', id: 'm1' },
    messages: [],
    isStreaming: false,
    subscribe: () => () => {},
    prompt: async (text?: string) => {
      recorders.promptTexts.push(text ?? '');
      return true;
    },
    executeBash: async () => ({ output: '', exitCode: 0, cancelled: false, truncated: false }),
    isBashRunning: false,
    getTodoPhases: () => [],
    steer: async () => {},
    abort: async () => {},
    dispose: async () => {},
    setModel: async () => ({ switched: true }),
    setThinkingLevel: () => {},
    maybeStartTitleGeneration: () => {},
    getEnabledToolNames: () => ['read', 'write', 'bash', 'edit'],
    getMountedXdevToolNames: () => [],
    hasBuiltInTool: (name: string) => name === 'write' || name === 'edit' || name === 'read',
    setPlanModeState: (state) => {
      recorders.planStateCalls.push(state);
    },
    getPlanModeState: () => undefined,
    setActiveToolsByName: async (names: string[]) => {
      recorders.activeToolsCalls.push(names);
    },
    restoreNonMCPToolPresentation: async () => {
      recorders.restorePresentationCalls++;
    },
    sendPlanModeContext: async () => {
      recorders.planModeContextCalls++;
    },
    setPlanProposalHandler: (handler: unknown) => {
      if (handler === null) {
        recorders.proposalHandlers = [];
        return;
      }
      // SAFETY: fixture double of the SDK handler shape the engine attaches.
      recorders.proposalHandlers = [handler as (title: string) => Promise<unknown>];
    },
    get proposalHandlers() {
      return recorders.proposalHandlers;
    },
    preparePlanForReview: async (title: string) => ({
      content: [{ type: 'text', text: 'THE E2E PLAN' }],
      details: { planFilePath: 'local://PLAN.md', title, planExists: true },
    }),
    setPlanReferencePath: () => {},
    markPlanReferenceSent: () => {},
    resolveRoleModelWithThinking: () => ({}),
    setModelTemporary: async () => {},
    configuredThinkingLevel: () => undefined,
  };
};

mock.module('@oh-my-pi/pi-coding-agent/modes/runtime-init', () => ({
  ...realRuntimeInit,
  initializeExtensions: async () => {},
}));

mock.module('@oh-my-pi/pi-coding-agent', () => ({
  ...realSdk,
  AgentRegistry: class {},
  ModelRegistry: class {
    async refresh() {}
    getAvailable() {
      return [{ provider: 'p1', id: 'm1' }];
    }
  },
  SessionManager: Object.assign(
    class {},
    {
      // SAFETY: fixture double for the SDK manager; only the members the
      // engine touches on this harness path exist.
      async open(file: string) {
        return {
          getSessionId: () => path.basename(file, '.jsonl'),
          onSessionNameChanged: () => () => {},
          getHeader: (): null => null,
          getEntries: () => [],
          buildSessionContext: () => ({ messages: [] }),
          getCwd: () => undefined,
          getSessionName: () => undefined,
          getArtifactsDir: () => file.slice(0, -'.jsonl'.length),
          close: async () => {},
          moveTo: async () => {},
        };
      },
      async list(cwd?: string) {
        return mockState.files
          .filter((file) => !cwd || file.cwd === cwd)
          .map((file) => ({ id: path.basename(file.path, '.jsonl'), path: file.path }));
      },
      getDefaultSessionDir: () => path.join(mockState.agentDir, 'sessions'),
      async forkFrom() {
        throw new Error('not needed');
      },
      createEmptySessionFile: () => 'unused',
    },
  ),
  // The engine always uses its injected factory; the module-level export is
  // never reached on this harness path.
  createAgentSession: async () => {
    throw new Error('injected factory must be used');
  },
}));

interface Harness {
  engine: OmpHostEngine;
  sessions: Map<string, RedGreenFixtureSession>;
  agentDir: string;
  host: { baseUrl: string; close: () => Promise<void> };
}

const createHarness = async ({ files }: { files?: Array<{ id: string; cwd: string }> } = {}): Promise<Harness> => {
  const agentDir = fs.mkdtempSync(path.join(os.tmpdir(), 'omp-redgreen-'));
  mockState.agentDir = agentDir;
  mockState.files = [];
  createdOptions.length = 0;
  recordersBySession.clear();
  sessionFixtures.clear();

  for (const file of files ?? [{ id: 's1', cwd: '/repo' }]) {
    const filePath = path.join(agentDir, 'sessions', `${file.id}.jsonl`);
    mockState.files.push({ path: filePath, cwd: file.cwd });
    fs.mkdirSync(path.dirname(filePath), { recursive: true });
    fs.writeFileSync(filePath, '');
  }

  // Dynamic import: bun's mock.module above must bind before the engine and
  // host modules load (module-loading boundary — see file header).
  const { OmpHostEngine: Engine } = await import('./engine.ts');
  const { startOmpHost } = await import('./host.ts');
  const engine = new Engine({
    agentDir,
    createAgentSession: (async (options: {
      cwd?: string;
      sessionManager?: { getSessionId?: () => string };
      hasUI?: boolean;
      interactivePrompts?: boolean;
    }) => {
      createdOptions.push(options);
      const id = options.sessionManager?.getSessionId?.() ?? 's1';
      let fixture = sessionFixtures.get(id);
      if (!fixture) {
        fixture = makeFixtureSession(id, options.cwd ?? '/repo');
        sessionFixtures.set(id, fixture);
      }
      // SAFETY: fixture is a duck-typed double for the SDK session; the
      // engine only calls the members RedGreenFixtureSession declares.
      return { session: fixture as never, setToolUIContext: () => {} };
    }) as never,
  });
  const host = await startOmpHost({ engine, port: 0 });
  return { engine, sessions: sessionFixtures, agentDir, host };
};

describe('RED→GREEN 1: /session/{id}/command reconstructs the slash invocation', () => {
  test('command + arguments reach engine.prompt as "/cmd args" with a 200 answer', async () => {
    const h = await createHarness();
    try {
      // Materialize the session first (the command route needs a live record).
      await h.engine.prompt({ sessionID: 's1', directory: '/repo', text: 'warm' });
      const rec = recordersBySession.get('s1')!;

      const res = await fetch(`${h.host.baseUrl}/session/s1/command`, {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify({ command: 'e2e-echo', arguments: 'hello world', directory: '/repo' }),
      });
      expect(res.status).toBe(200);
      // The last prompt dispatch must carry the reconstructed slash text —
      // Red behavior sent the bare name "e2e-echo" and dropped the args.
      expect(rec.promptTexts[rec.promptTexts.length - 1]).toBe('/e2e-echo hello world');
    } finally {
      await h.host.close();
    }
  });

  test('skill commands keep the skill: form so the engine skill path hits', async () => {
    const h = await createHarness();
    try {
      await h.engine.prompt({ sessionID: 's1', directory: '/repo', text: 'warm' });
      const rec = recordersBySession.get('s1')!;

      await fetch(`${h.host.baseUrl}/session/s1/command`, {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify({ command: 'skill:pr-review', arguments: 'focus on auth', directory: '/repo' }),
      });
      expect(rec.promptTexts[rec.promptTexts.length - 1]).toBe('/skill:pr-review focus on auth');
    } finally {
      await h.host.close();
    }
  });
});

describe('RED→GREEN 2: entering plan mode activates the SDK plan state', () => {
  test('POST /omp/sessions/{id}/mode {mode:"plan"} awaits setPlanModeState then setActiveToolsByName', async () => {
    const h = await createHarness();
    try {
      await h.engine.prompt({ sessionID: 's1', directory: '/repo', text: 'warm' });
      const rec = recordersBySession.get('s1')!;

      const res = await fetch(`${h.host.baseUrl}/omp/sessions/s1/mode?directory=/repo`, {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify({ mode: 'plan' }),
      });
      expect(res.status).toBe(200);
      // The route response settles after the (async) SDK activation, so the
      // recorders are authoritative at this point — no timer settling.
      // Red behavior: zero SDK-side plan activation.
      expect(rec.planStateCalls.length).toBeGreaterThanOrEqual(1);
      expect(rec.planStateCalls[0]?.enabled).toBe(true);
      expect(rec.activeToolsCalls.length).toBeGreaterThanOrEqual(1);
      // The plan toolset is the previous set with the built-in write kept as
      // the xd://propose staging transport.
      expect(rec.activeToolsCalls[0]).toContain('write');
    } finally {
      await h.host.close();
    }
  });
});

describe('RED→GREEN 2b: plan approval dispatches the execution chain', () => {
  test('decide approve-execute tears plan down, restores tools, dispatches the synthetic prompt', async () => {
    const h = await createHarness();
    try {
      await h.engine.prompt({ sessionID: 's1', directory: '/repo', text: 'warm' });
      const rec = recordersBySession.get('s1')!;
      const fixture = h.sessions.get('s1')!;

      // Enter plan mode (attaches the review hook to the fixture session).
      const enter = await fetch(`${h.host.baseUrl}/omp/sessions/s1/mode?directory=/repo`, {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify({ mode: 'plan' }),
      });
      expect(enter.status).toBe(200);

      // Fire the xd://propose hook the way the SDK would: the handler stored
      // by setPlanProposalHandler prepares a review and parks a pending.
      expect(fixture.proposalHandlers.length).toBe(1);
      // SAFETY: fixture double of the SDK handler; resolves with the review
      // tool result once the decision arrives.
      const proposalSettled = fixture.proposalHandlers[0]('e2e-plan');
      await new Promise((resolve) => setTimeout(resolve, 30));

      const res = await fetch(`${h.host.baseUrl}/omp/sessions/s1/plan/review?directory=/repo`, {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify({ choice: 'approve-execute' }),
      });
      expect(res.status).toBe(200);
      const body = (await res.json()) as { dispatched?: boolean; mode?: string };
      expect(body.dispatched).toBe(true);
      expect(body.mode).toBe('none');

      // SDK teardown happened: plan state cleared and presentation restored.
      expect(rec.planStateCalls.at(-1)).toBeUndefined();
      expect(rec.restorePresentationCalls).toBeGreaterThanOrEqual(1);
      // The synthetic plan-approved prompt was dispatched to the session.
      const lastPrompt = rec.promptTexts.at(-1) ?? '';
      expect(lastPrompt).toContain('Plan approved.');
      expect(lastPrompt).toContain('THE E2E PLAN');
      // The propose hook settled with the approval tool result.
      await expect(proposalSettled).resolves.toMatchObject({
        content: [{ type: 'text', text: 'Plan approved (approve-execute).' }],
      });
    } finally {
      await h.host.close();
    }
  });
});
describe('RED→GREEN 3: sessions advertise interactive prompts (ask roster)', () => {
  test('createAgentSession receives interactivePrompts:true so the ask tool enters the roster', async () => {
    const h = await createHarness();
    try {
      // No UI lease is held — the session materializes headless.
      await h.engine.prompt({ sessionID: 's1', directory: '/repo', text: 'warm' });
      // Red behavior: interactivePrompts is undefined → AskTool.createIf
      // ((canPromptUser ?? hasUI) || advertiseHeadlessAsk) keeps ask out of
      // the roster for this session's whole lifetime (E2E-observed for both
      // lease-held and headless sessions).
      expect(createdOptions.length).toBeGreaterThanOrEqual(1);
      expect(createdOptions[createdOptions.length - 1]?.interactivePrompts).toBe(true);
    } finally {
      await h.host.close();
    }
  });
});
