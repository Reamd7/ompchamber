// Chapter-04 domain tests (spec 04 §5.2/§5.4/§5.5/§5.6, master R2-H2/R7/R8/R12,
// R2-M5). The local:// suite exercises the REAL SDK router + handler
// (containment, session pinning, traversal rejection are the SDK's own —
// we only pin options per request), so the isolation guarantees here are
// end-to-end, not mocks of them.

import { describe, test, expect, afterAll } from 'bun:test';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { readFileSync } from 'node:fs';
import {
  uriCapabilities,
  createLocalProtocolOptions,
  treeUpdatedPayload,
  UriTokenService,
  handleUriResolve,
  buildSessionTree,
  buildSessionSubtree,
  buildEntryTreeSnapshot,
  normalizeNavigateRequest,
  normalizeLabelRequest,
  navigateBusyResponse,
  OMP_AGENTS_UPDATED,
  AgentRunsAggregator,
  projectAgentRun,
  ParkedAgentDescriptors,
  handleAgentRunAction,
  handleJobsRequest,
  artifactsDirForSessionFile,
  JOBS_UNAVAILABLE_REASON,
  createUriDomain,
} from './domain-uri.ts';
import { ompFeatures } from './omp-parity.ts';
import type { UriRequestBody, UriResolveBody } from './domain-uri.ts';

const DIRECTORY = 'C:/proj/alpha';

// ---------------------------------------------------------------------------
// shared fixtures
// ---------------------------------------------------------------------------

const base = fs.mkdtempSync(path.join(os.tmpdir(), 'oc-domain-uri-'));
const artifactsOf = (sessionId) => path.join(base, sessionId);
const writeLocal = (sessionId, file, content) => {
  const target = path.join(artifactsOf(sessionId), 'local', file);
  fs.mkdirSync(path.dirname(target), { recursive: true });
  fs.writeFileSync(target, content);
  return target;
};
writeLocal('ses_A', 'scratch.md', 'alpha session secret');
writeLocal('ses_A', 'notes/deep.md', 'nested note');
writeLocal('ses_B', 'scratch.md', 'beta session secret');

const localOptionsFor = (sessionID, directory) => {
  if (!fs.existsSync(artifactsOf(sessionID))) return null;
  return createLocalProtocolOptions(sessionID, directory, artifactsOf(sessionID));
};
/** Asserted fields of the JSON bodies the resolve/open/info endpoints return
 * (superset per response; unasserted fields stay untyped). */
interface ResolveResponseBody {
  content?: string;
  url?: string;
  contentType?: string;
  filename?: string;
  editable?: boolean;
  scheme?: string;
  error?: string;
  message?: string;
  /** Binary descriptor arm (previewable images): bytes stream via the token
   * content endpoint instead of an inline body. */
  binary?: boolean;
  immutable?: boolean;
  size?: number;
  revivable?: boolean;
  token?: { id: string; expiresAt: number };
}

const tokens = new UriTokenService();
const resolveBody = (body: UriResolveBody): Promise<{ status: number; body: ResolveResponseBody }> =>
  handleUriResolve({ body, localOptionsFor, tokens }).then((r) =>
    // SAFETY: the resolve/open endpoints answer the ResolveResponseBody wire
    // shape this helper exists to assert; json() only loses that type.
    r.json().then((data) => ({ status: r.status, body: data as ResolveResponseBody })),
  );

afterAll(() => {
  fs.rmSync(base, { recursive: true, force: true, maxRetries: 5 });
});

// ---------------------------------------------------------------------------
// §5.2 capability matrix + options factory
// ---------------------------------------------------------------------------

describe('uriCapabilities + createLocalProtocolOptions (spec 04 §5.2, R7/R8)', () => {
  test('P1 matrix is local:// read only, no router-mediated writes', () => {
    expect(uriCapabilities()).toEqual({ read: ['local'], write: [] });
  });

  test('options pin sessionId and resolve artifactsDir from constant or provider', () => {
    const fromConstant = createLocalProtocolOptions('ses_A', DIRECTORY, 'X:/artifacts/A');
    expect(fromConstant.getSessionId()).toBe('ses_A');
    expect(fromConstant.getArtifactsDir()).toBe('X:/artifacts/A');
    const fromProvider = createLocalProtocolOptions('ses_B', DIRECTORY, (sid) =>
      sid === 'ses_B' ? 'X:/artifacts/B' : null,
    );
    expect(fromProvider.getArtifactsDir()).toBe('X:/artifacts/B');
    expect(createLocalProtocolOptions('ses_C', DIRECTORY, () => undefined).getArtifactsDir()).toBeNull();
  });

  test('sessionId is required', () => {
    expect(() => createLocalProtocolOptions('', DIRECTORY, 'X:/a')).toThrow(TypeError);
  });

  test('module source never mutates SDK global router state (R2-H2)', () => {
    const source = readFileSync(new URL('./domain-uri.ts', import.meta.url), 'utf8');
    expect(/registerArtifactsDir\(/.test(source)).toBe(false);
    expect(/\.setOverride\(/.test(source)).toBe(false);
  });

  test('artifactsDirForSessionFile derives the per-session dir, rejects non-transcripts', () => {
    expect(artifactsDirForSessionFile('C:/sess/2026-08-27T10-00-00Z_ses_A.jsonl')).toBe(
      'C:/sess/2026-08-27T10-00-00Z_ses_A',
    );
    expect(artifactsDirForSessionFile('C:/sess/notes.txt')).toBeNull();
    expect(artifactsDirForSessionFile(undefined)).toBeNull();
  });

  test('resolve accepts an async localOptionsFor hook (engine cold path)', async () => {
    const { status, body } = await handleUriResolve({
      body: { scheme: 'local', ref: 'scratch.md', sessionID: 'ses_A', directory: DIRECTORY },
      localOptionsFor: async (sessionID, directory) => {
        await Promise.resolve();
        return createLocalProtocolOptions(sessionID, directory, artifactsOf(sessionID));
      },
      tokens,
    }).then((r) =>
      // SAFETY: the resolve endpoint answers the ResolveResponseBody wire shape.
      r.json().then((data) => ({ status: r.status, body: data as ResolveResponseBody })),
    );
    expect(status).toBe(200);
    expect(body.content).toBe('alpha session secret');
  });
});

// ---------------------------------------------------------------------------
// §5.2.1 resolve endpoint
// ---------------------------------------------------------------------------

describe('local:// resolve (spec 04 §5.2.1, R2-H2/R7)', () => {
  test('resolves own file, strips sourcePath, mints opaque token', async () => {
    const { status, body } = await resolveBody({
      scheme: 'local',
      ref: 'scratch.md',
      sessionID: 'ses_A',
      directory: DIRECTORY,
    });
    expect(status).toBe(200);
    expect(body.content).toBe('alpha session secret');
    expect(body.url).toBe('local://scratch.md');
    expect('sourcePath' in body).toBe(false);
    expect(JSON.stringify(body)).not.toContain(base.replaceAll('\\', '/'));
    expect(body.token.id).toMatch(/^ocuri_[A-Za-z0-9_-]{43}$/);
    expect(typeof body.token.expiresAt).toBe('number');
  });

  test('same directory, different sessions are isolated (session pinning)', async () => {
    const own = await resolveBody({ scheme: 'local', ref: 'scratch.md', sessionID: 'ses_A', directory: DIRECTORY });
    expect(own.status).toBe(200);
    const cross = await resolveBody({ scheme: 'local', ref: 'scratch.md', sessionID: 'ses_B', directory: DIRECTORY });
    // ses_B resolves inside ITS OWN root: the file exists there too but with
    // different content; a file only present in ses_A is invisible to ses_B.
    expect(cross.body.content).toBe('beta session secret');
    const onlyA = await resolveBody({ scheme: 'local', ref: 'notes/deep.md', sessionID: 'ses_B', directory: DIRECTORY });
    expect(onlyA.status).toBe(404);
    expect(onlyA.body.error).toBe('resolve-failed');
  });

  test('traversal outside the local root is rejected by the handler', async () => {
    const { status, body } = await resolveBody({
      scheme: 'local',
      ref: '../escape.md',
      sessionID: 'ses_A',
      directory: DIRECTORY,
    });
    expect(status).toBe(404);
    expect(body.error).toBe('resolve-failed');
    expect(body.message).toMatch(/traversal/i);
  });

  test.each(['agent', 'history', 'artifact', 'mcp', 'ssh', 'vault', 'security', 'xd', 'skill', 'memory', 'rule', 'omp', 'issue', 'pr'])(
    'non-enabled scheme %s:// → 501 scheme-not-enabled (R2-H2/R2-M11)',
    async (scheme) => {
      const { status, body } = await resolveBody({ scheme, ref: 'x', sessionID: 'ses_A', directory: DIRECTORY });
      expect(status).toBe(501);
      expect(body.error).toBe('scheme-not-enabled');
      // SAFETY: test.each table rows arrive untyped; the row value is the scheme string.
      expect(body.scheme).toBe(scheme as string);
    },
  );

  test('unknown and external schemes → 404 unknown-scheme, MCP fallback never exposed', async () => {
    for (const u of ['foo://x', 'file:///etc/passwd', 'http://example.com/x']) {
      const { status, body } = await resolveBody({ u, sessionID: 'ses_A', directory: DIRECTORY });
      expect(status).toBe(404);
      expect(body.error).toBe('unknown-scheme');
    }
  });

  test('local:// requires sessionID then directory (§5.2.3 pinning)', async () => {
    const noSession = await resolveBody({ scheme: 'local', ref: 'scratch.md', directory: DIRECTORY });
    expect(noSession.status).toBe(400);
    expect(noSession.body.error).toBe('session-required');
    const noDirectory = await resolveBody({ scheme: 'local', ref: 'scratch.md', sessionID: 'ses_A' });
    expect(noDirectory.status).toBe(400);
    expect(noDirectory.body.error).toBe('directory-required');
  });

  test('unknown session → 404 session-not-found; oversized URL → 400', async () => {
    const unknown = await resolveBody({ scheme: 'local', ref: 'x', sessionID: 'ses_missing', directory: DIRECTORY });
    expect(unknown.status).toBe(404);
    expect(unknown.body.error).toBe('session-not-found');
    const long = await resolveBody({ u: `local://${'a'.repeat(3000)}`, sessionID: 'ses_A', directory: DIRECTORY });
    expect(long.status).toBe(400);
    expect(long.body.error).toBe('url-too-long');
  });

  test('bare local:// resolves the session listing with the file links', async () => {
    const { status, body } = await resolveBody({ scheme: 'local', ref: '', sessionID: 'ses_A', directory: DIRECTORY });
    expect(status).toBe(200);
    expect(body.content).toContain('scratch.md');
    expect(body.contentType).toBe('text/markdown');
  });
});

// ---------------------------------------------------------------------------
// §5.2.4 tokens
// ---------------------------------------------------------------------------

describe('resource tokens (spec 04 §5.2.4, R7)', () => {
  test('redeem returns content + basename only; no path in any response', async () => {
    const domain = createUriDomain({
      features: () => ({ ...ompFeatures(), 'uri.v1': true }),
      tokens,
      localOptionsFor,
    });
    const resolved = await domain.uri.resolve({
      body: { scheme: 'local', ref: 'scratch.md', sessionID: 'ses_A', directory: DIRECTORY },
    });
    // SAFETY: resolve answers { token: { id } } per the §5.2.1 wire contract.
    const resource = (await resolved.json()) as ResolveResponseBody;
    const opened = await domain.uri.open({ body: { token: resource.token.id }, directory: DIRECTORY });
    // SAFETY: open answers the same ResolveResponseBody wire shape.
    const openBody = (await opened.json()) as ResolveResponseBody;
    expect(opened.status).toBe(200);
    expect(openBody.content).toBe('alpha session secret');
    expect(openBody.filename).toBe('scratch.md');
    const serialized = JSON.stringify(openBody);
    expect(serialized).not.toContain('absolutePath');
    expect(serialized).not.toContain(base.replaceAll('\\', '/'));
  });

  test('info returns metadata without content and without consuming a read', async () => {
    const domain = createUriDomain({
      features: () => ({ ...ompFeatures(), 'uri.v1': true }),
      tokens,
      localOptionsFor,
    });
    // SAFETY: resolve answers { token: { id } } per the §5.2.1 wire contract.
    const resource = (await (
      await domain.uri.resolve({
        body: { scheme: 'local', ref: 'scratch.md', sessionID: 'ses_A', directory: DIRECTORY },
      })
    ).json()) as ResolveResponseBody;
    const params = new URLSearchParams({ token: resource.token.id });
    // SAFETY: info answers the metadata subset of ResolveResponseBody.
    const info = (await domain.uri.info({ query: params, directory: DIRECTORY }).json()) as ResolveResponseBody;
    expect(info.url).toBe('local://scratch.md');
    expect(info.filename).toBe('scratch.md');
    expect(info.editable).toBe(true);
    expect('content' in info).toBe(false);
    expect(JSON.stringify(info)).not.toContain(base.replaceAll('\\', '/'));
    // still fully redeemable afterwards
    const opened = await domain.uri.open({ body: { token: resource.token.id }, directory: DIRECTORY });
    expect(opened.status).toBe(200);
  });

  test('wrong directory → 403 scope; bogus/expired/exhausted → 404', async () => {
    const controlled = new UriTokenService({ ttlMs: 50, maxReads: 2, now: () => clock });
    let clock = 1_000;
    const issued = controlled.issue({
      resourceUrl: 'local://scratch.md',
      directory: DIRECTORY,
      absolutePath: writeLocal('ses_T', 't.md', 'token test'),
    });
    const wrongDir = await controlled.open(issued.id, { directory: 'C:/other' });
    expect(wrongDir.status).toBe(403);
    expect((await controlled.open('ocuri_bogus', { directory: DIRECTORY })).status).toBe(404);
    expect((await controlled.open(undefined, {})).status).toBe(400);
    expect((await controlled.open(issued.id, { directory: DIRECTORY })).status).toBe(200); // read 1
    expect((await controlled.open(issued.id, { directory: DIRECTORY })).status).toBe(200); // read 2 = max
    expect((await controlled.open(issued.id, { directory: DIRECTORY })).status).toBe(404); // exhausted
    clock += 100;
    const second = controlled.issue({
      resourceUrl: 'local://t.md',
      directory: DIRECTORY,
      absolutePath: writeLocal('ses_T', 't2.md', 'ttl'),
    });
    clock += 100; // past ttl
    expect((await controlled.open(second.id, { directory: DIRECTORY })).status).toBe(404); // expired
  });
});

// ---------------------------------------------------------------------------
// §5.4 session tree
// ---------------------------------------------------------------------------

const wireSessions = [
  { id: 'ses_1', title: 'root work', time: { created: 100, updated: 500 } },
  { id: 'ses_2', forkParentID: 'ses_1', title: 'fork of root', time: { created: 200, updated: 900 } },
  { id: 'ses_3', forkParentID: 'ses_2', title: 'grandchild', time: { created: 300, updated: 300 } },
  { id: 'ses_4', forkParentID: 'ses_gone', title: 'orphan fork', time: { created: 400, updated: 400 } },
  // Subagent parentage is not fork lineage: a wire parentID session stays a
  // root in the fork tree.
  { id: 'ses_sub', parentID: 'ses_1', title: 'subagent child', time: { created: 350, updated: 350 } },
];

describe('buildSessionTree / buildSessionSubtree (spec 04 §5.4)', () => {
  test('projects registry fork metadata into the flat {leafId, nodes} shape', () => {
    const tree = buildSessionTree(wireSessions);
    expect(tree.leafId).toBe('ses_2'); // most recently updated
    expect(tree.nodes.map((n) => n.id)).toEqual(['ses_1', 'ses_2', 'ses_3', 'ses_sub', 'ses_4']);
    expect(tree.nodes[1]).toEqual({
      id: 'ses_2',
      parentId: 'ses_1',
      title: 'fork of root',
      time: { created: 200, updated: 900 },
    });
    expect(tree.nodes[4].parentId).toBeNull(); // fork parent not in the set
    expect(tree.nodes[3].parentId).toBeNull(); // subagent parentID is not lineage
    expect(tree.nodes.every((n) => !('sourcePath' in n))).toBe(true);
  });

  test('accepts { sessions } wrapping and empty input', () => {
    expect(buildSessionTree({ sessions: wireSessions }).leafId).toBe('ses_2');
    expect(buildSessionTree([])).toEqual({ leafId: null, nodes: [] });
    expect(buildSessionTree(undefined)).toEqual({ leafId: null, nodes: [] });
  });

  test('cycles in fork metadata are cut instead of hanging', () => {
    const cyclic = [
      { id: 'a', forkParentID: 'b', title: 'a', time: { created: 1, updated: 1 } },
      { id: 'b', forkParentID: 'a', title: 'b', time: { created: 2, updated: 2 } },
    ];
    const tree = buildSessionTree(cyclic);
    // exactly enough edges are cut that every parent walk terminates
    const nodeById = new Map(tree.nodes.map((n) => [n.id, n]));
    for (const node of tree.nodes) {
      const seen = new Set();
      let cursor = node;
      while (cursor?.parentId) {
        expect(seen.has(cursor.id)).toBe(false);
        seen.add(cursor.id);
        cursor = nodeById.get(cursor.parentId);
      }
    }
  });

  test('subtree returns the lineage + descendants of one session', () => {
    const subtree = buildSessionSubtree('ses_2', wireSessions);
    expect(subtree.nodes.map((n) => n.id).sort()).toEqual(['ses_1', 'ses_2', 'ses_3']);
    expect(subtree.leafId).toBe('ses_2');
    expect(buildSessionSubtree('ses_missing', wireSessions)).toBeNull();
  });
});

describe('buildEntryTreeSnapshot (spec 04 §5.4.1)', () => {
  const manager = {
    getTree: () => [
      {
        entry: { type: 'message', id: 'e_1', parentId: null, timestamp: 't1', message: { role: 'user', content: 'explore the repo' } },
        label: '探索阶段',
        children: [
          {
            entry: {
              type: 'message',
              id: 'e_2',
              parentId: 'e_1',
              timestamp: 't2',
              message: { role: 'assistant', content: [{ type: 'tool_call', name: 'read' }, { type: 'text', text: 'reading' }] },
            },
            children: [
              {
                entry: { type: 'branch_summary', id: 'e_4', parentId: 'e_2', timestamp: 't4', summary: 'abandoned exploration branch' },
                children: [],
              },
            ],
          },
          { entry: { type: 'label', id: 'e_3', parentId: 'e_1', timestamp: 't3', targetId: 'e_1', label: 'x' }, children: [] },
        ],
      },
    ],
    getEntries: () => [{ id: 'e_1' }, { id: 'e_2' }, { id: 'e_3' }, { id: 'e_4' }],
    getLeafId: () => 'e_4',
  };

  test('flattens entries, skips label nodes, folds resolved labels + gists', () => {
    const snapshot = buildEntryTreeSnapshot({ sessionID: 'ses_1', directory: DIRECTORY, manager });
    expect(snapshot.sessionID).toBe('ses_1');
    expect(snapshot.leafId).toBe('e_4');
    expect(snapshot.revision).toBe(4);
    expect(snapshot.nodes.map((n) => n.id)).toEqual(['e_1', 'e_2', 'e_4']);
    expect(snapshot.nodes.find((n) => n.id === 'e_3')).toBeUndefined();
    expect(snapshot.nodes[0].label).toBe('探索阶段');
    expect(snapshot.nodes[0].gist).toEqual({ role: 'user', preview: 'explore the repo' });
    expect(snapshot.nodes[1].gist.toolName).toBe('read');
    expect(snapshot.nodes[2].gist.preview).toContain('abandoned exploration');
    expect(snapshot.pathToLeaf).toEqual(['e_1', 'e_2', 'e_4']);
  });
});

describe('navigate/label contracts (spec 04 §5.4.2/§5.4.3, engine hooks)', () => {
  test('navigate request defaults and validation', () => {
    const ok = normalizeNavigateRequest({ targetId: 'e_9' });
    expect(ok.ok).toBe(true);
    expect(ok.value).toEqual({
      targetId: 'e_9',
      summarize: false,
      customInstructions: null,
      allowAskReopen: true,
      reanswerAskResult: null,
    });
    expect(normalizeNavigateRequest({}).ok).toBe(false);
    expect(normalizeNavigateRequest({ targetId: 'e_9', reanswerAskResult: { nope: true } }).response.status).toBe(400);
    const twoPhase = normalizeNavigateRequest({
      targetId: 'e_9',
      reanswerAskResult: { content: [{ type: 'text', text: 'yes' }], details: {}, isError: false },
    });
    expect(twoPhase.ok).toBe(true);
  });

  test('tree update payload contract: navigate|label|summary delta only', () => {
    expect(() => treeUpdatedPayload({ kind: 'wat' })).toThrow(TypeError);
    expect(treeUpdatedPayload({ leafId: 'e_4', kind: 'summary', entryId: 'e_9' })).toEqual({
      leafId: 'e_4',
      kind: 'summary',
      entryId: 'e_9',
    });
    expect(treeUpdatedPayload({ kind: 'label' })).toEqual({ leafId: null, kind: 'label' });
  });

  test('label request: undefined clears, non-string rejected', () => {
    expect(normalizeLabelRequest({ targetId: 'e_1' }).value).toEqual({ targetId: 'e_1', label: undefined });
    expect(normalizeLabelRequest({ targetId: 'e_1', label: 'phase' }).value.label).toBe('phase');
    expect(normalizeLabelRequest({ targetId: 'e_1', label: 5 }).ok).toBe(false);
    expect(normalizeLabelRequest({}).ok).toBe(false);
  });

  test('streaming guard is 409 {busy:true} (D04-4)', async () => {
    const response = navigateBusyResponse();
    expect(response.status).toBe(409);
    expect(await response.json()).toEqual({ busy: true });
  });
});

// ---------------------------------------------------------------------------
// §5.5 agent runs aggregation + parked/historical split
// ---------------------------------------------------------------------------

const ref = (overrides = {}) => ({
  id: 'Anna',
  displayName: 'Anna',
  kind: 'sub',
  parentId: 'Main',
  status: 'running',
  session: {},
  sessionFile: 'C:/agentdir/sessions/x/Anna.jsonl',
  createdAt: 1,
  lastActivity: 10,
  activity: 'reading engine.js',
  history: { modelRole: 'default', resolvedModel: 'glm-4.7', readOnly: false, outputPath: 'agent://Anna', patchPath: 'C:/leak/patch.diff' },
  ...overrides,
});

const makeRegistry = (...refs) => ({ list: () => refs });

describe('projectAgentRun (spec 04 §5.5.1, R7)', () => {
  test('row carries the two-part key and NO absolute paths', () => {
    const row = projectAgentRun({ sessionID: 'ses_1', directory: DIRECTORY, ref: ref() });
    expect(row.key).toBe('ses_1::Anna');
    expect(row.hasTranscript).toBe(true);
    expect(row.history.outputPath).toBe('agent://Anna');
    expect('sessionFile' in row).toBe(false);
    expect(row.history.patchPath).toBeUndefined();
    const serialized = JSON.stringify(row);
    expect(serialized).not.toContain('.jsonl');
    expect(serialized).not.toContain('C:/leak');
  });
});

describe('AgentRunsAggregator (spec 04 §5.5.1)', () => {
  /** Manual timer capture: coalescing is asserted deterministically, never
   * via wall-clock sleeps (the real timer path is exercised by the domain
   * integration test). */
  const manualTimers = () => {
    const state = { fn: null, ms: null, cleared: 0 };
    return {
      setTimeout: (fn, ms) => {
        state.fn = fn;
        state.ms = ms;
        return 1;
      },
      clearTimeout: () => {
        state.cleared += 1;
      },
      fire: () => {
        const fn = state.fn;
        state.fn = null;
        fn?.();
      },
      state,
    };
  };

  test('keys rows by sessionID::agentId — same flat id across sessions stays distinct', () => {
    const aggregator = new AgentRunsAggregator({
      snapshot: () => [
        { sessionID: 'ses_A', directory: DIRECTORY, registry: makeRegistry(ref()) },
        { sessionID: 'ses_B', directory: 'C:/proj/beta', registry: makeRegistry(ref({ lastActivity: 99 })) },
      ],
      publish: () => {},
    });
    const { agentRuns } = aggregator.refresh();
    expect(agentRuns.map((r) => r.key).sort()).toEqual(['ses_A::Anna', 'ses_B::Anna']);
    aggregator.dispose();
  });

  test('coalesces bursts into one omp.agents.updated per directory with monotonic revision', () => {
    const events = [];
    let registry = makeRegistry(ref());
    const timers = manualTimers();
    const aggregator = new AgentRunsAggregator({
      snapshot: () => [{ sessionID: 'ses_A', directory: DIRECTORY, registry }],
      publish: (type, payload, scope) => events.push({ type, payload, scope }),
      coalesceMs: 250,
      ...timers,
    });
    aggregator.refresh();
    aggregator.notify(); // registry burst
    aggregator.notify();
    expect(events).toEqual([]); // nothing published inside the window
    expect(timers.state.ms).toBe(250);
    timers.fire();
    expect(events.length).toBe(1);
    expect(events[0].type).toBe(OMP_AGENTS_UPDATED);
    expect(events[0].scope).toEqual({ directory: DIRECTORY, durable: true });
    expect(events[0].payload.revision).toBe(1);
    expect(events[0].payload.agentRuns.map((r) => r.key)).toEqual(['ses_A::Anna']);

    registry = makeRegistry(ref({ status: 'idle', activity: undefined }));
    aggregator.refresh();
    aggregator.flush(); // engine-style immediate flush
    expect(events.length).toBe(2);
    expect(events[1].payload.revision).toBe(2);
    expect(events[1].payload.agentRuns[0].status).toBe('idle');
    aggregator.dispose();
    expect(timers.state.cleared).toBeGreaterThan(0);
  });

  test('historical disk rows merge in; registry rows override same key (R2-M5)', () => {
    const aggregator = new AgentRunsAggregator({
      snapshot: () => [{ sessionID: 'ses_A', directory: DIRECTORY, registry: makeRegistry(ref()) }],
      diskScan: (directory) => [
        { sessionID: 'ses_C', agentId: 'Ghost', directory },
        { sessionID: 'ses_A', agentId: 'Anna', directory, hasTranscript: true }, // shadowed by live row
      ],
      publish: () => {},
    });
    const { agentRuns } = aggregator.refresh();
    const ghost = agentRuns.find((r) => r.agentId === 'Ghost');
    expect(ghost.status).toBe('historical');
    expect(ghost.key).toBe('ses_C::Ghost');
    const live = agentRuns.find((r) => r.key === 'ses_A::Anna');
    expect(live.status).toBe('running'); // registry wins, never historical
    aggregator.dispose();
  });

  test('directory filter + emptied-directory publishes a full-replace empty snapshot', () => {
    const events = [];
    let snapshot = () => [{ sessionID: 'ses_A', directory: DIRECTORY, registry: makeRegistry(ref()) }];
    const aggregator = new AgentRunsAggregator({
      snapshot: () => snapshot(),
      publish: (type, payload, scope) => events.push({ type, payload, scope }),
    });
    aggregator.refresh();
    aggregator.flush();
    const listed = aggregator.snapshot(DIRECTORY);
    expect(listed.agentRuns.every((r) => r.directory === DIRECTORY)).toBe(true);
    expect(aggregator.snapshot('C:/unrelated').agentRuns).toEqual([]);

    // everything goes away → empty full-replace snapshot still publishes
    snapshot = () => [];
    aggregator.refresh();
    aggregator.flush();
    const last = events[events.length - 1];
    expect(last.scope.directory).toBe(DIRECTORY);
    expect(last.payload.agentRuns).toEqual([]);
    aggregator.dispose();
  });

  test('sorts running > idle > parked > aborted > historical', () => {
    const byStatus = { h: 'historical', p: 'parked', r: 'running', a: 'aborted', i: 'idle' };
    const aggregator = new AgentRunsAggregator({
      snapshot: () => [
        {
          sessionID: 'ses_A',
          directory: DIRECTORY,
          registry: makeRegistry(
            ...Object.entries(byStatus).map(([id, status]) =>
              ref({ id, displayName: id, status, parentId: undefined, lastActivity: 9 }),
            ),
          ),
        },
      ],
      publish: () => {},
    });
    aggregator.refresh();
    const order = aggregator.snapshot(DIRECTORY).agentRuns.map((r) => r.agentId);
    expect(order).toEqual(['r', 'i', 'p', 'a', 'h']);
    aggregator.dispose();
  });
});


/** Asserted fields of agent-run action JSON responses. */
interface ActionResponseBody {
  ok?: boolean;
  status?: string;
  error?: string;
  revivable?: boolean;
}


describe('agent-run actions: parked vs historical (spec 04 §5.5.2, R2-M5)', () => {
  const setup = (rows) => {
    const aggregator = new AgentRunsAggregator({
      snapshot: () => [
        {
          sessionID: 'ses_A',
          directory: DIRECTORY,
          registry: makeRegistry(...rows.map((r) => ref(r))),
        },
      ],
      publish: () => {},
      coalesceMs: 1,
    });
    aggregator.refresh();
    const descriptors = new ParkedAgentDescriptors();
    const calls = { revive: 0, kill: 0, chat: 0 };
    const actions = {
      revive: async () => { calls.revive += 1; },
      kill: async () => { calls.kill += 1; },
      chat: async () => { calls.chat += 1; },
    };
    return { aggregator, descriptors, actions, calls };
  };
  const act = (ctx, agentId, body) =>
    handleAgentRunAction({
      aggregator: ctx.aggregator,
      descriptors: ctx.descriptors,
      actions: ctx.actions,
      sessionID: 'ses_A',
      agentId,
      directory: DIRECTORY,
      body,
    }).then((r) =>
      // SAFETY: the action endpoint answers the ActionResponseBody wire shape.
      r.json().then((data) => ({ status: r.status, body: data as ActionResponseBody })),
    );

  test('historical rows refuse revive/kill/chat with 409 {historical, revivable:false}', async () => {
    const ctx = setup([{ id: 'Ghost', status: 'parked' }]);
    ctx.aggregator['#rows']; // no-op touch
    // force historical status via disk-scan style row injection
    const historicalCtx = {
      aggregator: {
        row: (sessionID, agentId) =>
          projectAgentRun({
            sessionID,
            directory: DIRECTORY,
            ref: ref({ id: agentId }),
            status: 'historical',
          }),
      },
      descriptors: ctx.descriptors,
    };
    for (const kind of ['revive', 'kill', { kind: 'chat', text: 'hi' }]) {
      const result = await handleAgentRunAction({
        aggregator: historicalCtx.aggregator,
        descriptors: ctx.descriptors,
        body: typeof kind === 'string' ? { kind } : kind,
      }).then((r) =>
        // SAFETY: the agent-run action endpoint answers the ResolveResponseBody wire shape.
        r.json().then((data) => ({ status: r.status, body: data as ResolveResponseBody })),
      );
      expect(result.status).toBe(409);
      expect(result.body).toEqual({ error: 'historical', revivable: false });
    }
    expect(ctx.calls.revive).toBe(0);
    expect(ctx.calls.kill).toBe(0);
    expect(ctx.calls.chat).toBe(0);
  });

  test('revive works only for in-process parked rows via descriptor claim', async () => {
    const ctx = setup([
      { id: 'Runner', status: 'running' },
      { id: 'Parked', status: 'parked', session: null },
    ]);
    const onRunning = await act(ctx, 'Runner', { kind: 'revive' });
    expect(onRunning.status).toBe(409);
    expect(onRunning.body.error).toBe('not-parked');

    const noDescriptor = await act(ctx, 'Parked', { kind: 'revive' });
    expect(noDescriptor.status).toBe(409);
    expect(noDescriptor.body).toEqual({ error: 'reviver-unavailable', revivable: false });

    ctx.descriptors.register({
      sessionID: 'ses_A',
      agentId: 'Parked',
      ref: ref({ id: 'Parked', status: 'parked' }),
      revive: async () => ({}),
    });
    const revived = await act(ctx, 'Parked', { kind: 'revive' });
    expect(revived.status).toBe(200);
    expect(revived.body).toEqual({ ok: true, status: 'running' });
    expect(ctx.calls.revive).toBe(1);
    expect(ctx.descriptors.has('ses_A', 'Parked')).toBe(false); // single claim
    const second = await act(ctx, 'Parked', { kind: 'revive' });
    expect(second.status).toBe(409);
  });

  test('kill works on live rows; chat on parked revives first; validation', async () => {
    const ctx = setup([
      { id: 'Idle', status: 'idle', session: null },
      { id: 'Parked', status: 'parked', session: null },
    ]);
    const killed = await act(ctx, 'Idle', { kind: 'kill' });
    expect(killed.body).toEqual({ ok: true, status: 'aborted' });
    expect(ctx.calls.kill).toBe(1);

    ctx.descriptors.register({ sessionID: 'ses_A', agentId: 'Parked', revive: async () => ({}) });
    const chatted = await act(ctx, 'Parked', { kind: 'chat', text: 'status?', mode: 'steer' });
    expect(chatted.body).toEqual({ ok: true, status: 'running' });
    expect(ctx.calls.revive).toBe(1);
    expect(ctx.calls.chat).toBe(1);

    expect((await act(ctx, 'Idle', { kind: 'chat', text: '' })).status).toBe(400);
    expect((await act(ctx, 'Idle', { kind: 'chat', text: 'x', mode: 'wat' })).status).toBe(400);
    expect((await act(ctx, 'Idle', { kind: 'dance' })).status).toBe(400);
    expect((await act(ctx, 'Nobody', { kind: 'kill' })).status).toBe(404);
    expect(
      (
        await handleAgentRunAction({
          aggregator: ctx.aggregator,
          descriptors: ctx.descriptors,
          actions: ctx.actions,
          sessionID: 'ses_A',
          agentId: 'Idle',
          directory: 'C:/elsewhere',
          body: { kind: 'kill' },
        })
      ).status,
    ).toBe(404); // wrong directory scope
  });
});

// ---------------------------------------------------------------------------
// §5.6 jobs (master R12)
// ---------------------------------------------------------------------------

/** Asserted fields of jobs endpoint JSON responses. */
interface JobsResponseBody {
  ownerSessionID?: string | null;
  delivery?: unknown;
  error?: string;
}

describe('jobs endpoint (spec 04 §5.6, R12)', () => {
  test('capability off → structured 501 with ownerSessionID, never 404', async () => {
    const response = await handleJobsRequest({ liveSessionIds: ['ses_first', 'ses_second'] });
    expect(response.status).toBe(501);
    expect(await response.json()).toEqual({
      error: 'jobs-unavailable',
      reason: JOBS_UNAVAILABLE_REASON,
      ownerSessionID: 'ses_first',
    });
    const empty = await handleJobsRequest({ liveSessionIds: [] });
    expect(empty.status).toBe(501);
    // SAFETY: the 501 body carries ownerSessionID (handleJobsRequest contract).
    expect(((await empty.json()) as JobsResponseBody).ownerSessionID).toBeNull();
  });

  test('capability on delegates to the snapshot hook with owner echo', async () => {
    const response = await handleJobsRequest({
      liveSessionIds: ['ses_first'],
      jobsEnabled: true,
      recentLimit: 3,
      snapshot: async (ownerSessionID, recentLimit) => {
        expect(ownerSessionID).toBe('ses_first');
        expect(recentLimit).toBe(3);
        return { running: [], recent: [], delivery: { queued: 0, delivering: false } };
      },
    });
    expect(response.status).toBe(200);
    // SAFETY: jobs answers JobsResponseBody (ownerSessionID echo + snapshot).
    const body = (await response.json()) as JobsResponseBody;
    expect(body.ownerSessionID).toBe('ses_first');
    expect(body.delivery).toEqual({ queued: 0, delivering: false });
  });
});

// ---------------------------------------------------------------------------
// domain assembly + mount
// ---------------------------------------------------------------------------

describe('createUriDomain + mount (integration surface)', () => {
  const registerRoutes = (domain) => {
    const routes = new Map();
    domain.mount((method, pattern, handler) => routes.set(`${method} ${pattern}`, handler));
    return routes;
  };
  const fakeRequest = (url: string, body?: UriRequestBody) => ({
    url,
    json: async () => body,
  });
  const ctxFor = (url, params = {}) => ({ params, url: new URL(url), headers: new Headers() });

  test('features off → explicit 501s per key (fail loudly, R2)', async () => {
    const domain = createUriDomain({ tokens, localOptionsFor, features: () => ({ 'uri.v1': false, 'tree.v1': false, 'agentRuns.v1': false, 'jobs.v1': false }) });
    const routes = registerRoutes(domain);
    const resolve = await routes.get('POST /omp/uri/resolve')(fakeRequest('http://x/omp/uri/resolve', { scheme: 'local', ref: 'scratch.md', sessionID: 'ses_A', directory: DIRECTORY }));
    expect(resolve.status).toBe(501);
    expect(await resolve.json()).toEqual({ error: 'uri.v1-unavailable' });
    const tree = await routes.get('GET /omp/sessions/{sessionID}/tree')(
      fakeRequest(`http://x/omp/sessions/ses_1/tree?directory=${encodeURIComponent(DIRECTORY)}`),
      ctxFor(`http://x/omp/sessions/ses_1/tree?directory=${encodeURIComponent(DIRECTORY)}`, { sessionID: 'ses_1' }),
    );
    expect(tree.status).toBe(501);
    expect(await tree.json()).toEqual({ error: 'tree.v1-unavailable' });
    const runs = await routes.get('GET /omp/agent-runs')(fakeRequest('http://x/omp/agent-runs?directory=C:/p'), ctxFor('http://x/omp/agent-runs?directory=C:/p'));
    expect(runs.status).toBe(501);
    // jobs is ALWAYS mounted — 501 is its steady state, not a gate (R12)
    const jobs = await routes.get('GET /omp/jobs')(fakeRequest('http://x/omp/jobs'), ctxFor('http://x/omp/jobs'));
    expect(jobs.status).toBe(501);
    expect((await jobs.json()).error).toBe('jobs-unavailable');
    domain.dispose();
  });

  test('features on → full route flow against real SDK router + registry data', async () => {
    const published = [];
    const domain = createUriDomain({
      features: () => ({ ...ompFeatures(), 'uri.v1': true, 'tree.v1': true, 'agentRuns.v1': true }),
      tokens,
      localOptionsFor,
      sessionTreeData: async () => wireSessions,
      agentsSnapshot: () => [
        { sessionID: 'ses_A', directory: DIRECTORY, registry: makeRegistry(ref()) },
      ],
      publish: (type, payload, scope) => published.push({ type, payload, scope }),
      liveSessionIds: () => ['ses_A'],
    });
    const routes = registerRoutes(domain);
    domain.aggregator.refresh();
    await new Promise((resolve) => setTimeout(resolve, 300)); // > default 250ms coalesce

    const resolve = await routes.get('POST /omp/uri/resolve')(
      fakeRequest('http://x/omp/uri/resolve', { scheme: 'local', ref: 'scratch.md', sessionID: 'ses_A', directory: DIRECTORY }),
    );
    expect(resolve.status).toBe(200);
    const resource = await resolve.json();
    expect(resource.content).toBe('alpha session secret');
    expect('sourcePath' in resource).toBe(false);

    const opened = await routes.get('POST /omp/uri/open')(
      fakeRequest('http://x/omp/uri/open', { token: resource.token.id, directory: DIRECTORY }),
      ctxFor('http://x/omp/uri/open'),
    );
    expect(opened.status).toBe(200);

    const treeUrl = `http://x/omp/sessions/ses_2/tree?directory=${encodeURIComponent(DIRECTORY)}`;
    const tree = await routes.get('GET /omp/sessions/{sessionID}/tree')(fakeRequest(treeUrl), ctxFor(treeUrl, { sessionID: 'ses_2' }));
    expect(tree.status).toBe(200);
    const treeBody = await tree.json();
    expect(treeBody.leafId).toBe('ses_2');
    expect(treeBody.nodes.map((n) => n.id).sort()).toEqual(['ses_1', 'ses_2', 'ses_3']);
    const missingTree = await routes.get('GET /omp/sessions/{sessionID}/tree')(
      fakeRequest(treeUrl),
      ctxFor(treeUrl, { sessionID: 'nope' }),
    );
    expect(missingTree.status).toBe(404);

    const runs = await routes.get('GET /omp/agent-runs')(
      fakeRequest(`http://x/omp/agent-runs?directory=${encodeURIComponent(DIRECTORY)}`),
      ctxFor(`http://x/omp/agent-runs?directory=${encodeURIComponent(DIRECTORY)}`),
    );
    const runsBody = await runs.json();
    expect(runsBody.agentRuns.map((r) => r.key)).toEqual(['ses_A::Anna']);
    expect(runsBody.revision).toBeGreaterThan(0);

    const action = await routes.get('POST /omp/agent-runs/{sessionID}/{agentId}')(
      fakeRequest('http://x/omp/agent-runs/ses_A/Anna', { kind: 'kill', directory: DIRECTORY }),
      ctxFor('http://x/omp/agent-runs/ses_A/Anna', { sessionID: 'ses_A', agentId: 'Anna' }),
    );
    expect(action.status).toBe(500);
    expect((await action.json()).error).toBe('hook-unavailable');

    expect(published.some((e) => e.type === OMP_AGENTS_UPDATED && e.scope.directory === DIRECTORY)).toBe(true);
    domain.dispose();
  });
});


describe('artifacts browse (spec 04 — host-level read-only local:// listing)', () => {
  const registerRoutes = (domain) => {
    const routes = new Map();
    domain.mount((method, pattern, handler) => routes.set(`${method} ${pattern}`, handler));
    return routes;
  };
  const ctxFor = (url, params = {}) => ({ params, url: new URL(url), headers: new Headers() });

  const localFilesOf = {
    ses_A: {
      files: [
        { ref: 'PLAN.md', size: 10, modifiedAt: 5 },
        { ref: 'scratch/notes.md', size: 20, modifiedAt: 9 },
      ],
      truncated: false,
    },
    // Session with no local:// root yet — authoritative empty, still indexed.
    ses_B: { files: [], truncated: false },
  };
  const localFiles = async (sessionID) => localFilesOf[sessionID] ?? null;
  const sessionTreeData = async () => [
    { id: 'ses_A', title: 'Alpha', time: { created: 1, updated: 10 } },
    { id: 'ses_B', title: '', time: { created: 2, updated: 20 } },
  ];

  test('per-session only: missing sessionID → 400 session-required (no index form)', async () => {
    const domain = createUriDomain({
      features: () => ({ ...ompFeatures(), artifacts: true }),
      localFiles,
      sessionTreeData,
    });
    const res = await domain.artifacts.list({ directory: DIRECTORY });
    expect(res.status).toBe(400);
    expect(await res.json()).toEqual({ error: 'session-required' });
    domain.dispose();
  });

  test('session form: rows mtime desc; unknown session → 404; missing directory → 400', async () => {
    const domain = createUriDomain({
      features: () => ({ ...ompFeatures(), artifacts: true }),
      localFiles,
      sessionTreeData,
    });
    const files = await domain.artifacts.list({ directory: DIRECTORY, sessionID: 'ses_A' });
    const filesBody =
      // SAFETY: the artifacts endpoint answers {files, truncated}.
      (await files.json()) as { files: Array<{ ref: string; size?: number }>; truncated?: boolean };
    expect(files.status).toBe(200);
    expect(filesBody.files.map((file) => file.ref)).toEqual(['scratch/notes.md', 'PLAN.md']);
    expect(filesBody.truncated).toBe(false);

    const unknown = await domain.artifacts.list({ directory: DIRECTORY, sessionID: 'ses_X' });
    expect(unknown.status).toBe(404);
    expect(await unknown.json()).toEqual({ error: 'session-not-found' });

    const missing = await domain.artifacts.list({ directory: null });
    expect(missing.status).toBe(400);
    expect(await missing.json()).toEqual({ error: 'directory-required' });
    domain.dispose();
  });

  test('truncated flag survives the composed handler; malformed rows are dropped', async () => {
    const domain = createUriDomain({
      features: () => ({ ...ompFeatures(), artifacts: true }),
      localFiles: async () => ({
        files: [
          { ref: 'a.md', size: 1, modifiedAt: 2 },
          { ref: '', size: 1, modifiedAt: 3 }, // malformed — dropped, not fatal
          { ref: 'b.md', size: 1, modifiedAt: 4, extra: 'ignored' },
        ],
        truncated: true,
      }),
      sessionTreeData: async () => [{ id: 'ses_A', title: 'Alpha', time: { created: 1, updated: 1 } }],
    });
    const res = await domain.artifacts.list({ directory: DIRECTORY, sessionID: 'ses_A' });
    // SAFETY: the artifacts endpoint answers {files, truncated}.
    const body = (await res.json()) as { files: Array<{ ref: string; size?: number; modifiedAt?: number }>; truncated?: boolean };
    expect(body.files).toEqual([
      { ref: 'b.md', size: 1, modifiedAt: 4 },
      { ref: 'a.md', size: 1, modifiedAt: 2 },
    ]);
    expect(body.truncated).toBe(true);
    domain.dispose();
  });

  test('mounted route: capability off → 501 artifacts-unavailable; hook missing → 500; happy path via route', async () => {
    const off = createUriDomain({
      features: () => ({ ...ompFeatures(), artifacts: false }),
      localFiles,
      sessionTreeData,
    });
    const routes = registerRoutes(off);
    const blocked = await routes.get('GET /omp/artifacts')(
      { url: `http://x/omp/artifacts?directory=${encodeURIComponent(DIRECTORY)}` },
      ctxFor(`http://x/omp/artifacts?directory=${encodeURIComponent(DIRECTORY)}`),
    );
    expect(blocked.status).toBe(501);
    expect(await blocked.json()).toEqual({ error: 'artifacts-unavailable' });
    off.dispose();

    const noHook = createUriDomain({
      features: () => ({ ...ompFeatures(), artifacts: true }),
    });
    const noHookRoutes = registerRoutes(noHook);
    const hookless = await noHookRoutes.get('GET /omp/artifacts')(
      { url: `http://x/omp/artifacts?directory=${encodeURIComponent(DIRECTORY)}&sessionID=ses_A` },
      ctxFor(`http://x/omp/artifacts?directory=${encodeURIComponent(DIRECTORY)}&sessionID=ses_A`),
    );
    expect(hookless.status).toBe(500);
    expect(await hookless.json()).toEqual({ error: 'hook-unavailable', hook: 'localFiles' });
    noHook.dispose();

    const on = createUriDomain({
      features: () => ({ ...ompFeatures(), artifacts: true }),
      localFiles,
      sessionTreeData,
    });
    const onRoutes = registerRoutes(on);
    const ok = await onRoutes.get('GET /omp/artifacts')(
      { url: `http://x/omp/artifacts?directory=${encodeURIComponent(DIRECTORY)}&sessionID=ses_A` },
      ctxFor(`http://x/omp/artifacts?directory=${encodeURIComponent(DIRECTORY)}&sessionID=ses_A`),
    );
    expect(ok.status).toBe(200);
    expect((await ok.json()).files).toHaveLength(2);
    on.dispose();
  });
});

describe('local:// binary preview (spec 04 §5.2.4 — token byte stream)', () => {
  const PNG_BYTES = Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x01]);
  fs.writeFileSync(path.join(artifactsOf('ses_A'), 'local', 'shot.png'), PNG_BYTES);

  test('image resolve answers a binary descriptor: mime + token, no placeholder content', async () => {
    const { status, body } = await resolveBody({
      scheme: 'local',
      ref: 'shot.png',
      sessionID: 'ses_A',
      directory: DIRECTORY,
    });
    expect(status).toBe(200);
    expect(body.binary).toBe(true);
    expect(body.contentType).toBe('image/png');
    expect(body.immutable).toBe(true);
    expect(body.size).toBe(PNG_BYTES.byteLength);
    expect(body.content).toBeUndefined();
    expect('sourcePath' in body).toBe(false);
  });

  test('uri.content streams the raw bytes with the mime and honors token scope', async () => {
    const domain = createUriDomain({
      features: () => ({ ...ompFeatures(), 'uri.v1': true }),
      localOptionsFor,
      tokens,
    });
    const { body } = await resolveBody({
      scheme: 'local',
      ref: 'shot.png',
      sessionID: 'ses_A',
      directory: DIRECTORY,
    });
    const res = await domain.uri.content({ id: body.token.id, directory: DIRECTORY });
    expect(res.status).toBe(200);
    expect(res.headers.get('content-type')).toBe('image/png');
    expect(res.headers.get('x-content-type-options')).toBe('nosniff');
    const bytes = Buffer.from(await res.arrayBuffer());
    expect(bytes.subarray(0, 8).equals(PNG_BYTES.subarray(0, 8))).toBe(true);

    const scoped = await domain.uri.content({ id: body.token.id, directory: 'C:/other' });
    expect(scoped.status).toBe(403);
    domain.dispose();
  });
});