// Chapter-04 domain tests (spec 04 §5.2/§5.4/§5.5/§5.6, master R2-H2/R7/R8/R12,
// R2-M5). The local:// suite exercises the REAL SDK router + handler
// (containment, session pinning, traversal rejection are the SDK's own —
// we only pin options per request), so the isolation guarantees here are
// end-to-end, not mocks of them.
/**
 * domain-uri 域的测试套件（spec 04）：覆盖 URI 能力矩阵、local:// 解析与
 * resource token、会话树/条目树投影、navigate/label 契约、agent-runs 聚合
 * 与动作、jobs 端点、artifacts 浏览以及二进制预览。local:// 相关用例直接
 * 驱动真实 SDK 的 router + handler（本套件只按请求钉扎 options），因此目录
 * 遏制、会话钉扎、穿越拒绝等隔离保证是端到端验证的，而非 mock。
 */

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
import { normalizeDirectoryKey } from './registry.ts';
import type { UriRequestBody, UriResolveBody } from './domain-uri.ts';

/** 测试统一使用的项目目录常量：作为 registry 键与会话钉扎参数，无需真实存在。 */
const DIRECTORY = 'C:/proj/alpha';

/** Row keys are composite directory\0session::agent (plan §3.1) — opaque to
 *  consumers; tests reconstruct them to pin the format. */
/** 按 plan §3.1 的行键格式拼接 directory\0session::agent，供断言时与被测代码对键。 */
const runKey = (directory: string, sessionID: string, agentId: string) =>
  `${normalizeDirectoryKey(directory)}\u0000${sessionID}::${agentId}`;

// ---------------------------------------------------------------------------
// shared fixtures
// ---------------------------------------------------------------------------

/** 全套件共享的临时根目录（mkdtemp 生成），afterAll 统一递归清理。 */
const base = fs.mkdtempSync(path.join(os.tmpdir(), 'oc-domain-uri-'));
/** 返回会话在临时根下的 artifacts 目录路径，即该会话 local:// 协议的根。 */
const artifactsOf = (sessionId: string) => path.join(base, sessionId);
/** 在指定会话的 local:// 根下写入相对路径文件（自动创建父目录），返回绝对路径。 */
const writeLocal = (sessionId: string, file: string, content: string) => {
  const target = path.join(artifactsOf(sessionId), 'local', file);
  fs.mkdirSync(path.dirname(target), { recursive: true });
  fs.writeFileSync(target, content);
  return target;
};
// 预置 ses_A / ses_B 两个会话的 local:// 夹具；notes/deep.md 仅存在于 ses_A，用于验证跨会话隔离。
writeLocal('ses_A', 'scratch.md', 'alpha session secret');
writeLocal('ses_A', 'notes/deep.md', 'nested note');
writeLocal('ses_B', 'scratch.md', 'beta session secret');

/** 会话目录存在则为其创建 local:// 协议 options（真实 SDK handler 据此钉扎会话与根目录），否则返回 null。 */
const localOptionsFor = (sessionID: string, directory: string) => {
  if (!fs.existsSync(artifactsOf(sessionID))) return null;
  return createLocalProtocolOptions(sessionID, directory, artifactsOf(sessionID));
};
/** Asserted fields of the JSON bodies the resolve/open/info endpoints return
 * (superset per response; unasserted fields stay untyped). */
/** resolve/open/info 端点 JSON 响应中被断言的字段集合；各响应只取其子集，未断言字段不作类型约束。 */
interface ResolveResponseBody {
  /** 文件文本内容（binary 响应不携带）。 */
  content?: string;
  /** 规范化后的资源 URI（如 local://scratch.md）。 */
  url?: string;
  /** MIME 类型；文本为 text/markdown，二进制按扩展名推断。 */
  contentType?: string;
  /** open 响应返回的文件名（仅 basename，不含路径）。 */
  filename?: string;
  /** 该资源是否可编辑。 */
  editable?: boolean;
  /** 未知/未启用 scheme 错误响应里回显的 scheme 名。 */
  scheme?: string;
  /** 机器可读错误码（resolve-failed、scheme-not-enabled 等）。 */
  error?: string;
  /** 人类可读的错误说明。 */
  message?: string;
  /** Binary descriptor arm (previewable images): bytes stream via the token
   * content endpoint instead of an inline body. */
  /** 二进制描述分支（可预览图片）：内容改经 token 的 content 端点按字节流返回。 */
  binary?: boolean;
  /** 资源内容是否不可变（二进制资源为 true）。 */
  immutable?: boolean;
  /** 二进制资源的字节大小。 */
  size?: number;
  /** 动作错误响应附带：该行是否可 revive。 */
  revivable?: boolean;
  /** resolve 铸出的不透明 resource token（id + 过期时间）。 */
  token?: { id: string; expiresAt: number };
}

/** 跨用例共享的 UriTokenService 实例（默认 TTL 与读取上限），模拟进程内 token 服务。 */
const tokens = new UriTokenService();
/** 直调 handleUriResolve 并把 Response 解析为 { status, body } 的通用封装。 */
const resolveBody = (body: UriResolveBody): Promise<{ status: number; body: ResolveResponseBody }> =>
  handleUriResolve({ body, localOptionsFor, tokens }).then((r) =>
    // SAFETY: the resolve/open endpoints answer the ResolveResponseBody wire
    // shape this helper exists to assert; json() only loses that type.
    r.json().then((data) => ({ status: r.status, body: data as ResolveResponseBody })),
  );

// 递归清理临时根目录（重试 5 次以容忍文件系统句柄延迟释放）。
afterAll(() => {
  fs.rmSync(base, { recursive: true, force: true, maxRetries: 5 });
});

// ---------------------------------------------------------------------------
// §5.2 capability matrix + options factory
// ---------------------------------------------------------------------------

/** §5.2 能力矩阵与 local:// options 工厂：local 只读、无 router 写入、会话/artifacts 目录钉扎与 sessionId 必填校验。 */
describe('uriCapabilities + createLocalProtocolOptions (spec 04 §5.2, R7/R8)', () => {
  test('P1 matrix is local:// read only, no router-mediated writes', () => {
    expect(uriCapabilities()).toEqual({ read: ['local'], write: [] });
  });

  test('options pin sessionId and resolve artifactsDir from constant or provider', () => {
    const fromConstant = createLocalProtocolOptions('ses_A', DIRECTORY, 'X:/artifacts/A');
    expect(fromConstant.getSessionId?.()).toBe('ses_A');
    expect(fromConstant.getArtifactsDir?.()).toBe('X:/artifacts/A');
    const fromProvider = createLocalProtocolOptions('ses_B', DIRECTORY, (sid) =>
      sid === 'ses_B' ? 'X:/artifacts/B' : null,
    );
    expect(fromProvider.getArtifactsDir?.()).toBe('X:/artifacts/B');
    // SAFETY: the provider overload answers undefined → null artifacts dir.
    const nullProvider = createLocalProtocolOptions('ses_C', DIRECTORY, () => undefined);
    expect(nullProvider.getArtifactsDir?.()).toBeNull();
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
    // SAFETY: degenerate input — a missing session file yields no artifacts dir.
    const noFile: string | { path: string } = '';
    expect(artifactsDirForSessionFile(noFile)).toBeNull();
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

/** §5.2.1 resolve 端点：自有文件解析与 token 铸造、同目录跨会话隔离、穿越拒绝、scheme 错误分级与参数钉扎。 */
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
    expect(body.token?.id).toMatch(/^ocuri_[A-Za-z0-9_-]{43}$/);
    expect(typeof body.token?.expiresAt).toBe('number');
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

/** §5.2.4 resource token：open 只回内容与文件名、info 不消耗读取次数、目录 scope 校验与伪造/过期/耗尽 token 的 404。 */
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
    const opened = await domain.uri.open({ body: { token: resource.token?.id ?? '' }, directory: DIRECTORY });
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
    const params = new URLSearchParams({ token: resource.token?.id ?? '' });
    // SAFETY: info answers the metadata subset of ResolveResponseBody.
    const info = (await domain.uri.info({ query: params, directory: DIRECTORY }).json()) as ResolveResponseBody;
    expect(info.url).toBe('local://scratch.md');
    expect(info.filename).toBe('scratch.md');
    expect(info.editable).toBe(true);
    expect('content' in info).toBe(false);
    expect(JSON.stringify(info)).not.toContain(base.replaceAll('\\', '/'));
    // still fully redeemable afterwards
    const opened = await domain.uri.open({ body: { token: resource.token?.id ?? '' }, directory: DIRECTORY });
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

/** 会话树投影输入的 wire 形态 fixture：fork 链、孤儿 fork、subagent 父子关系（非 fork 血缘）。 */
const wireSessions = [
  { id: 'ses_1', title: 'root work', time: { created: 100, updated: 500 } },
  { id: 'ses_2', forkParentID: 'ses_1', title: 'fork of root', time: { created: 200, updated: 900 } },
  { id: 'ses_3', forkParentID: 'ses_2', title: 'grandchild', time: { created: 300, updated: 300 } },
  { id: 'ses_4', forkParentID: 'ses_gone', title: 'orphan fork', time: { created: 400, updated: 400 } },
  // Subagent parentage is not fork lineage: a wire parentID session stays a
  // root in the fork tree.
  { id: 'ses_sub', parentID: 'ses_1', title: 'subagent child', time: { created: 350, updated: 350 } },
];

/** §5.4 会话树：fork 血缘投影为扁平 {leafId, nodes}、血缘环被切断、subtree 只截取单个会话的血缘与后代。 */
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
      let cursor: import('./domain-uri.ts').SessionTreeNodeProjection | undefined = node;
      while (cursor?.parentId) {
        expect(seen.has(cursor.id)).toBe(false);
        seen.add(cursor.id);
        cursor = nodeById.get(cursor.parentId ?? '') ?? undefined;
      }
    }
  });

  test('subtree returns the lineage + descendants of one session', () => {
    const subtree = buildSessionSubtree('ses_2', wireSessions);
    expect(subtree?.nodes.map((n) => n.id).sort()).toEqual(['ses_1', 'ses_2', 'ses_3']);
    expect(subtree?.leafId).toBe('ses_2');
    expect(buildSessionSubtree('ses_missing', wireSessions)).toBeNull();
  });
});

/** §5.4.1 条目树快照：拍平 entries、跳过 label 节点、折叠 resolved label 与 gist 摘要。 */
describe('buildEntryTreeSnapshot (spec 04 §5.4.1)', () => {
  // 最小 SessionManager 替身：一棵含 label/branch_summary 的消息树，加平铺 entries 与 leafId。
  const manager = {
    getTree: (): FixtureTreeNode[] => [
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
    expect(snapshot.nodes[0]?.label).toBe('探索阶段');
    expect(snapshot.nodes[0]?.gist).toEqual({ role: 'user', preview: 'explore the repo' });
    expect(snapshot.nodes[1]?.gist?.toolName).toBe('read');
    expect(snapshot.nodes[2]?.gist?.preview).toContain('abandoned exploration');
    expect(snapshot.pathToLeaf).toEqual(['e_1', 'e_2', 'e_4']);
  });
});

/** navigate/label 契约：请求规范化默认值与 400 校验、tree 更新负载只允许 navigate|label|summary、流式期 409 busy 守卫。 */
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
    expect(normalizeNavigateRequest({ targetId: 'e_9', reanswerAskResult: { nope: true } }).response?.status).toBe(400);
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
    expect(normalizeLabelRequest({ targetId: 'e_1', label: 'phase' }).value?.label).toBe('phase');
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

/** 构造 AgentRefLike fixture：默认是 running 的子代理引用，overrides 原样覆盖（含 history 等敏感字段）。 */
const ref = (overrides: Partial<import('./domain-uri.ts').AgentRefLike> = {}) => ({
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

/** 用给定 refs 构造最小 registry 替身（只实现 list()）。 */
const makeRegistry = (...refs: import('./domain-uri.ts').AgentRefLike[]) => ({ list: () => refs });

/** §5.5.1 projectAgentRun 行投影：双段行键、剔除一切绝对路径、live 指标取自会话统计且非有限值时降级为无。 */
describe('projectAgentRun (spec 04 §5.5.1, R7)', () => {
  test('row carries the two-part key and NO absolute paths', () => {
    const row = projectAgentRun({ sessionID: 'ses_1', directory: DIRECTORY, ref: ref() });
    expect(row.key).toBe(runKey(DIRECTORY, 'ses_1', 'Anna'));
    expect(row.hasTranscript).toBe(true);
    expect(row.history?.outputPath).toBe('agent://Anna');
    expect('sessionFile' in row).toBe(false);
    expect(row.history?.patchPath).toBeUndefined();
    const serialized = JSON.stringify(row);
    expect(serialized).not.toContain('.jsonl');
    expect(serialized).not.toContain('C:/leak');
  });

  test('running rows carry live metrics from the session stats accessor', () => {
    // The real SDK session is a class whose getSessionStats reads #stats — an
    // unbound invocation throws, so the mock must be a this-sensitive method.
    class StatsSession {
      #stats = { tokens: { total: 1500 }, cost: 0.042 };
      getSessionStats() {
        return this.#stats;
      }
    }
    const row = projectAgentRun({
      sessionID: 'ses_1',
      directory: DIRECTORY,
      ref: ref({
        session: new StatsSession(),
      }),
    });
    // durationMs = lastActivity - createdAt, computed from registry fields.
    expect(row.live).toEqual({ tokens: 1500, cost: 0.042, durationMs: 9 });
  });

  test('live metrics are absent without a live session or a stats provider', () => {
    expect(projectAgentRun({ sessionID: 'ses_1', directory: DIRECTORY, ref: ref({ session: null }) }).live).toBeUndefined();
    expect(projectAgentRun({ sessionID: 'ses_1', directory: DIRECTORY, ref: ref({ session: {} }) }).live).toBeUndefined();
  });

  test('non-finite live tokens degrade to no live payload instead of NaN', () => {
    const row = projectAgentRun({
      sessionID: 'ses_1',
      directory: DIRECTORY,
      ref: ref({ session: { getSessionStats: () => ({ tokens: { total: Number.NaN }, cost: Number.NaN }) } }),
    });
    expect(row.live).toBeUndefined();
  });

});

/** §5.5.1 AgentRunsAggregator：目录+会话+agent 的行键隔离、合并窗口发布、盘扫历史行合并、状态排序与 ensureDirectory 预热。 */
describe('AgentRunsAggregator (spec 04 §5.5.1)', () => {
  /** Manual timer capture: coalescing is asserted deterministically, never
   * via wall-clock sleeps (the real timer path is exercised by the domain
   * integration test). */
  type ManualTimerState = { fn: (() => void) | null; ms: number | null; cleared: number };
  // 手动定时器替身：捕获合并窗口的 setTimeout 回调，fire() 主动触发到期，cleared 计数用于断言 dispose 清理。
  const manualTimers = () => {
    const state: ManualTimerState = { fn: null, ms: null, cleared: 0 };
    return {
      setTimeout: (fn: () => void, ms?: number) => {
        state.fn = fn;
        state.ms = ms ?? null;
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

  test('keys rows by directory+sessionID+agentId — same flat id across sessions stays distinct', () => {
    const aggregator = new AgentRunsAggregator({
      snapshot: () => [
        { sessionID: 'ses_A', directory: DIRECTORY, registry: makeRegistry(ref()) },
        { sessionID: 'ses_B', directory: 'C:/proj/beta', registry: makeRegistry(ref({ lastActivity: 99 })) },
      ],
      publish: () => {},
    });
    const { agentRuns } = aggregator.refresh();
    expect(agentRuns.map((r) => r.key).sort()).toEqual([
      runKey(DIRECTORY, 'ses_A', 'Anna'),
      runKey('C:/proj/beta', 'ses_B', 'Anna'),
    ]);
    aggregator.dispose();
  });

  test('the same session id materialized under two directories stays two isolated rows', () => {
    const aggregator = new AgentRunsAggregator({
      snapshot: () => [
        { sessionID: 'same', directory: DIRECTORY, registry: makeRegistry(ref({ id: 'Anna', activity: 'alpha work' })) },
        { sessionID: 'same', directory: 'C:/proj/beta', registry: makeRegistry(ref({ id: 'Anna', activity: 'beta work' })) },
      ],
      publish: () => {},
    });
    const { agentRuns } = aggregator.refresh();
    expect(agentRuns).toHaveLength(2);
    expect(aggregator.row('same', 'Anna', DIRECTORY)?.activity).toBe('alpha work');
    expect(aggregator.row('same', 'Anna', 'C:/proj/beta')?.activity).toBe('beta work');
    expect(aggregator.row('same', 'Anna')).toBeNull(); // ambiguous unscoped — never guess
    aggregator.dispose();
  });

  test('releaseForSession drops only the owning directory\'s rows', () => {
    const aggregator = new AgentRunsAggregator({
      snapshot: () => [
        { sessionID: 'same', directory: DIRECTORY, registry: makeRegistry(ref({ id: 'Anna' })) },
        { sessionID: 'same', directory: 'C:/proj/beta', registry: makeRegistry(ref({ id: 'Bob', displayName: 'Bob' })) },
      ],
      publish: () => {},
    });
    aggregator.refresh();
    expect(aggregator.releaseForSession(DIRECTORY, 'same')).toBe(1);
    expect(aggregator.row('same', 'Anna', DIRECTORY)).toBeNull();
    expect(aggregator.row('same', 'Bob', 'C:/proj/beta')?.agentId).toBe('Bob');
    aggregator.dispose();
  });

  test('coalesces bursts into one omp.agents.updated per directory with monotonic revision', () => {
    const events: Array<{ type: string; payload?: { revision?: number; agentRuns?: Array<{ key?: string; status?: string }> }; scope?: { directory?: string; durable?: boolean } }> = [];
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
    // SAFETY: test fixture narrowing — the asserted shape is the harness contract this test reads.
    expect((events[0].payload as { revision?: number }).revision).toBe(1);
    // SAFETY: test fixture narrowing — the asserted shape is the harness contract this test reads.
    expect(((events[0].payload as { agentRuns?: Array<{ key: string }> }).agentRuns ?? []).map((r) => r.key)).toEqual([runKey(DIRECTORY, 'ses_A', 'Anna')]);

    registry = makeRegistry(ref({ status: 'idle', activity: undefined }));
    aggregator.refresh();
    aggregator.flush(); // engine-style immediate flush
    expect(events.length).toBe(2);
    // SAFETY: test fixture narrowing — the asserted shape is the harness contract this test reads.
    expect((events[1].payload as { revision?: number }).revision).toBe(2);
    // SAFETY: test fixture narrowing — the asserted shape is the harness contract this test reads.
    expect((((events[1].payload as { agentRuns?: Array<{ status: string }> }).agentRuns ?? [])[0]).status).toBe('idle');
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
    expect(ghost?.status).toBe('historical');
    expect(ghost?.key).toBe(runKey(DIRECTORY, 'ses_C', 'Ghost'));
    const live = agentRuns.find((r) => r.key === runKey(DIRECTORY, 'ses_A', 'Anna'));
    expect(live?.status).toBe('running'); // registry wins, never historical
    aggregator.dispose();
  });

  test('ensureDirectory warms the disk cache and surfaces rows for directories without live rows', async () => {
    let warmed: string[] = [];
    let coldRows: Array<Record<string, unknown>> = [];
    const aggregator = new AgentRunsAggregator({
      snapshot: () => [], // no live rows at all — the pure-historical case
      diskScan: () => coldRows.map((row) => row as never),
      warmDiskScan: async (directory) => {
        // The engine hook is one-shot per directory (its own cache); model that.
        if (warmed.includes(directory)) return;
        warmed.push(directory);
        coldRows = [{ sessionID: 'ses_old', agentId: 'BranchScout', directory, childSessionID: 'ses_child_1' }];
      },
      publish: () => {},
    });
    // No ensure yet: refresh scans nothing (no live rows, no previous rows).
    expect(aggregator.refresh().agentRuns).toEqual([]);
    const snapshot = await aggregator.ensureDirectory(DIRECTORY);
    expect(warmed).toEqual([DIRECTORY]);
    const row = snapshot.agentRuns.find((r) => r.agentId === 'BranchScout');
    expect(row?.status).toBe('historical');
    expect(row?.childSessionID).toBe('ses_child_1');
    // One shot: the second ensure does not re-warm.
    await aggregator.ensureDirectory(DIRECTORY);
    expect(warmed).toEqual([DIRECTORY]);
    aggregator.dispose();
  });
  test('directory filter + emptied-directory publishes a full-replace empty snapshot', () => {
    const events: Array<{ type: string; payload?: { revision?: number; agentRuns?: Array<{ key: string }> }; scope?: { directory?: string } }> = [];
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
    // SAFETY: test fixture narrowing — the asserted shape is the harness contract this test reads.
    expect((last.scope as { directory?: string }).directory).toBe(DIRECTORY);
    // SAFETY: test fixture narrowing — the asserted shape is the harness contract this test reads.
    expect((last.payload as { agentRuns?: unknown[] }).agentRuns).toEqual([]);
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
/** agent-run 动作端点 JSON 响应中被断言的字段。 */
interface ActionResponseBody {
  /** 动作是否成功受理。 */
  ok?: boolean;
  /** 动作后的行状态（running/aborted 等）。 */
  status?: string;
  /** 机器可读错误码（historical、not-parked 等）。 */
  error?: string;
  /** 错误响应附带：该行是否可 revive。 */
  revivable?: boolean;
}


/** §5.5.2 agent-run 动作：historical 行一律 409 拒绝、parked 凭 descriptor 单次认领 revive、kill/chat 语义与入参校验。 */
describe('agent-run actions: parked vs historical (spec 04 §5.5.2, R2-M5)', () => {
  // 组装测试上下文：按 rows 建 aggregator，外加 ParkedAgentDescriptors 与 revive/kill/chat 计数桩。
  const setup = (rows: Array<Partial<import('./domain-uri.ts').AgentRefLike>>) => {
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
  // 以固定 sessionID/directory 调 handleAgentRunAction，并把 Response 解析成 { status, body }。
  const act = (ctx: { aggregator: import('./domain-uri.ts').AgentRunsAggregator; descriptors: import('./domain-uri.ts').ParkedAgentDescriptors; actions: { revive: () => Promise<void>; kill: () => Promise<void>; chat: () => Promise<void> } }, agentId: string, body: { kind: string; text?: string; messageId?: string; mode?: string }) =>
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
    void ctx; // no-op touch (private #rows access dropped under noImplicitAny)
    // force historical status via disk-scan style row injection
    const historicalCtx = {
      aggregator: {
        row: (sessionID: string, agentId: string) =>
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
        actions: {},
        sessionID: 'ses_A',
        agentId: 'Ghost',
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
      directory: DIRECTORY,
      ref: ref({ id: 'Parked', status: 'parked' }),
      revive: async () => ({}),
    });
    const revived = await act(ctx, 'Parked', { kind: 'revive' });
    expect(revived.status).toBe(200);
    expect(revived.body).toEqual({ ok: true, status: 'running' });
    expect(ctx.calls.revive).toBe(1);
    expect(ctx.descriptors.has('ses_A', 'Parked', DIRECTORY)).toBe(false); // single claim
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

    ctx.descriptors.register({ sessionID: 'ses_A', agentId: 'Parked', directory: DIRECTORY, revive: async () => ({}) });
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
/** jobs 端点 JSON 响应中被断言的字段。 */
interface JobsResponseBody {
  /** 501/200 响应回显的 owner 会话（无活跃会话时为 null）。 */
  ownerSessionID?: string | null;
  /** snapshot 钩子返回的投递状态，原样透传。 */
  delivery?: unknown;
  /** 机器可读错误码（jobs-unavailable）。 */
  error?: string;
}

/** §5.6 jobs 端点：能力关闭时返回结构化 501 并携带 ownerSessionID（绝不 404），开启时委托 snapshot 钩子并回显 owner。 */
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

/** Partial route ctx the direct handler invocations below pass. */
/** 直调 handler 时传入的部分路由 ctx（params/url/headers 皆可选）。 */
type UriCtxLite = { params?: Record<string, string>; url?: URL; headers?: Headers };

/** Minimal Request stand-in the uri handlers read (url + json body). */
/** uri handler 只读取的最小 Request 替身（url + json() body）。 */
type FakeUriRequest = { url: string; json: () => Promise<UriRequestBody | undefined> };

/** 按固定 url 与可选 body 构造 FakeUriRequest。 */
const fakeUriRequest = (url: string, body?: UriRequestBody): FakeUriRequest => ({ url, json: async () => body });

/** Session-tree fixture row (entry + label + children), matching buildSessionTree input. */
/** 会话树 fixture 节点（entry + label + children），对应 buildSessionTree 的输入形态。 */
type FixtureTreeNode = { entry: { type: string; id: string; parentId: string | null; timestamp: string; message?: { role?: unknown; content?: unknown }; summary?: string; targetId?: string; label?: string }; label?: string; children: FixtureTreeNode[] };

/** createUriDomain 组装与路由挂载的集成面：特性关闭时的逐端点 501 门禁，以及特性开启后走真实 SDK 数据的完整路由流。 */
describe('createUriDomain + mount (integration surface)', () => {
  // 把 domain.mount 注册的 (method, pattern, handler) 收进 Map，键为 "METHOD pattern"，供逐路由直调。
  const registerRoutes = (domain: { mount: (route: (m: string, p: string, h: import('./domain-uri.ts').UriRouteHandler) => void) => void }) => {
    const routes = new Map<string, import('./domain-uri.ts').UriRouteHandler>();
    domain.mount((method: string, pattern: string, handler: import('./domain-uri.ts').UriRouteHandler) => routes.set(`${method} ${pattern}`, handler));
    return routes;
  };
  // 复用 fakeUriRequest 构造请求（本块内的简短别名）。
  const fakeRequest = fakeUriRequest;
  // 按 url 与路径参数构造最小路由 ctx。
  const ctxFor = (url: string, params: Record<string, string> = {}) => ({ params, url: new URL(url), headers: new Headers() });

  test('features off → explicit 501s per key (fail loudly, R2)', async () => {
    const domain = createUriDomain({ tokens, localOptionsFor, features: () => ({ 'uri.v1': false, 'tree.v1': false, 'agentRuns.v1': false, 'jobs.v1': false }) });
    const routes = registerRoutes(domain);
    // SAFETY: test fixture narrowing — the asserted shape is the harness contract this test reads.
    const resolve = await (routes.get('POST /omp/uri/resolve') as (request: FakeUriRequest, ctx?: UriCtxLite) => Promise<Response>)(fakeRequest('http://x/omp/uri/resolve', { scheme: 'local', ref: 'scratch.md', sessionID: 'ses_A', directory: DIRECTORY }));
    expect(resolve.status).toBe(501);
    expect(await resolve.json()).toEqual({ error: 'uri.v1-unavailable' });
    // SAFETY: test fixture narrowing — the asserted shape is the harness contract this test reads.
    const tree = await (routes.get('GET /omp/sessions/{sessionID}/tree') as (request: FakeUriRequest, ctx?: UriCtxLite) => Promise<Response>)(
      fakeRequest(`http://x/omp/sessions/ses_1/tree?directory=${encodeURIComponent(DIRECTORY)}`),
      ctxFor(`http://x/omp/sessions/ses_1/tree?directory=${encodeURIComponent(DIRECTORY)}`, { sessionID: 'ses_1' }),
    );
    expect(tree.status).toBe(501);
    expect(await tree.json()).toEqual({ error: 'tree.v1-unavailable' });
    // SAFETY: test fixture narrowing — the asserted shape is the harness contract this test reads.
    const runs = await (routes.get('GET /omp/agent-runs') as (request: FakeUriRequest, ctx?: UriCtxLite) => Promise<Response>)(fakeRequest('http://x/omp/agent-runs?directory=C:/p'), ctxFor('http://x/omp/agent-runs?directory=C:/p'));
    expect(runs.status).toBe(501);
    // jobs is ALWAYS mounted — 501 is its steady state, not a gate (R12)
    // SAFETY: test fixture narrowing — the asserted shape is the harness contract this test reads.
    const jobs = await (routes.get('GET /omp/jobs') as (request: FakeUriRequest, ctx?: UriCtxLite) => Promise<Response>)(fakeRequest('http://x/omp/jobs'), ctxFor('http://x/omp/jobs'));
    expect(jobs.status).toBe(501);
    // SAFETY: test fixture narrowing — the asserted shape is the harness contract this test reads.
    expect(((await jobs.json()) as { error: string }).error).toBe('jobs-unavailable');
    domain.dispose();
  });

  test('features on → full route flow against real SDK router + registry data', async () => {
    const published: Array<{ type: string; payload?: import('./domain-uri.ts').AgentsUpdatedPayload | { directory?: string }; scope?: import('./domain-uri.ts').AgentRunsPublishScope | { directory?: string } }> = [];
    const domain = createUriDomain({
      features: () => ({ ...ompFeatures(), 'uri.v1': true, 'tree.v1': true, 'agentRuns.v1': true }),
      tokens,
      localOptionsFor,
      sessionTreeData: async () => wireSessions,
      agentsSnapshot: () => [
        { sessionID: 'ses_A', directory: DIRECTORY, registry: makeRegistry(ref()) },
      ],
      publish: (type: string, payload: import('./domain-uri.ts').AgentsUpdatedPayload, scope: import('./domain-uri.ts').AgentRunsPublishScope) => {
        // SAFETY: harness publish rows keep the payload/scope pairs verbatim.
        published.push({ type, payload, scope });
      },
      liveSessionIds: () => ['ses_A'],
    });
    const routes = registerRoutes(domain);
    domain.aggregator.refresh();
    await new Promise((resolve) => setTimeout(resolve, 300)); // > default 250ms coalesce

    // SAFETY: test fixture narrowing — the asserted shape is the harness contract this test reads.
    const resolve = await (routes.get('POST /omp/uri/resolve') as (request: FakeUriRequest, ctx?: UriCtxLite) => Promise<Response>)(
      fakeRequest('http://x/omp/uri/resolve', { scheme: 'local', ref: 'scratch.md', sessionID: 'ses_A', directory: DIRECTORY }),
    );
    expect(resolve.status).toBe(200);
    // SAFETY: resolve body is the UriResource wire record.
    const resource = await resolve.json() as { content: string; token: { id: string } };
    expect(resource.content).toBe('alpha session secret');
    expect('sourcePath' in resource).toBe(false);

    // SAFETY: test fixture narrowing — the asserted shape is the harness contract this test reads.
    const opened = await (routes.get('POST /omp/uri/open') as (request: FakeUriRequest, ctx?: UriCtxLite) => Promise<Response>)(
      fakeRequest('http://x/omp/uri/open', { token: resource.token?.id ?? '', directory: DIRECTORY }),
      ctxFor('http://x/omp/uri/open'),
    );
    expect(opened.status).toBe(200);

    const treeUrl = `http://x/omp/sessions/ses_2/tree?directory=${encodeURIComponent(DIRECTORY)}`;
    // SAFETY: test fixture narrowing — the asserted shape is the harness contract this test reads.
    const tree = await (routes.get('GET /omp/sessions/{sessionID}/tree') as (request: FakeUriRequest, ctx?: UriCtxLite) => Promise<Response>)(fakeRequest(treeUrl), ctxFor(treeUrl, { sessionID: 'ses_2' }));
    expect(tree.status).toBe(200);
    // SAFETY: tree body is the session-tree snapshot record.
    const treeBody = await tree.json() as { leafId: string; nodes: Array<{ id: string }> };
    expect(treeBody.leafId).toBe('ses_2');
    expect(treeBody.nodes.map((n) => n.id).sort()).toEqual(['ses_1', 'ses_2', 'ses_3']);
    // SAFETY: test fixture narrowing — the asserted shape is the harness contract this test reads.
    const missingTree = await (routes.get('GET /omp/sessions/{sessionID}/tree') as (request: FakeUriRequest, ctx?: UriCtxLite) => Promise<Response>)(
      fakeRequest(treeUrl),
      ctxFor(treeUrl, { sessionID: 'nope' }),
    );
    expect(missingTree.status).toBe(404);

    // SAFETY: test fixture narrowing — the asserted shape is the harness contract this test reads.
    const runs = await (routes.get('GET /omp/agent-runs') as (request: FakeUriRequest, ctx?: UriCtxLite) => Promise<Response>)(
      fakeRequest(`http://x/omp/agent-runs?directory=${encodeURIComponent(DIRECTORY)}`),
      ctxFor(`http://x/omp/agent-runs?directory=${encodeURIComponent(DIRECTORY)}`),
    );
    // SAFETY: agent-runs body is the aggregator snapshot.
    const runsBody = await runs.json() as { agentRuns: Array<{ key: string }>; revision: number };
    expect(runsBody.agentRuns.map((r) => r.key)).toEqual([runKey(DIRECTORY, 'ses_A', 'Anna')]);
    expect(runsBody.revision).toBeGreaterThan(0);

    // SAFETY: test fixture narrowing — the asserted shape is the harness contract this test reads.
    const action = await (routes.get('POST /omp/agent-runs/{sessionID}/{agentId}') as (request: FakeUriRequest, ctx?: UriCtxLite) => Promise<Response>)(
      fakeRequest('http://x/omp/agent-runs/ses_A/Anna', { kind: 'kill', directory: DIRECTORY }),
      ctxFor('http://x/omp/agent-runs/ses_A/Anna', { sessionID: 'ses_A', agentId: 'Anna' }),
    );
    expect(action.status).toBe(500);
    // SAFETY: test fixture narrowing — the asserted shape is the harness contract this test reads.
    expect(((await action.json()) as { error: string }).error).toBe('hook-unavailable');

    // SAFETY: test fixture narrowing — the asserted shape is the harness contract this test reads.
    expect(published.some((e) => e.type === OMP_AGENTS_UPDATED && ((e.scope as { directory?: string }).directory === DIRECTORY))).toBe(true);
    domain.dispose();
  });
});


/** host 层只读 artifacts 浏览：仅按会话列出 local:// 文件，mtime 倒序、truncated 透传、畸形行丢弃，含能力门禁与钩子缺失错误。 */
describe('artifacts browse (spec 04 — host-level read-only local:// listing)', () => {
  // 同上：把挂载的路由收进 Map，键为 "METHOD pattern"。
  const registerRoutes = (domain: { mount: (route: (m: string, p: string, h: import('./domain-uri.ts').UriRouteHandler) => void) => void }) => {
    const routes = new Map<string, import('./domain-uri.ts').UriRouteHandler>();
    domain.mount((method: string, pattern: string, handler: import('./domain-uri.ts').UriRouteHandler) => routes.set(`${method} ${pattern}`, handler));
    return routes;
  };
  // 按 url 与路径参数构造最小路由 ctx。
  const ctxFor = (url: string, params: Record<string, string> = {}) => ({ params, url: new URL(url), headers: new Headers() });

  // localFiles 钩子返回的文件列表形态（相对 ref + 大小 + 修改时间，附 truncated 标志）。
  type LocalFilesListing = { files: Array<{ ref: string; size: number; modifiedAt: number }>; truncated: boolean };
  // 两个会话的固定列表数据：ses_A 两个文件，ses_B 为权威空列表（无 local:// 根但仍可索引）。
  const localFilesOf = {
    ses_A: {
      files: [
        { ref: 'PLAN.md', size: 10, modifiedAt: 5 },
        { ref: 'scratch/notes.md', size: 20, modifiedAt: 9 },
      ],
      truncated: false,
    },
    // Session with no local:// root yet — authoritative empty, still indexed.
    // SAFETY: test fixture narrowing — the asserted shape is the harness contract this test reads.
    ses_B: { files: [] as Array<{ ref: string; size: number; modifiedAt: number }>, truncated: false },
  };
  // SAFETY: the fixture declares exactly two session rows; unknown ids read null.
  const localFiles = async (sessionID: string): Promise<LocalFilesListing | null> =>
    sessionID === 'ses_A' ? localFilesOf.ses_A : sessionID === 'ses_B' ? localFilesOf.ses_B : null;
  // 会话元数据 fixture，供 session-not-found 与标题兜底断言。
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
    // SAFETY: test fixture narrowing — the asserted shape is the harness contract this test reads.
    const blocked = await (routes.get('GET /omp/artifacts') as (request: FakeUriRequest, ctx?: UriCtxLite) => Promise<Response>)(
      fakeUriRequest(`http://x/omp/artifacts?directory=${encodeURIComponent(DIRECTORY)}`),
      ctxFor(`http://x/omp/artifacts?directory=${encodeURIComponent(DIRECTORY)}`),
    );
    expect(blocked.status).toBe(501);
    expect(await blocked.json()).toEqual({ error: 'artifacts-unavailable' });
    off.dispose();

    const noHook = createUriDomain({
      features: () => ({ ...ompFeatures(), artifacts: true }),
    });
    const noHookRoutes = registerRoutes(noHook);
    // SAFETY: test fixture narrowing — the asserted shape is the harness contract this test reads.
    const hookless = await (noHookRoutes.get('GET /omp/artifacts') as (request: FakeUriRequest, ctx?: UriCtxLite) => Promise<Response>)(
      fakeUriRequest(`http://x/omp/artifacts?directory=${encodeURIComponent(DIRECTORY)}&sessionID=ses_A`),
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
    // SAFETY: test fixture narrowing — the asserted shape is the harness contract this test reads.
    const ok = await (onRoutes.get('GET /omp/artifacts') as (request: FakeUriRequest, ctx?: UriCtxLite) => Promise<Response>)(
      fakeUriRequest(`http://x/omp/artifacts?directory=${encodeURIComponent(DIRECTORY)}&sessionID=ses_A`),
      ctxFor(`http://x/omp/artifacts?directory=${encodeURIComponent(DIRECTORY)}&sessionID=ses_A`),
    );
    expect(ok.status).toBe(200);
    // SAFETY: artifacts body is the listing record.
    expect(((await ok.json()) as { files: unknown[] }).files).toHaveLength(2);
    on.dispose();
  });
});

/** §5.2.4 二进制预览：图片 resolve 返回 mime + token 的 binary 描述符（无占位内容），uri.content 按 token scope 流式返回原始字节。 */
describe('local:// binary preview (spec 04 §5.2.4 — token byte stream)', () => {
  // 10 字节 PNG fixture：8 字节魔数加两个载荷字节，用于字节流断言。
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
    const res = await domain.uri.content({ id: body.token?.id, directory: DIRECTORY });
    expect(res.status).toBe(200);
    expect(res.headers.get('content-type')).toBe('image/png');
    expect(res.headers.get('x-content-type-options')).toBe('nosniff');
    const bytes = Buffer.from(await res.arrayBuffer());
    expect(bytes.subarray(0, 8).equals(PNG_BYTES.subarray(0, 8))).toBe(true);

    const scoped = await domain.uri.content({ id: body.token?.id, directory: 'C:/other' });
    expect(scoped.status).toBe(403);
    domain.dispose();
  });
});