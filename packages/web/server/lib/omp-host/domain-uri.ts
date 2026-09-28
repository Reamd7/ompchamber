// Domain module: omp-parity chapter 04 (protocols & entities), server side.
//
// Covers, per docs/omp-parity/04-protocols-and-entities.md v3 + 00-MASTER D6:
//   §5.2  local:// URI bridge — session-pinned resolution with ZERO SDK global
//          mutation (R7/R8: never registerArtifactsDir, never
//          LocalProtocolHandler.setOverride) + opaque resource tokens
//          replacing the sourcePath echo (R7 / §5.2.4).
//   §5.4  session tree — directory-level fork/parent projection from the
//          sidecar registry + per-session entry-tree snapshot; navigate/label
//          contract exports (engine hook points, not wired here).
//   §5.5  AgentRunsAggregator — sessionID::agentId keyed rows, 250ms
//          coalesced omp.agents.updated snapshots, parked/historical split
//          (R2-M5: historical rows are transcript-view only).
//   §5.6  jobs — structured 501 + ownerSessionID until the SDK exposes an
//          AsyncJobManager injection point (R12; C2 single-manager limit).
//
// SELF-CONTAINED BY CONTRACT: this module does not touch engine.js /
// endpoints.js / omp-parity.js / manifests. The coordinator integrates via
// `createUriDomain(deps)`; every engine data dependency is an injected hook
// and documented below. All routes this module can mount live under /omp/...
// (public paths /api/omp/... — the web proxy strips the /api prefix).
//
// Verified SDK ground truth (installed src, checked before writing):
// - local-protocol.ts:448-488 — LocalProtocolHandler.resolveOptions: caller
//   context.localProtocolOptions wins over the process override and the
//   global registry; missing options → "No session - local:// unavailable".
//   resolveLocalTarget (local-protocol.ts:344-394) + ensureWithinRoot (:21-25)
//   enforce root containment; validateRelativePath (skill-protocol.ts:28-42,
//   reused by local) rejects '..' segments and absolute paths.
// - router.ts:137-152 — resolve(input, context) threads ResolveContext;
//   write() throws for handlers without a write hook — LocalProtocolHandler
//   has none, so router-mediated local:// writes are impossible. P1 writes
//   stay model-side: the session's own write tool, pinned by the same
//   localProtocolOptions the engine passes to createAgentSession
//   (sdk.ts:552 option, sdk.ts:1811-1819 threading).
// - agent-registry.ts:72-97, 269-301 — AgentRef {id, displayName, kind,
//   parentId?, status: running|idle|parked|aborted, session, sessionFile,
//   createdAt, lastActivity, activity?, history?}; registry.list().
// - session-manager.ts — getLeafId (:2360), getEntries (:2442), getTree
//   (:2450), appendLabelChange (:2397); session-entries.ts:58-63 entry base
//   {type, id, parentId, timestamp}, :289-295 SessionTreeNode {entry,
//   children, label?}.
//
// Rate limiting note (§5.2.2): the URI surface rides the same omp-host Basic
// auth as /api/fs/* full-disk access; no per-scheme auth layer is added. The
// 2 MiB inline cap + 2 KiB URL cap + short-TTL bounded-read tokens are the
// endpoint-side abuse bounds; UI-side hover resolution stays debounced
// (§5.2.5, no stat-probe on render).
/**
 * 模块：omp-parity 第 04 章（协议与实体）的 Node 服务端实现 ——
 * local:// URI 桥接、会话树投影、AgentRuns 聚合与 jobs 占位
 * （对应 docs/omp-parity/04-protocols-and-entities.md；上方英文头注释
 * 是逐节对照的规格索引）。
 *
 * 核心约定：
 * - 会话钉扎（session-pinned）：local:// 的解析绝不触碰 SDK 全局状态
 *   （不 registerArtifactsDir、不 LocalProtocolHandler.setOverride），
 *   一律经每次请求注入的 ResolveContext.localProtocolOptions 完成。
 * - 不透明资源 token 取代 SDK 的 sourcePath 回显：绝对路径只存在于
 *   token 服务的进程内存，绝不序列化进任何响应。
 * - 自包含：本模块不触碰 engine.js / endpoints.js / omp-parity.js /
 *   manifests；协调方经 createUriDomain(deps) 注入全部引擎钩子并挂载
 *   /omp/... 路由（公网路径 /api/omp/...，web proxy 会剥掉 /api 前缀）。
 */

import crypto from 'node:crypto';
import fs from 'node:fs/promises';
import path from 'node:path';
import { InternalUrlRouter } from '@oh-my-pi/pi-coding-agent/internal-urls/router';
import type { InternalResource } from '@oh-my-pi/pi-coding-agent/internal-urls/types';
import type { LocalProtocolOptions } from '@oh-my-pi/pi-coding-agent/internal-urls/local-protocol';
import { normalizeDirectoryKey } from './registry.ts';
import { errorText, ompFeatures, featureUnavailable } from './omp-parity.ts';
import type { OmpFeatures } from './omp-parity.ts';

/** 统一的 JSON 响应构造器：Response.json 的薄封装，让各 handler 以
 * 一致姿势返回 JSON（可附带 status/headers 等 ResponseInit）。 */
const json = <T,>(data: T, init?: ResponseInit): Response => Response.json(data, init);

/** 扩展名 → 可预览二进制 MIME 的映射（local:// 媒体；SDK 的非视觉
 * 图片回退会把 PNG 写到这里）。文本资源继续走 JSON resolve/open 路径；
 * 命中此表的资源改走字节流 token URL。 */

/** Previewable binary mime types by file extension (local:// media — the
 *  SDK's non-visual image fallback writes PNGs here). Text stays on the JSON
 *  resolve/open path; these switch the viewer to the byte-stream token URL. */
const BINARY_MIME_BY_EXT = new Map([
  ['.png', 'image/png'],
  ['.jpg', 'image/jpeg'],
  ['.jpeg', 'image/jpeg'],
  ['.gif', 'image/gif'],
  ['.webp', 'image/webp'],
  ['.bmp', 'image/bmp'],
  ['.ico', 'image/x-icon'],
]);

/** 单次 raw token 兑换的字节数上限（spec §5.2.4 字节流；面向可预览
 * 媒体的 inline 天花板，而非任意大文件传输通道）。 */

/** Cap for one raw token redemption (spec §5.2.4 byte stream; the artifact
 *  inline ceiling — previewable media, not arbitrary large-file transfer). */
const MAX_RAW_BYTES = 8 * 1024 * 1024;

/** 提取路径的小写扩展名（含点，如 '.png'）；反斜杠统一按分隔符处理，
 * 无扩展名返回空串。仅用于二进制 MIME 查表。 */
const extensionOf = (value: string) => {
  const base = String(value ?? '').replaceAll('\\', '/').split('/').pop() ?? '';
  const dot = base.lastIndexOf('.');
  return dot === -1 ? '' : base.slice(dot).toLowerCase();
};

// ---------------------------------------------------------------------------
// §5.2 local:// URI bridge
// ---------------------------------------------------------------------------

/** 允许在 HOST 侧做读取解析的 scheme 白名单（master R7：P1 仅 local://；
 * agent/history/artifact 待上游支持按次解析的
 * ResolveContext.artifactsDirs 后再开放，R2-H2）。 */

/** Schemes whose HOST-side read resolution is enabled (master R7: P1 =
 *  local:// only; agent/history/artifact stay off until upstream per-resolve
 *  ResolveContext.artifactsDirs, R2-H2). */
const ENABLED_READ_SCHEMES = ['local'];

/** 经路由中转的写入 scheme：无。LocalProtocolHandler 未暴露 write 钩子
 * （router.ts:147-150 会抛错），故 write 保持空数组 —— local:// 写入由
 * 会话自身的 write 工具按同一份钉扎的 localProtocolOptions 完成
 * （master R8：P1 仅 local、同目录同会话）。 */

/** Router-mediated writes: none. LocalProtocolHandler exposes no write hook
 *  (router.ts:147-150 throws), so `write` stays empty — local:// writes are
 *  performed by the session's own write tool against the same pinned
 *  localProtocolOptions (master R8: P1 local-only, same directory+session). */
const ENABLED_WRITE_SCHEMES: string[] = [];

/** §5.2.2：resolve 请求中 `u` 的长度上限（2 KiB）。 */

/** §5.2.2: `u` length ≤ 2 KiB. */
const MAX_URL_LENGTH = 2048;

/** 端点 inline 内容上限（任务 §5.2.1 备注；SDK local handler 本身已
 * 拒绝 >1 MiB 文本与已知二进制文件）。 */

/** Endpoint inline-content cap (task §5.2.1 note; the SDK local handler
 *  already refuses >1 MiB text and known-binary files). */
const MAX_INLINE_BYTES = 2 * 1024 * 1024;

/** 返回 scheme × 读/写能力矩阵（R2-M11 的形状、P1 的取值）：
 * read = ['local']，write = []。每次调用返回新数组，调用方可安全修改。 */

/** Scheme × read/write capability matrix (R2-M11 shape, P1 values). */
export const uriCapabilities = () => ({
  read: [...ENABLED_READ_SCHEMES],
  write: [...ENABLED_WRITE_SCHEMES],
});

/** createLocalProtocolOptions 的 artifactsDir 形参类型：常量
 * （string/null/undefined）或按 (sessionId, directory) 惰性求值的引擎
 * 钩子；紧随其后的英文 JSDoc 详述由它构造的会话钉扎选项。 */

/**
 * Session-pinned local:// options (master R7). The returned mapping pins
 * every local:// resolve/write performed with it to ONE session's artifacts
 * directory — the SDK resolves the root as `<artifactsDir>/local`
 * (local-protocol.ts:243-254) and containment stays inside the handler.
 *
 * ZERO SDK global mutation by construction: the options travel per-request
 * through ResolveContext.localProtocolOptions (resolution order #1,
 * local-protocol.ts:468-482) — this module never calls registerArtifactsDir
 * or LocalProtocolHandler.setOverride (guarded by test: source scan).
 *
 * @param {string} sessionId session the root is pinned to (getSessionId).
 * @param {string} directory the session's project directory — scope key used
 *   for token issuance/redemption checks, not for path math (artifacts dirs
 *   live under the omp agentDir, not the project).
 * @param {string | ((sessionId: string, directory: string) => string | null | undefined)} artifactsDir
 *   the session's artifacts directory (SessionManager.getArtifactsDir():
 *   sessionFile with '.jsonl' stripped) — a constant or an engine-resolved
 *   lookup. Engine hook: live hostSession.agentSession.sessionManager, or a
 *   cold read-only SessionManager.open(file.path).
 * @returns {{ getSessionId(): string, getArtifactsDir(): string | null }}
 */
export type ArtifactsDirSource =
  | string
  | null
  | undefined
  | ((sessionId: string, directory: string) => string | null | undefined);

/**
 * 构造会话钉扎的 local:// 协议选项（master R7）。返回的映射把所有借它
 * 执行的 local:// 解析/写入钉死到唯一会话的 artifacts 目录 —— SDK 会把
 * 根解析为 `<artifactsDir>/local`（local-protocol.ts:243-254），目录
 * 围栏由 handler 内部保证。
 *
 * 构造上即做到零 SDK 全局变更：选项经 ResolveContext.localProtocolOptions
 * 按请求传递（解析优先级 #1，local-protocol.ts:468-482）—— 本模块从不
 * 调用 registerArtifactsDir 或 LocalProtocolHandler.setOverride（由测试
 * 的源码扫描守护）。
 *
 * @param sessionId 被钉扎的会话 id（getSessionId 的返回值）。
 * @param directory 会话的项目目录 —— 仅作 token 签发/兑换的范围键，
 *   不参与路径计算（artifacts 目录在 omp agentDir 之下，不在项目里）。
 * @param artifactsDir 会话的 artifacts 目录（SessionManager.getArtifactsDir()：
 *   去掉 '.jsonl' 的 transcript 文件）—— 常量或引擎侧惰性查找。
 * @returns {getSessionId(): string, getArtifactsDir(): string | null}
 * @throws {TypeError} sessionId 非字符串或为空时抛出。
 */
export const createLocalProtocolOptions = (
  sessionId: string,
  directory: string,
  artifactsDir: ArtifactsDirSource,
): LocalProtocolOptions => {
  if (typeof sessionId !== 'string' || sessionId.length === 0) {
    throw new TypeError('createLocalProtocolOptions: sessionId is required');
  }
  const resolveDir = () =>
    typeof artifactsDir === 'function' ? artifactsDir(sessionId, directory) : artifactsDir;
  return {
    getSessionId: () => sessionId,
    getArtifactsDir: () => {
      const dir = resolveDir();
      return typeof dir === 'string' && dir ? dir : null;
    },
  };
};

/**
 * 由 transcript 文件路径推导每会话 artifacts 目录（与 TUI 对齐）：
 * 去掉 '.jsonl' 后缀的同级目录，即 SessionManager.getArtifactsDir()
 * 解析的结果（session-manager.ts:109-112，artifactsDirectoryFor，未导出）。
 * 所有 local:// 根必须经由这个每会话目录派生，绝不能用项目级会话
 * 目录，同一目录下的会话才能保持私有根（spec 04 §5.2.3 会话钉扎；
 * 跨边界拷贝必须显式进行）。
 *
 * @param sessionFile 以 '.jsonl' 结尾的绝对 transcript 路径。
 * @returns artifacts 目录；非 transcript 路径返回 null。
 */

/**
 * Per-session artifacts directory (TUI parity): the transcript file with its
 * '.jsonl' suffix stripped — the sibling directory SessionManager.getArtifactsDir()
 * resolves (session-manager.ts:109-112, artifactsDirectoryFor — not exported).
 * Every local:// root must be derived through this per-session dir, never the
 * project-level session directory, so sessions in one directory keep private
 * roots (spec 04 §5.2.3 session pinning; cross-boundary copy is explicit).
 *
 * @param {string} sessionFile absolute transcript path ending in '.jsonl'.
 * @returns {string | null} artifacts dir, or null for non-transcript paths.
 */
export const artifactsDirForSessionFile = (sessionFile: string | { path: string }) =>
  typeof sessionFile === 'string' && sessionFile.endsWith('.jsonl')
    ? sessionFile.slice(0, -'.jsonl'.length)
    : null;

/** UriTokenService 的构造选项：token 生命周期、最大读取次数与可注入
 * 时钟（测试用）。 */

/**
 * Opaque resource tokens — the authorized stand-in for the SDK
 * InternalResource.sourcePath echo (R7 / §5.2.4; SDK types.ts:25 keeps that
 * field "for debugging, not exposed to agent"). Ids carry no path or
 * resource information; the absolute path exists only in process memory and
 * is never serialized into any response.
 */
export interface UriTokenOptions {
  /** token 有效期（毫秒），默认 10 分钟。 */
  ttlMs?: number;
  /** 单 token 最大兑换次数，默认 32；耗尽即失效。 */
  maxReads?: number;
  /** 可注入的当前时间函数（默认 Date.now，测试用来推进时钟）。 */
  now?: () => number;
}

/** 签发 token 时登记的单个资源描述：resourceUrl/directory 等元数据，
 * 加上仅在进程内持有的 absolutePath（绝不外泄到响应）。 */
export interface UriTokenResource {
  /** 资源的内部 URI（如 local://...）。 */
  resourceUrl: string;
  /** 签发时的作用域目录（兑换时校验）。 */
  directory: string;
  /** 关联会话 id（可选，仅元数据）。 */
  sessionID?: string;
  /** 资源 MIME 类型（可选）。 */
  contentType?: string;
  /** 资源字节数（可选）。 */
  size?: number;
  /** 是否不可变（影响响应中的 editable 标志）。 */
  immutable?: boolean;
  /** 服务端绝对路径 —— 只存内存，绝不序列化进任何响应。 */
  absolutePath: string;
}

/** token 签发结果：不透明 id 与绝对过期时间戳。 */
export interface UriTokenIssueResult {
  /** 不透明 token id（ocuri_ 前缀的随机 base64url）。 */
  id: string;
  /** 过期时间戳（ms，基于注入的 now()）。 */
  expiresAt: number;
}

/** token 服务内部的单条登记项（模块私有）：资源完整快照 + 签发/过期
 * 时间与读取计数。 */
interface UriTokenEntry {
  /** 服务端绝对路径（仅存内存）。 */
  absolutePath: string;
  /** 资源的内部 URI。 */
  resourceUrl: string;
  /** 归一化后的作用域目录。 */
  directory: string;
  /** 关联会话 id（可选）。 */
  sessionID?: string;
  /** MIME 类型（可选）。 */
  contentType?: string;
  /** 字节数（可选）。 */
  size?: number;
  /** 是否不可变（可选）。 */
  immutable?: boolean;
  /** 签发时间戳（ms）。 */
  issuedAt: number;
  /** 过期时间戳（ms）。 */
  expiresAt: number;
  /** 允许的最大兑换次数。 */
  maxReads: number;
  /** 已兑换次数；达到 maxReads 后条目即删。 */
  reads: number;
}

/**
 * 不透明资源 token 服务（§5.2.4）：为已解析的 local:// 资源签发短命、
 * 限次的 token，作为 SDK InternalResource.sourcePath 回显的授权替代
 * （R7；SDK types.ts:25 注明该字段"仅供调试，不暴露给 agent"）。
 * id 不携带任何路径或资源信息；绝对路径只存在于进程内存，绝不会被
 * 序列化进任何响应。
 */

export class UriTokenService {
  /** token 有效期（毫秒）。 */
  ttlMs: number;
  /** 每个 token 的默认最大兑换次数。 */
  maxReads: number;
  /** 当前时间函数（可注入，测试用）。 */
  now: () => number;

  /** 构造函数：落存可选的 ttl/maxReads/now 覆盖项（默认 10 分钟/32 次/
   * Date.now）。 */
  /** @param {{ ttlMs?: number, maxReads?: number, now?: () => number }} [options] */
  constructor({ ttlMs = 10 * 60 * 1000, maxReads = 32, now = () => Date.now() }: UriTokenOptions = {}) {
    this.ttlMs = ttlMs;
    this.maxReads = maxReads;
    this.now = now;
  }

  /** 活跃 token 登记表：id → 登记项；签发时顺带清扫过期项。 */
  #entries = new Map<string, UriTokenEntry>();

  /**
   * 为一个已解析的资源铸造 token：生成 32 字节随机 base64url id
   * （ocuri_ 前缀），登记资源快照（目录经 normalizeDirectoryKey 归一化），
   * 顺带清扫，返回 {id, expiresAt}。
   * @param resource 资源描述（见 UriTokenResource；absolutePath 只入内存）。
   */

  /**
   * Mint a token for one resolved resource.
   * @param {{ resourceUrl: string, directory: string, sessionID?: string,
   *           contentType?: string, size?: number, immutable?: boolean,
   *           absolutePath: string }} resource
   * @returns {{ id: string, expiresAt: number }}
   */
  issue({ resourceUrl, directory, sessionID, contentType, size, immutable, absolutePath }: UriTokenResource): UriTokenIssueResult {
    const id = `ocuri_${crypto.randomBytes(32).toString('base64url')}`;
    const issuedAt = this.now();
    const expiresAt = issuedAt + this.ttlMs;
    this.#entries.set(id, {
      absolutePath,
      resourceUrl,
      directory: normalizeDirectoryKey(directory),
      ...(sessionID ? { sessionID } : {}),
      ...(contentType ? { contentType } : {}),
      ...(typeof size === 'number' ? { size } : {}),
      ...(immutable !== undefined ? { immutable } : {}),
      issuedAt,
      expiresAt,
      maxReads: this.maxReads,
      reads: 0,
    });
    this.sweep();
    return { id, expiresAt };
  }

  /** 清扫过期/耗尽的 token。签发路径会顺带调用；引擎的空闲清扫器也会
   * 周期性调用，避免空闲主机长期保留 token（plan §6）。 */

  /**
   * Sweep expired/exhausted tokens. Issue-time sweeping only runs when
   * tokens are being minted; the engine's idle sweeper calls this
   * periodically so idle hosts do not retain tokens forever (plan §6).
   */
  sweep(): void {
    const now = this.now();
    for (const [id, entry] of this.#entries) {
      if (entry.expiresAt <= now || entry.reads >= entry.maxReads) this.#entries.delete(id);
    }
  }

  /** 查找绑定到 directory 的活跃 token：id 缺失 400；不存在或已过期
   * 404 token-not-found；目录不匹配 403 scope（纵深防御，§5.2.4）。
   * @returns 命中返回登记项；失败返回可直接回给客户端的 Response。 */

  /**
   * Look up a live token bound to `directory`.
   * @returns {{ ok: true, entry: UriTokenEntry } | { ok: false, response: Response }}
   */
  #lookup(id: string | null | undefined, directory?: string | null): { ok: true; entry: UriTokenEntry } | { ok: false; response: Response } {
    if (typeof id !== 'string' || id.length === 0) {
      return { ok: false, response: json({ error: 'token-required' }, { status: 400 }) };
    }
    const entry = this.#entries.get(id);
    const now = this.now();
    if (!entry || entry.expiresAt <= now) {
      return { ok: false, response: json({ error: 'token-not-found' }, { status: 404 }) };
    }
    if (directory && normalizeDirectoryKey(directory) !== entry.directory) {
      // Defense in depth (§5.2.4): issuing directory must match the request's.
      return { ok: false, response: json({ error: 'scope' }, { status: 403 }) };
    }
    return { ok: true, entry };
  }

  /** GET /omp/uri/info 的元数据描述：只含 url/contentType/size/immutable/
   * editable/filename/expiresAt/readsLeft —— 无内容、无路径字段，
   * 且不消耗读取次数。 */

  /**
   * Metadata descriptor for GET /omp/uri/info — no content, no path fields.
   * Does not consume a read.
   */
  describe(id: string | null | undefined, { directory }: { directory?: string | null } = {}): Response {
    const found = this.#lookup(id, directory);
    if (found.ok === false) return found.response;
    const { entry } = found;
    return json({
      url: entry.resourceUrl,
      ...(entry.contentType ? { contentType: entry.contentType } : {}),
      ...(typeof entry.size === 'number' ? { size: entry.size } : {}),
      ...(entry.immutable !== undefined ? { immutable: entry.immutable } : {}),
      editable: !entry.immutable,
      filename: path.basename(entry.absolutePath),
      expiresAt: entry.expiresAt,
      readsLeft: Math.max(0, entry.maxReads - entry.reads),
    });
  }

  /** 兑换 token 取文本内容（POST /omp/uri/open）：服务端按登记的绝对
   * 路径读文件（utf8），消耗一次读取；超限或过期回 404（查看器应重新
   * resolve 换新 token）；超过 inline 上限回 413；读失败同样按 404
   * token-not-found 处理。 */

  /**
   * Redeem a token for content (POST /omp/uri/open). Reads the file from the
   * stored absolute path server-side; consumes one read; over-limit or
   * expired tokens 404 (viewer re-resolves for a fresh token).
   */
  async open(id: string | null | undefined, { directory }: { directory?: string | null } = {}): Promise<Response> {
    const found = this.#lookup(id, directory);
    if (found.ok === false) return found.response;
    const { entry } = found;
    let content: string;
    try {
      content = await fs.readFile(entry.absolutePath, 'utf8');
    } catch {
      return json({ error: 'token-not-found' }, { status: 404 });
    }
    if (Buffer.byteLength(content, 'utf8') > MAX_INLINE_BYTES) {
      return json({ error: 'too-large', size: Buffer.byteLength(content, 'utf8') }, { status: 413 });
    }
    entry.reads += 1;
    if (entry.reads >= entry.maxReads && typeof id === 'string') this.#entries.delete(id);
    return json({
      url: entry.resourceUrl,
      content,
      ...(entry.contentType ? { contentType: entry.contentType } : {}),
      size: Buffer.byteLength(content, 'utf8'),
      ...(entry.immutable !== undefined ? { immutable: entry.immutable } : {}),
      editable: !entry.immutable,
      filename: path.basename(entry.absolutePath),
    });
  }

  /** 兑换 token 取原始字节（GET /omp/uri/tokens/{id}/content，§5.2.4）：
   * 不做 utf8 强转地返回可预览二进制（图片）；消耗一次读取，作用域/
   * 过期规则与 open() 相同；超过 MAX_RAW_BYTES 回 413；读文件失败按
   * 404 token-not-found 处理。
   * @returns 成功返回 {bytes, contentType, filename}；失败返回 Response。 */

  /**
   * Redeem a token for RAW BYTES (GET /omp/uri/tokens/{id}/content,
   * §5.2.4). Streams previewable binaries (images) without utf8 coercion;
   * consumes one read; same scope/expiry rules as open().
   * @returns {{ ok: true, bytes: Buffer, contentType: string, filename: string } | { ok: false, response: Response }}
   */
  async openRaw(
    id: string | null | undefined,
    { directory }: { directory?: string | null } = {},
  ): Promise<{ ok: true; bytes: Buffer; contentType: string; filename: string } | { ok: false; response: Response }> {
    const found = this.#lookup(id, directory);
    if (found.ok === false) return found;
    const { entry } = found;
    let bytes;
    try {
      bytes = await fs.readFile(entry.absolutePath);
    } catch {
      return { ok: false, response: json({ error: 'token-not-found' }, { status: 404 }) };
    }
    if (bytes.byteLength > MAX_RAW_BYTES) {
      return { ok: false, response: json({ error: 'too-large', size: bytes.byteLength }, { status: 413 }) };
    }
    entry.reads += 1;
    // SAFETY: `found` resolved through this exact id, so it is a live key.
    const liveKey = id as string;
    if (entry.reads >= entry.maxReads) this.#entries.delete(liveKey);
    return {
      ok: true,
      bytes,
      contentType: entry.contentType ?? 'application/octet-stream',
      filename: path.basename(entry.absolutePath),
    };
  }
}

/** POST /omp/uri/resolve 的请求体（宽松类型：字段一律 unknown，
 * handler 逐字段做运行时校验）。 */
export interface UriResolveBody {
  /** 完整 URI 字符串（与 scheme/ref 二选一）。 */
  u?: unknown;
  /** scheme 部分（与 ref 搭配使用）。 */
  scheme?: unknown;
  /** scheme 内的引用部分。 */
  ref?: unknown;
  /** local:// 必填：所属会话 id。 */
  sessionID?: unknown;
  /** 作用域项目目录。 */
  directory?: unknown;
  /** true 时只解析路径不读内容。 */
  pathOnly?: unknown;
}

/** 引擎钩子：返回某个会话的 local:// 协议选项。冷路径需要先解析会话
 * 文件，因此异步返回是契约的一部分（spec 04 §5.2.3：钩子可能要查
 * SessionManager 索引）。 */

/** Engine hook: local:// protocol options for one session. Cold paths
 * resolve the session file first, so async returns are part of the contract
 * (spec 04 §5.2.3: the hook may hit the SessionManager index). */
export type LocalOptionsForHook = (
  sessionID: string,
  directory: string,
) => LocalProtocolOptions | null | Promise<LocalProtocolOptions | null>;

/** handleUriResolve 的输入：请求体 + 必需的 localOptionsFor 钩子与
 * token 服务；router 可选注入（缺省取全局单例）。 */
export interface UriResolveInput {
  /** 解析请求体（缺省空对象）。 */
  body?: UriResolveBody;
  /** 会话 → local:// 选项 的引擎钩子（必需）。 */
  localOptionsFor: LocalOptionsForHook;
  /** 资源 token 服务（必需）。 */
  tokens: UriTokenService;
  /** 可注入的内部 URL 路由器（缺省 InternalUrlRouter.instance()）。 */
  router?: InternalUrlRouter;
}

/**
 * POST /omp/uri/resolve 的 handler 核心（公网路径 /api/omp/uri/resolve）。
 *
 * 请求体：{scheme, ref, sessionID?, directory?, pathOnly?} 或 {u, ...}。
 *  - scheme 不在 uriCapabilities().read 中 → 501 {error:'scheme-not-enabled'}
 *    （R2-H2/R2-M11 —— agent/history/artifact/mcp/ssh/... 都落在这里）。
 *  - 未知 scheme（无原生 handler；含 file/http/https）→
 *    404 {error:'unknown-scheme'} —— 绝不暴露 MCP 回退。
 *  - local:// 必须带 sessionID（否则 400 session-required）与 directory
 *    （400 directory-required）；解析按请求钉扎，绝不依赖全局状态。
 *  - 响应 = InternalResource 去掉 sourcePath，再附不透明 token。路径
 *    穿越/未找到等错误透传 handler 自己的 message（404）；缺选项
 *    （理论不可达）按 §5.2.3 回 409；inline 内容超上限回 413。
 *  - 可预览二进制（图片后缀）改回 binary 描述符 + 专用 token，让查看器
 *    经字节流端点取真实字节而非 SDK 的占位文本。
 */

/**
 * POST /omp/uri/resolve handler core (public path /api/omp/uri/resolve).
 *
 * Body: { scheme, ref, sessionID?, directory?, pathOnly? } or { u, ... }.
 *  - scheme ∉ uriCapabilities().read → 501 {error:'scheme-not-enabled'}
 *    (R2-H2/R2-M11 — agent/history/artifact/mcp/ssh/... all land here).
 *  - unknown scheme (no native handler; file/http/https included) →
 *    404 {error:'unknown-scheme'} — the MCP fallback is never exposed.
 *  - local:// requires sessionID (400 session-required) + directory;
 *    resolution is pinned per-request, never via global state.
 *  - Response = InternalResource minus sourcePath, plus an opaque token.
 *    Traversal/not-found errors surface the handler's own message (404);
 *    missing-options (should be unreachable) → 409 per §5.2.3.
 *
 * @param {{ body: object, localOptionsFor: (sessionID: string, directory: string) => object | null | Promise<object | null>,
 *           tokens: UriTokenService, router?: object }} input
 */
export const handleUriResolve = async ({ body = {}, localOptionsFor, tokens, router }: UriResolveInput): Promise<Response> => {
  const resolveRouter = router ?? InternalUrlRouter.instance();
  const u =
    typeof body.u === 'string' && body.u.length > 0
      ? body.u
      : `${String(body.scheme ?? '').toLowerCase()}://${String(body.ref ?? '')}`;
  if (typeof u !== 'string' || u.length === 0 || !/^[a-z][a-z0-9+.-]*:\/\//i.test(u)) {
    return json({ error: 'invalid-url' }, { status: 400 });
  }
  if (u.length > MAX_URL_LENGTH) {
    return json({ error: 'url-too-long', limit: MAX_URL_LENGTH }, { status: 400 });
  }
  // The regex is a prefix match on an already length-checked string, so
  // the match always exists here; guard for the type checker anyway.
  const schemeMatch = u.match(/^[a-z][a-z0-9+.-]*/i);
  if (!schemeMatch) return json({ error: 'invalid-url' }, { status: 400 });
  const scheme = schemeMatch[0].toLowerCase();
  if (!resolveRouter.canHandle(u)) {
    // Registered handlers only — the MCP resource fallback is deliberately
    // not exposed (§5.2.1: 未知 scheme → 404 unknown-scheme).
    return json({ error: 'unknown-scheme', scheme }, { status: 404 });
  }
  if (!ENABLED_READ_SCHEMES.includes(scheme)) {
    return json({ error: 'scheme-not-enabled', scheme }, { status: 501 });
  }
  const sessionID = typeof body.sessionID === 'string' ? body.sessionID : '';
  const directory = typeof body.directory === 'string' ? body.directory : '';
  if (!sessionID) return json({ error: 'session-required' }, { status: 400 });
  if (!directory) return json({ error: 'directory-required' }, { status: 400 });

  const localProtocolOptions = await localOptionsFor(sessionID, directory);
  if (!localProtocolOptions) return json({ error: 'session-not-found' }, { status: 404 });

  let resource: InternalResource;
  try {
    resource = await resolveRouter.resolve(u, {
      localProtocolOptions,
      ...(body.pathOnly ? { pathOnly: true } : {}),
    });
  } catch (error) {
    const message = String(errorText(error));
    if (message.includes('No session')) {
      return json({ error: 'no-session', message }, { status: 409 });
    }
    // Handler-owned containment/not-found errors (e.g. "Path traversal (..)
    // is not allowed in local:// URLs", "Local file not found: ...") — the
    // endpoint never pre-rewrites paths, so the handler text is the contract.
    return json({ error: 'resolve-failed', message }, { status: 404 });
  }
  const size = Buffer.byteLength(String(resource.content ?? ''), 'utf8');
  if (size > MAX_INLINE_BYTES) {
    return json({ error: 'too-large', size }, { status: 413 });
  }
  // R7: sourcePath never leaves the process. It exists only inside the token.
  const { sourcePath, ...safe } = resource;
  // Previewable binaries (local:// image fallback): the SDK handler answers
  // with a placeholder text body; swap the response to a binary descriptor —
  // real mime + real size + token — so the viewer streams bytes via the
  // token content endpoint instead of rendering the placeholder (§5.2.4).
  const binaryMime = typeof sourcePath === 'string' ? BINARY_MIME_BY_EXT.get(extensionOf(sourcePath)) : undefined;
  if (binaryMime && typeof sourcePath === 'string') {
    const stat = await fs.stat(sourcePath).catch((): null => null);
    const binaryToken = tokens.issue({
      resourceUrl: resource.url,
      directory,
      sessionID,
      contentType: binaryMime,
      size: stat?.size,
      immutable: true,
      absolutePath: sourcePath,
    });
    return json({
      url: resource.url,
      contentType: binaryMime,
      size: stat?.size ?? 0,
      immutable: true,
      binary: true,
      notes: resource.notes ?? [],
      token: binaryToken,
    });
  }
  const token =
    typeof sourcePath === 'string' && sourcePath.length > 0
      ? tokens.issue({
          resourceUrl: resource.url,
          directory,
          sessionID,
          contentType: resource.contentType,
          size: resource.size,
          immutable: resource.immutable,
          absolutePath: sourcePath,
        })
      : undefined;
  return json({ ...safe, ...(token ? { token } : {}) });
};

// ---------------------------------------------------------------------------
// §5.4 session tree
// ---------------------------------------------------------------------------

/** 引擎注册表输出的 wire 会话记录：id/title/time 来自
 * SessionManager.list 加 SessionMetaRegistry 边车（registry.js）。 */
export interface WireSessionRecord {
  /** 会话 id（目录内唯一）。 */
  id: string;
  /** 会话标题（可选）。 */
  title?: string;
  /** 子代理意义上的父会话（保留字段，会话树不读它）。 */
  parentID?: string;
  /** fork 谱系父（registry forkParentID）：会话树读这个而非 parentID。 */
  /** Fork lineage (registry forkParentID): the wire parentID stays reserved
   * for subagent sessions; the session tree reads this instead. */
  forkParentID?: string;
  /** 非冷的活跃注册表状态；缺省 = 冷（无驻留）。 */
  /** Non-cold live-registry state; absent = cold (nothing resident). */
  live?: string;
  /** transcript 文件字节数 —— 会话内存成本的代理值。 */
  /** Transcript file bytes — the session's memory-cost proxy. */
  transcriptBytes?: number;
  /** 创建/更新时间戳（ms，可选）。 */
  time?: { created?: number; updated?: number };
}

/** 会话树数据源类型：wire 记录数组、{sessions} 包裹形态或 null/undefined
 * （视为空）；兼容 engine.listSessions 的多种返回形状。 */
export type SessionTreeData = WireSessionRecord[] | { sessions?: WireSessionRecord[] } | null | undefined;

/** 会话树投影的单个节点：扁平数组元素，UI 按 parentId 自行组装层级。 */
export interface SessionTreeNodeProjection {
  /** 会话 id。 */
  id: string;
  /** 谱系父 id；缺失（其它目录/已清理）或成环被切时为 null。 */
  parentId: string | null;
  /** 展示标题（缺省 'Untitled'）。 */
  title: string;
  /** 创建/更新时间戳（ms，缺省 0）。 */
  time: { created: number; updated: number };
}

/** 目录级会话树投影：{leafId, nodes}。leafId 是最近更新的会话
 * （目录的活跃叶子），nodes 为扁平节点数组。 */
export interface SessionTreeProjection {
  /** 最近更新的会话 id；空目录为 null。 */
  leafId: string | null;
  /** 全部节点（扁平；UI 按 parentId 组装层级）。 */
  nodes: SessionTreeNodeProjection[];
}

/**
 * 由引擎注册表数据构建目录级会话树（fork/父谱系图）。输入是
 * engine.listSessions({directory}) 产出的 wire 会话记录 —— id/title/time
 * 来自 SessionManager.list 加 SessionMetaRegistry 边车（registry.js），
 * forkParentID 来自 fork 谱系（engine.fork 写入 registry）。wire 的
 * parentID 是子代理从属关系而非谱系，这里刻意忽略 —— 子代理不得在
 * fork 树上另开分支。
 *
 * 形状：{leafId, nodes: [{id, parentId, title, time}]} —— 扁平数组，
 * UI 按 parentId 组装层级。leafId = 最近更新的会话（目录活跃叶子）。
 * 集合中缺失的父（其它目录、已清理 transcript）解析为 null；
 * 谱系成环会被切断（防边车被手改导致投影卡死）。
 */

/**
 * Directory-level session tree (fork/parent graph) from engine registry
 * data. Input = wire session records as produced by engine.listSessions
 * ({directory}) — id/title/time come from SessionManager.list plus the
 * SessionMetaRegistry sidecar (registry.js), forkParentID from fork lineage
 * (engine.fork writes registry forkParentID). Wire `parentID` is subagent
 * parentage, not lineage, and is ignored here — subagent children must not
 * sprout fork-tree branches.
 *
 * Shape: { leafId, nodes: [{id, parentId, title, time}] } — flat array, UI
 * builds the hierarchy by parentId. leafId = most recently updated session
 * (the directory's active leaf). Parents missing from the set (other
 * directories, pruned transcripts) resolve to null; parent cycles are cut.
 *
 * @param {Array<object> | { sessions: Array<object> }} engineRegistryData
 */
export const buildSessionTree = (engineRegistryData: SessionTreeData): SessionTreeProjection => {
  const sessions = Array.isArray(engineRegistryData)
    ? engineRegistryData
    : Array.isArray(engineRegistryData?.sessions)
      ? engineRegistryData.sessions
      : [];
  const byId = new Map(sessions.map((s) => [s.id, s]));
  const nodes = sessions
    .slice()
    .sort((a, b) => (a.time?.created ?? 0) - (b.time?.created ?? 0))
    .map((session) => {
      const parentId =
        typeof session.forkParentID === 'string' && byId.has(session.forkParentID) ? session.forkParentID : null;
      return {
        id: session.id,
        parentId,
        title: session.title ?? 'Untitled',
        time: {
          created: session.time?.created ?? 0,
          updated: session.time?.updated ?? 0,
        },
      };
    });
  // Cycle cut: if a node's ancestor chain revisits an id, drop its parent
  // edge (defensive — fork metadata is append-only, but the sidecar is
  // user-editable state and a corrupt cycle must not wedge the projection).
  const nodeById = new Map(nodes.map((n) => [n.id, n]));
  for (const node of nodes) {
    const seen = new Set([node.id]);
    let cursor = node.parentId ? nodeById.get(node.parentId) : null;
    while (cursor) {
      if (seen.has(cursor.id)) {
        node.parentId = null;
        break;
      }
      seen.add(cursor.id);
      cursor = cursor.parentId ? nodeById.get(cursor.parentId) : null;
    }
  }
  let leafId: string | null = null;
  let latest = -1;
  for (const node of nodes) {
    if (node.time.updated > latest) {
      latest = node.time.updated;
      leafId = node.id;
    }
  }
  return { leafId, nodes };
};


/**
 * 单个会话的 fork 谱系子树（GET /omp/sessions/{id}/tree 的 handler
 * 核心）：{sessionID} 的祖先链加上全部后代 fork —— 即"该会话所属的
 * 那棵树"。节点形状与 buildSessionTree 相同；leafId = 谱系中最近更新
 * 的会话。会话不在注册表/列表数据中时返回 null（→ 404）。
 */

/**
 * Fork-lineage subtree for one session (GET /omp/sessions/{id}/tree handler
 * core): the ancestor chain of {sessionID} plus every descendant fork — the
 * "tree this session belongs to". Same node shape as buildSessionTree;
 * leafId = most recently updated session in the lineage. Returns null when
 * the session is unknown to the registry/listing data (→ 404).
 *
 * @param {string} sessionID
 * @param {Array<object> | { sessions: Array<object> }} engineRegistryData
 */
export const buildSessionSubtree = (sessionID: string, engineRegistryData: SessionTreeData): SessionTreeProjection | null => {
  const full = buildSessionTree(engineRegistryData);
  const nodeById = new Map(full.nodes.map((node) => [node.id, node]));
  const target = typeof sessionID === 'string' ? nodeById.get(sessionID) : undefined;
  if (!target) return null;
  const lineage = new Set([target.id]);
  let cursor = target.parentId ? nodeById.get(target.parentId) : null;
  while (cursor) {
    lineage.add(cursor.id);
    cursor = cursor.parentId ? nodeById.get(cursor.parentId) : null;
  }
  const childrenOf = new Map<string, string[]>();
  for (const node of full.nodes) {
    if (!node.parentId) continue;
    const siblings = childrenOf.get(node.parentId) ?? [];
    siblings.push(node.id);
    childrenOf.set(node.parentId, siblings);
  }
  const pending = [target.id];
  while (pending.length > 0) {
    const next = pending.pop();
    if (next === undefined) break;
    for (const child of childrenOf.get(next) ?? []) {
      if (!lineage.has(child)) {
        lineage.add(child);
        pending.push(child);
      }
    }
  }
  const nodes = full.nodes.filter((node) => lineage.has(node.id));
  let leafId: string | null = null;
  let latest = -1;
  for (const node of nodes) {
    if (node.time.updated > latest) {
      latest = node.time.updated;
      leafId = node.id;
    }
  }
  return { leafId, nodes };
};
/** 把文本压成单行预览：折叠空白、截断到 limit（默认 80）并补省略号；
 * null/undefined 归一为空串。供 entry 树 gist 使用。 */
const previewOf = (text: string | null | undefined, limit: number = 80): string => {
  const flat = String(text ?? '').replace(/\s+/g, ' ').trim();
  return flat.length > limit ? `${flat.slice(0, limit)}…` : flat;
};

/** SessionManager entry 的结构子集（树投影只关心这些字段）：基础
 * {type, id, parentId, timestamp} 加按类型出现的可选字段。 */
export interface EntryLike {
  /** entry 类型（message/branch_summary/mode_change/label/...）。 */
  type: string;
  /** entry id（会话内唯一）。 */
  id: string;
  /** 树结构上的父 entry id（可选）。 */
  parentId?: string | null;
  /** ISO 时间戳（可选）。 */
  timestamp?: string;
  /** message 类型专有：role + content（块数组或纯文本）。 */
  message?: { role?: unknown; content?: unknown };
  /** branch_summary 类型专有：摘要文本。 */
  summary?: string | null;
  /** mode_change 类型专有：切换后的模式。 */
  mode?: unknown;
  /** label 类型专有：被改标签的目标 entry id。 */
  targetId?: string;
  /** label 类型专有：新标签文本。 */
  label?: string;
}

/** getTree() 返回的树节点形状：entry + children + 已解析 label。 */
export interface EntryTreeNodeLike {
  /** 本节点对应的 entry（label entry 不产生节点）。 */
  entry?: EntryLike;
  /** 子节点数组（缺省为空）。 */
  children?: EntryTreeNodeLike[];
  /** 折叠了后续 label 变更后的节点标签。 */
  label?: string;
}

/** 树投影所需的 SessionManager 最小接口（结构化鸭子类型）：三个
 * 访问器全部可选，缺失时投影按空树/0/null 处理。 */
export interface EntryTreeManagerLike {
  /** 返回带层级的 entry 树（含折叠后的 label）。 */
  getTree?(): EntryTreeNodeLike[];
  /** 返回扁平 entry 数组；长度即 revision。 */
  getEntries?(): unknown[];
  /** 当前叶子 entry id（无则 null）。 */
  getLeafId?(): string | null;
}

/** 单个树节点按 entry 类型透出的"要点"字段：message 给 role + 预览、
 * 工具块给 toolName、模式切换给 mode；无可展示内容的类型整体缺省。 */

/** Entry-type-specific fields surfaced for one tree node ("gist"): role +
 *  preview for messages, toolName for tool-call blocks, mode for mode
 *  changes; absent entirely for types with nothing to show. */
export interface EntryNodeGist {
  /** 消息角色（user/assistant/...）。 */
  role?: unknown;
  /** 内容块里第一个工具调用的名称（如有）。 */
  toolName?: string;
  /** 压平截断后的文本预览。 */
  preview?: string;
  /** mode_change 后的模式。 */
  mode?: unknown;
}

/** entry 树快照中的单个节点：定位字段 + 可选 label 与类型要点 gist。 */
export interface EntrySnapshotNode {
  /** entry id。 */
  id: string;
  /** 父节点 id（根为 null）。 */
  parentId: string | null;
  /** entry 类型。 */
  type: string;
  /** ISO 时间戳（可选）。 */
  timestamp?: string;
  /** 已解析标签（有才出现）。 */
  label?: string;
  /** 类型要点（有才出现，见 EntryNodeGist）。 */
  gist?: EntryNodeGist;
}

/** GET /omp/sessions/{sessionID}/tree 的完整快照（§5.4.1 只读视图）。 */
export interface EntryTreeSnapshot {
  /** 会话 id。 */
  sessionID: string;
  /** 作用域目录（可为 null）。 */
  directory: string | null;
  /** 当前叶子 entry id（无则 null）。 */
  leafId: string | null;
  /** 根 → 叶 的 entry id 路径（展开定位用）。 */
  pathToLeaf: string[];
  /** 修订号 = entry 总数（getEntries().length，缺省用节点数）。 */
  revision: number;
  /** 扁平节点数组（label entry 不产生节点）。 */
  nodes: EntrySnapshotNode[];
}

/**
 * 构建单会话 entry 树快照（§5.4.1 只读视图），服务于
 * GET /omp/sessions/{sessionID}/tree。对 SessionManager 形状的对象做
 * 纯投影 —— 引擎传入活跃的 agentSession.sessionManager，或冷开的只读
 * SessionManager.open()（绝不物化 agent）。
 *
 * label entry 不产生节点（它只改目标节点的已解析 label，而 getTree()
 * 已把该 label 折叠进 node.label）。
 */

/**
 * Per-session entry-tree snapshot (§5.4.1 read-only view) for
 * GET /omp/sessions/{sessionID}/tree. Pure projection over a
 * SessionManager-like object — the engine passes the live
 * agentSession.sessionManager or a cold read-only SessionManager.open()
 * (never materializes an agent).
 *
 * Label entries produce no node (they only mutate the target's resolved
 * label, which getTree() already folds into node.label).
 *
 * @param {{ sessionID: string, directory: string, manager: {
 *   getTree(): Array<{entry: object, children: Array, label?: string}>,
 *   getEntries(): Array<object>, getLeafId(): string | null } }} input
 */
export const buildEntryTreeSnapshot = ({ sessionID, directory, manager }: {
  sessionID: string;
  directory: string | null;
  manager: EntryTreeManagerLike;
}): EntryTreeSnapshot => {
  const treeNodes = manager.getTree?.() ?? [];
  const nodes: EntrySnapshotNode[] = [];
  // 深度优先遍历：非 label 的 entry 产出节点并提取类型化 gist，随后递归 children。
  const walk = (node: EntryTreeNodeLike) => {
    const entry = node?.entry;
    if (entry && entry.type !== 'label') {
      const gist: EntryNodeGist = {};
      if (entry.type === 'message') {
        const message: { role?: unknown; content?: unknown } = entry.message ?? {};
        gist.role = message.role;
        const blocks = Array.isArray(message.content) ? message.content : [];
        const toolBlock = blocks.find((b) => b && typeof b.name === 'string');
        if (toolBlock) gist.toolName = toolBlock.name;
        const text = typeof message.content === 'string'
          ? message.content
          : blocks.filter((b) => b?.type === 'text').map((b) => b.text).join('');
        gist.preview = previewOf(text);
      } else if (entry.type === 'branch_summary') {
        gist.preview = previewOf(entry.summary);
      } else if (entry.type === 'mode_change') {
        gist.mode = entry.mode ?? null;
      }
      nodes.push({
        id: entry.id,
        parentId: entry.parentId ?? null,
        type: entry.type,
        timestamp: entry.timestamp,
        ...(node.label !== undefined ? { label: node.label } : {}),
        ...(Object.keys(gist).length > 0 ? { gist } : {}),
      });
    }
    for (const child of node?.children ?? []) walk(child);
  };
  for (const root of treeNodes) walk(root);
  const byId = new Map(nodes.map((n) => [n.id, n]));
  const leafId = manager.getLeafId?.() ?? null;
  const pathToLeaf: string[] = [];
  let cursor = leafId ? byId.get(leafId) : null;
  while (cursor) {
    pathToLeaf.unshift(cursor.id);
    cursor = cursor.parentId ? byId.get(cursor.parentId) : null;
  }
  return {
    sessionID,
    directory,
    leafId,
    pathToLeaf,
    revision: manager.getEntries?.().length ?? nodes.length,
    nodes,
  };
};

/** §5.2/§5.4 的事件名登记在 omp-event-registry.json（第五章拥有通道；
 * 本模块只是生产者）。payload 不重复信封已带的 directory/sessionID。 */

/** §5.2/§5.4 event names live in omp-event-registry.json (chapter 05 owns
 *  the channel; this module is the producer). Payload never repeats the
 *  envelope's directory/sessionID. */
export const OMP_AGENTS_UPDATED = 'omp.agents.updated';
/** 会话树变更事件名（navigate/label/summary 提交后由引擎发布）。 */
export const OMP_TREE_UPDATED = 'omp.tree.updated';

/** D04-4：流式输出中的会话拒绝 navigate，返回 409 {busy:true}。 */

/** D04-4: streaming sessions reject navigate with 409 {busy:true}. */
export const navigateBusyResponse = () => json({ busy: true }, { status: 409 });

/** navigateTree 的原始请求体（字段全部可选，由 normalize 校验收窄）。 */
export interface NavigateRequestRaw {
  /** 跳转目标 entry id。 */
  targetId?: string;
  /** 是否先做分支摘要。 */
  summarize?: boolean;
  /** 附带给摘要器的自定义指令。 */
  customInstructions?: string;
  /** 是否允许重开 ask 分支。 */
  allowAskReopen?: boolean;
  /** 重答 ask 的既有结果（形如 {content: 块数组}）。 */
  reanswerAskResult?: unknown;
}

/** 校验归一后的 navigateTree 请求：全部字段补齐默认值。 */
export interface NavigateRequest {
  /** 跳转目标 entry id（必填）。 */
  targetId: string;
  /** 是否先做分支摘要（布尔化）。 */
  summarize: boolean;
  /** 自定义指令；非字符串归一为 null。 */
  customInstructions: string | null;
  /** 是否允许重开 ask 分支（缺省 true：web 端提供 ask 桥）。 */
  allowAskReopen: boolean;
  /** 重答结果；缺省 null。 */
  reanswerAskResult: unknown;
}

/** navigateTree 校验结果判别联合：ok:true 携带归一值；ok:false 携带
 * 可直接返回的错误 Response。 */
export type NavigateRequestResult =
  | { ok: true; value: NavigateRequest; response?: undefined }
  | { ok: false; response: Response; value?: undefined };

/**
 * navigateTree request contract (§5.4.2). Engine hook: validate with this,
 * then `agentSession.navigateTree(targetId, options)` (agent-session.ts:8273)
 * and publish OMP_TREE_UPDATED. Not wired here.
 * @returns {{ ok: true, value: object } | { ok: false, response: Response }}
 */
/** navigateTree 请求契约（§5.4.2）。引擎钩子：先用本函数校验，再调
 * agentSession.navigateTree(targetId, options)（agent-session.ts:8273）
 * 并发布 OMP_TREE_UPDATED。此处不接线。
 * @returns 校验通过返回归一值；失败返回 400 Response。 */

export const normalizeNavigateRequest = (raw: NavigateRequestRaw = {}): NavigateRequestResult => {
  if (typeof raw.targetId !== 'string' || raw.targetId.length === 0) {
    return { ok: false, response: json({ error: 'target-required' }, { status: 400 }) };
  }
  if (raw.reanswerAskResult !== null && raw.reanswerAskResult !== undefined) {
    const r = raw.reanswerAskResult;
    if (typeof r !== 'object' || !('content' in r) || !Array.isArray(r.content)) {
      return { ok: false, response: json({ error: 'invalid-reanswer' }, { status: 400 }) };
    }
  }
  return {
    ok: true,
    value: {
      targetId: raw.targetId,
      summarize: Boolean(raw.summarize),
      customInstructions: typeof raw.customInstructions === 'string' ? raw.customInstructions : null,
      // Web ships the ask bridge (chapter 03), so reopen stays enabled.
      allowAskReopen: raw.allowAskReopen === undefined ? true : Boolean(raw.allowAskReopen),
      reanswerAskResult: raw.reanswerAskResult ?? null,
    },
  };
};

/** label 请求校验结果判别联合：ok:true 携带 {targetId, label}；
 * ok:false 携带可直接返回的错误 Response。 */
export type LabelRequestResult =
  | { ok: true; value: { targetId: string; label: unknown }; response?: undefined }
  | { ok: false; response: Response; value?: undefined };

/** label 变更契约（§5.4.2）：{targetId, label?} —— label === undefined
 * 表示清除标签。引擎钩子：manager.appendLabelChange(targetId, label)
 * （session-manager.ts:2397；冷 SessionManager 上同样可用）。此处不接线。 */

/**
 * Label change contract (§5.4.2): {targetId, label?} — `label === undefined`
 * clears. Engine hook: `manager.appendLabelChange(targetId, label)`
 * (session-manager.ts:2397; works on cold SessionManagers too). Not wired.
 */
export const normalizeLabelRequest = (raw: { targetId?: string; label?: unknown } = {}): LabelRequestResult => {
  if (typeof raw.targetId !== 'string' || raw.targetId.length === 0) {
    return { ok: false, response: json({ error: 'target-required' }, { status: 400 }) };
  }
  if (raw.label !== undefined && typeof raw.label !== 'string') {
    return { ok: false, response: json({ error: 'invalid-label' }, { status: 400 }) };
  }
  return { ok: true, value: { targetId: raw.targetId, label: raw.label } };
};

/** omp.tree.updated 的 payload 形状：轻量增量，客户端收到后重拉
 * GET /omp/sessions/{id}/tree。 */
export interface TreeUpdatedPayload {
  /** 变更后的叶子 entry id（可为 null）。 */
  leafId: string | null;
  /** 变更种类：navigate | label | summary。 */
  kind: string;
  /** 相关 entry id（如有）。 */
  entryId?: string;
}

/**
 * omp.tree.updated payload contract (§5.0/§5.4): light delta — the client
 * re-pulls GET /omp/sessions/{id}/tree on receipt. Engine publishes with
 * scope {directory, sessionID, durable: true} after navigateTree/label
 * commits. Not wired here.
 */
/** omp.tree.updated 的 payload 构造（§5.0/§5.4）：轻量增量 —— 客户端
 * 收到后自行重拉 GET /omp/sessions/{id}/tree。引擎在 navigateTree/label
 * 提交后以 {directory, sessionID, durable: true} 范围发布。此处不接线。
 * @throws {TypeError} kind 不是 navigate|label|summary 时抛出。 */

export const treeUpdatedPayload = ({ leafId, kind, entryId }: { leafId?: string | null; kind?: string; entryId?: string } = {}): TreeUpdatedPayload => {
  if (kind !== 'navigate' && kind !== 'label' && kind !== 'summary') {
    throw new TypeError('treeUpdatedPayload: kind must be navigate|label|summary');
  }
  return {
    leafId: leafId ?? null,
    kind,
    ...(entryId ? { entryId } : {}),
  };
};

// ---------------------------------------------------------------------------
// §5.5 AgentRunsAggregator + parked/historical split (R2-M5)
// ---------------------------------------------------------------------------

/** 行状态全集：SDK 的 AgentStatus 加上 omp-host 独有的 historical
 * （冷扫描行：仅可看 transcript，进程内永不可复活）。 */

/** Row statuses — SDK AgentStatus plus the omp-host-only `historical`
 *  (cold-scan rows: transcript-view only, never revivable in-process). */
export const AGENT_RUN_STATUSES = ['running', 'idle', 'parked', 'aborted', 'historical'] as const;
/** 单个 agent run 行的状态字面量联合类型。 */
export type AgentRunStatus = (typeof AGENT_RUN_STATUSES)[number];

/** 行排序权重：running 最前、historical 最后；未知状态在比较时按 9 垫底。 */
const STATUS_ORDER = { running: 0, idle: 1, parked: 2, aborted: 3, historical: 4 } satisfies Record<AgentRunStatus, number>;

/**
 * 行主键：与 LiveSessionRegistry 相同的复合身份（plan §3.1）——
 * 会话 id 只在目录内唯一、不在进程内唯一；同一 id 在两个 worktree
 * 下物化时绝不能互相覆盖行、描述符或释放。
 * 格式：normalizeDirectoryKey(directory) + NUL + sessionID + '::' + agentId。
 */

/**
 * Rows key on the SAME composite identity as LiveSessionRegistry (plan
 * §3.1): session ids are unique per directory, not per process — the same
 * id materialized under two worktrees must never overwrite each other's
 * row, descriptor, or release.
 */
const agentRunKey = (directory: string, sessionID: string, agentId: string): string =>
  `${normalizeDirectoryKey(directory)}\u0000${sessionID}::${agentId}`;

/** 某个 (directory, sessionID) 拥有的全部行/描述符的 key 前缀
 * （释放时做前缀匹配删除）。 */
const agentRunSessionPrefix = (directory: string, sessionID: string): string =>
  `${normalizeDirectoryKey(directory)}\u0000${sessionID}::`;

/** SDK AgentRef.history 的结构子集：运行结束时的归档信息（模型、
 * 指标、产物路径等）；投影时会剥掉绝对路径字段。 */
export interface AgentRunHistory {
  /** 代理显示名。 */
  agent?: string;
  /** 模型角色。 */
  modelRole?: string;
  /** 实际解析到的模型 id。 */
  resolvedModel?: string;
  /** 结束时的指标快照（原样透传）。 */
  metrics?: unknown;
  /** 是否只读运行。 */
  readOnly?: boolean;
  /** 产物路径（保留 agent:// URL 形式）。 */
  outputPath?: string;
  /** 补丁路径（worktree 路径，投影时丢弃）。 */
  patchPath?: string;
  /** worktree 分支名（投影时丢弃）。 */
  branchName?: string;
}

/** SDK AgentRef（agent-registry.ts:72-97）的结构子集：私有注册表
 * list() 返回的运行中/驻留代理引用。 */
export interface AgentRefLike {
  /** 代理 id（会话内唯一）。 */
  id: string;
  /** 展示名。 */
  displayName?: string;
  /** 代理种类（投影缺省 'sub'）。 */
  kind?: string;
  /** 父代理 id（如有）。 */
  parentId?: string;
  /** SDK 状态：running | idle | parked | aborted。 */
  status?: string;
  /** 运行中的活跃 AgentSession（SDK：parked/aborted 时恰为 null）。
   * 此处结构化收窄到快照所需的唯一公共访问器。 */
  /**
   * Live AgentSession while the agent is running (SDK: null exactly when
   * parked/aborted). Structurally narrowed to the public stats accessor —
   * the only surface the snapshot needs.
   */
  session?: {
    /** 快照指标访问器：tokens.total 与 cost。 */
    getSessionStats?: () => {
      tokens?: { total?: number };
      cost?: number;
    };
    /** 子会话自身的 id（只读下钻按它解析）。 */
    /** The child session's own id (read-only drill-in resolves by it). */
    sessionId?: string;
  } | null;
  /** transcript 文件路径（仅用于推断 hasTranscript / childSessionID）。 */
  sessionFile?: string | null;
  /** 创建时间戳（ms）。 */
  createdAt?: number;
  /** 最近活动时间戳（ms）。 */
  lastActivity?: number;
  /** 当前活动描述文本。 */
  activity?: string;
  /** 结束归档信息（见 AgentRunHistory）。 */
  history?: AgentRunHistory;
}

/** 运行中代理在快照时点的指标；parked/aborted 后不再出现。 */

/** Snapshot-time metrics for a running agent; absent once parked/aborted. */
export interface OmpAgentRunLive {
  /** 累计 token 数。 */
  tokens: number;
  /** 累计成本。 */
  cost: number;
  /** 运行时长（ms）= lastActivity - createdAt，非负。 */
  durationMs: number;
}

/** agent-runs 行（§5.5.1）：SDK AgentRef 的对外投影。R7：行内不带任何
 * 绝对路径 —— sessionFile 塌缩成 hasTranscript，history 剥掉
 * patchPath/branchName（worktree 路径），outputPath 只保留 agent:// URL。 */
export interface OmpAgentRun {
  /** 复合主键（见 agentRunKey）。 */
  key: string;
  /** 拥有者会话 id。 */
  sessionID: string;
  /** 归一化后的作用域目录。 */
  directory: string;
  /** 代理 id。 */
  agentId: string;
  /** 展示名（缺省 agentId）。 */
  displayName: string;
  /** 代理种类（缺省 'sub'）。 */
  kind: string;
  /** 父代理 id（如有）。 */
  parentId?: string;
  /** AGENT_RUN_STATUSES 之一。 */
  status: string;
  /** 创建时间戳（ms）。 */
  createdAt: number;
  /** 最近活动时间戳（ms）。 */
  lastActivity: number;
  /** 该 run 自身的会话 id —— 可解析时才出现（活跃会话或已预热的缓存）。 */
  /** The run's own session id — set when resolvable (live session or warmed cache). */
  childSessionID?: string;
  /** 当前活动描述文本。 */
  activity?: string;
  /** 结束归档信息（路径字段已脱敏）。 */
  history?: AgentRunHistory;
  /** 运行时指标（仅 running 行携带）。 */
  live?: OmpAgentRunLive;
  /** 是否存在 transcript 文件（sessionFile 的布尔投影）。 */
  hasTranscript: boolean;
}

/**
 * 把一个 SDK AgentRef（agent-registry.ts:72-87）投影为 OmpAgentRun 行
 * （§5.5.1）。R7：行内不携带任何绝对路径 —— sessionFile 塌缩为
 * hasTranscript，history 丢弃 patchPath/branchName（worktree 路径），
 * outputPath 只保留 agent:// URL 形式。live 指标仅在能从 session 取到
 * 有限 token 数时附加。
 */

/**
 * Project one SDK AgentRef (agent-registry.ts:72-87) into an OmpAgentRun row
 * (§5.5.1). R7: the row carries NO absolute paths — sessionFile collapses to
 * hasTranscript, history drops patchPath/branchName (worktree paths), and
 * outputPath keeps only its agent:// URL form.
 */
export const projectAgentRun = ({ sessionID, directory, ref, status, childSessionIdFor }: {
  sessionID: string;
  directory: string;
  ref: AgentRefLike;
  status?: string;
  /** 同步的 transcript 头缓存查找（引擎侧）：为 parked 行补 childSessionID。 */
  /** Sync transcript-header cache lookup (engine): childSessionID for parked runs. */
  childSessionIdFor?: (sessionFile: string) => string | undefined;
}): OmpAgentRun => {
  // Live sessions carry their id; parked runs resolve through the engine's
  // warmed transcript-header cache (immutable identity, one open per file).
  const childSessionID = ref.session?.sessionId ?? (ref.sessionFile ? childSessionIdFor?.(ref.sessionFile) : undefined);
  const history = ref.history
    ? {
        ...(ref.history.agent !== undefined ? { agent: ref.history.agent } : {}),
        ...(ref.history.modelRole !== undefined ? { modelRole: ref.history.modelRole } : {}),
        ...(ref.history.resolvedModel !== undefined ? { resolvedModel: ref.history.resolvedModel } : {}),
        ...(ref.history.metrics ? { metrics: ref.history.metrics } : {}),
        ...(ref.history.readOnly !== undefined ? { readOnly: ref.history.readOnly } : {}),
        ...(ref.history.outputPath !== undefined ? { outputPath: ref.history.outputPath } : {}),
      }
    : undefined;
  // Invoke on the session: the SDK accessor is a class method reading #stats —
  // extracting it and calling bare leaves this === undefined and throws.
  const stats = ref.session?.getSessionStats?.();
  const liveTokens = stats?.tokens?.total;
  const liveCost = stats?.cost;
  const row: OmpAgentRun = {
    key: agentRunKey(directory, sessionID, ref.id),
    sessionID,
    directory,
    agentId: ref.id,
    displayName: ref.displayName ?? ref.id,
    kind: ref.kind ?? 'sub',
    ...(ref.parentId ? { parentId: ref.parentId } : {}),
    status: status ?? ref.status ?? 'historical',
    createdAt: ref.createdAt ?? 0,
    lastActivity: ref.lastActivity ?? 0,
    ...(childSessionID ? { childSessionID } : {}),
    ...(ref.activity ? { activity: ref.activity } : {}),
    ...(history && Object.keys(history).length > 0 ? { history } : {}),
    hasTranscript: Boolean(ref.sessionFile),
  };
  if (stats && liveTokens !== undefined && Number.isFinite(liveTokens)) {
    row.live = {
      tokens: liveTokens,
      cost: liveCost !== undefined && Number.isFinite(liveCost) ? liveCost : 0,
      durationMs: Math.max(0, (ref.lastActivity ?? 0) - (ref.createdAt ?? 0)),
    };
  }
  return row;
};

/** agentsSnapshot 钩子的单条目：一个活跃 host 会话及其私有
 * AgentRegistry（refs 直给数组，或经 registry.list() 取）。 */
export interface AgentsSnapshotEntry {
  /** 拥有者会话 id。 */
  sessionID: string;
  /** 作用域目录（缺省按空串归一化）。 */
  directory?: string;
  /** 代理引用数组（与 registry 二选一）。 */
  refs?: AgentRefLike[];
  /** 会话私有注册表（list() 返回 AgentRef[]）。 */
  registry?: { list(): AgentRefLike[] };
}

/** 冷扫描行：引擎对目录 artifacts 账本的文件系统扫描结果（§5.2.3）；
 * 进入快照时状态固定为 historical。 */
export interface DiskScanRow {
  /** 拥有者会话 id。 */
  sessionID: string;
  /** 代理 id。 */
  agentId: string;
  /** 作用域目录。 */
  directory?: string;
  /** 展示名。 */
  displayName?: string;
  /** 代理种类。 */
  kind?: string;
  /** 父代理 id（如有）。 */
  parentId?: string;
  /** 是否有 transcript（缺省 true）。 */
  hasTranscript?: boolean;
  /** 该 run 自身的会话 id（transcript 头）—— 只读下钻的目标。 */
  /** The run's own session id (transcript header) — read-only drill-in target. */
  childSessionID?: string;
  /** 创建时间戳（ms）。 */
  createdAt?: number;
  /** 最近活动时间戳（ms）。 */
  lastActivity?: number;
}

/** 事件发布作用域：目录 + 是否持久（durable）。 */
export interface AgentRunsPublishScope {
  /** 目标目录。 */
  directory: string;
  /** true = 事件可持久化重放（omp-event-registry 约定）。 */
  durable?: boolean;
}

/** GET /omp/agent-runs 的快照：行 + 生成时间 + 修订号。 */
export interface AgentRunsSnapshot {
  /** 行数组（已按状态/最近活动排序）。 */
  agentRuns: OmpAgentRun[];
  /** 快照生成时间戳（ms）。 */
  generatedAt: number;
  /** 单调递增修订号（每次 refresh +1）。 */
  revision: number;
}

/** omp.agents.updated 的 payload（§5.5.1）：单个脏目录合并后的全量行
 * 快照 + 修订号。 */

/** omp.agents.updated event payload (§5.5.1): the coalesced full-row
 *  snapshot + revision for one dirty directory. */
export interface AgentsUpdatedPayload {
  /** 该目录的全部行（替换式语义，客户端不得增量合并）。 */
  agentRuns: OmpAgentRun[];
  /** 对应快照的修订号。 */
  revision: number;
}

/** 注入的 setTimeout 返回、交还 clearTimeout 的定时器句柄 —— Node 的
 * Timeout 或浏览器式数字 id（测试注入数字句柄）。按不透明值存取，从不检视。 */

/** Timer handle returned by an injected setTimeout and handed back to the
 *  matching clearTimeout — Node's Timeout object or a browser-style numeric
 *  id (tests inject numeric handles). Stored opaquely, never inspected. */
export type AgentRunsTimerHandle = NodeJS.Timeout | number;

/** AgentRunsAggregator 的注入依赖：snapshot/publish 必需（构造时
 * 校验），其余（冷扫描/预热/时钟/定时器）可选，测试可全量替身。 */
export interface AgentRunsAggregatorDeps {
  /** 返回每个活跃 host 会话一条目的数组。 */
  snapshot?: () => AgentsSnapshotEntry[] | null | undefined;
  /** 事件发布回调（引擎传 ompBus.publish）。 */
  publish?: (type: string, payload: AgentsUpdatedPayload, scope: AgentRunsPublishScope) => void;
  /** 冷扫描回调：目录 → 历史行数组。 */
  diskScan?: (directory: string) => DiskScanRow[] | null | undefined;
  /** 引擎钩子：为目录填充其冷扫描缓存（异步、一次性）。 */
  /** Engine hook: fill its disk-scan cache for a directory (async, one shot). */
  warmDiskScan?: (directory: string) => Promise<void>;
  /** transcript 头缓存查找（为 parked 行补 childSessionID）。 */
  childSessionIdFor?: (sessionFile: string) => string | undefined;
  /** 发布合并窗口毫秒数（默认 250）。 */
  coalesceMs?: number;
  /** 可注入的定时器（默认全局 setTimeout；测试替身用）。 */
  setTimeout?: (fn: () => void, ms?: number) => AgentRunsTimerHandle;
  /** 与 setTimeout 注入配对的清除函数。 */
  clearTimeout?: (timer: AgentRunsTimerHandle) => void;
  /** 可注入时钟（默认 Date.now）。 */
  now?: () => number;
}

/**
 * 按目录聚合所有活跃会话的私有 AgentRegistry（D04-6）。引擎注入
 * snapshot 回调，每个活跃 host 会话返回一条：
 *
 *   { sessionID, directory, registry: { list(): AgentRef[] } }
 *
 * 另可注入 diskScan(directory) 返回冷行（引擎侧对该目录 artifacts 账本
 * 的文件系统扫描，§5.2.3）—— 这些行是 historical（R2-M5）：可见、
 * 可读 transcript，但绝不可复活。同 key 时活跃注册表行优先于磁盘行。
 *
 * 注册表变更被合并（250ms，§5.5.1）为全量快照 omp.agents.updated 事件
 * {agentRuns, revision}，经注入的 publish 回调发布（引擎传 ompBus.publish；
 * 信封按 omp-event-registry.json 携带目录范围与 durable=true）。
 */

/**
 * Directory-scoped aggregation of every live session's private AgentRegistry
 * (D04-6). The engine injects a `snapshot` callback returning one entry per
 * live host session:
 *
 *   { sessionID: string, directory: string,
 *     registry: { list(): AgentRef[] } }   // the session's PRIVATE registry
 *
 * plus an optional `diskScan(directory)` callback returning cold rows
 * (engine-side FS scan over the directory's artifacts ledger, §5.2.3) —
 * those rows are `historical` (R2-M5): visible + transcript-readable, never
 * revivable. Live registry rows override same-key disk rows.
 *
 * Registry changes are coalesced (250ms, §5.5.1) into full-snapshot
 * omp.agents.updated events { agentRuns, revision } via the injected
 * `publish` callback (engine passes ompBus.publish; envelope carries
 * directory scope and durable=true per omp-event-registry.json).
 */
export class AgentRunsAggregator {
  /** 构造函数：校验 snapshot/publish 两个必需回调，落存全部注入依赖与默认值。 */

  /**
   * @param {{ snapshot: () => AgentsSnapshotEntry[] | null | undefined,
   *           publish: (type: string, payload: AgentsUpdatedPayload, scope: AgentRunsPublishScope) => void,
   *           diskScan?: (directory: string) => DiskScanRow[] | null | undefined, coalesceMs?: number,
   *           setTimeout?: (fn: () => void, ms?: number) => AgentRunsTimerHandle,
   *           clearTimeout?: (timer: AgentRunsTimerHandle) => void,
   *           now?: () => number }} options
   */
  constructor({ snapshot, publish, diskScan, childSessionIdFor, warmDiskScan, coalesceMs = 250, setTimeout: setTimer = setTimeout, clearTimeout: clearTimer = clearTimeout, now = () => Date.now() }: AgentRunsAggregatorDeps = {}) {
    if (typeof snapshot !== 'function') throw new TypeError('AgentRunsAggregator: snapshot callback is required');
    if (typeof publish !== 'function') throw new TypeError('AgentRunsAggregator: publish callback is required');
    this.#snapshot = snapshot;
    this.#publish = publish;
    this.#diskScan = diskScan ?? null;
    this.#childSessionIdFor = childSessionIdFor;
    this.#warmDiskScan = warmDiskScan;
    this.#coalesceMs = coalesceMs;
    this.#setTimer = setTimer;
    this.#clearTimer = clearTimer;
    this.#now = now;
  }

  /** 活跃会话快照回调（构造时已校验为函数）。 */
  #snapshot: () => AgentsSnapshotEntry[] | null | undefined;
  /** 事件发布回调。 */
  #publish: (type: string, payload: AgentsUpdatedPayload, scope: AgentRunsPublishScope) => void;
  /** transcript 头缓存查找（可选）。 */
  #childSessionIdFor: ((sessionFile: string) => string | undefined) | undefined;
  /** 经 ensureDirectory 显式登记的目录（无行也纳入冷扫描）。 */
  #ensureDirectories = new Set<string>();
  /** 引擎的磁盘缓存预热钩子（可选）。 */
  #warmDiskScan: ((directory: string) => Promise<void>) | undefined;
  /** 当前行表：复合 key → OmpAgentRun。 */
  #rows = new Map<string, OmpAgentRun>();
  /** 单调递增修订号（每次 refresh +1）。 */
  #revision = 0;
  /** 最近一次快照生成时间。 */
  #generatedAt = 0;
  /** 挂起的合并发布定时器句柄（无则 null）。 */
  #timer: AgentRunsTimerHandle | null = null;

  /**
   * 丢弃某个拥有者会话的全部行（docs/plan.md §6）：引擎在 evict/delete
   * 时调用，避免死亡会话的代理行残留为过期投影；下次 refresh 只从
   * 活跃注册表重建。返回删除的行数。
   */

  /**
   * Drop one owner session's rows (docs/plan.md §6): engine calls this on
   * evict/delete so a dead session's agent rows do not survive as stale
   * projections. The next refresh rebuilds from live registries only.
   */
  releaseForSession(directory: string, sessionID: string): number {
    const prefix = agentRunSessionPrefix(directory, sessionID);
    let dropped = 0;
    for (const key of this.#rows.keys()) {
      if (key.startsWith(prefix)) {
        this.#rows.delete(key);
        dropped += 1;
      }
    }
    return dropped;
  }
  /** 注入时钟（默认 Date.now）。 */
  #now: () => number;
  /** 冷扫描回调（未注入时为 null）。 */
  #diskScan: ((directory: string) => DiskScanRow[] | null | undefined) | null;
  /** 发布合并窗口（毫秒）。 */
  #coalesceMs: number;
  /** 注入的 setTimeout（默认全局）。 */
  #setTimer: (fn: () => void, ms?: number) => AgentRunsTimerHandle;
  /** 注入的 clearTimeout（与 #setTimer 配对）。 */
  #clearTimer: (timer: AgentRunsTimerHandle) => void;
  /** 待发布（脏）目录集合，flush 时整体替换为空集。 */
  #dirty = new Set<string>();

  /** 从活跃注册表（+ 冷扫描）重建全部行，然后触发通知；返回最新快照。 */

  /** Rebuild rows from live registries (+ cold scan), then notify. */
  refresh(): AgentRunsSnapshot {
    // Directories that had rows before the rebuild stay dirty even if they
    // end up empty — UI stores must receive the empty snapshot to REPLACE
    // (not merge) their authoritative state.
    const previousDirectories = new Set([...this.#rows.values()].map((row) => row.directory));
    const next = new Map<string, OmpAgentRun>();
    for (const entry of this.#snapshot() ?? []) {
      if (!entry || typeof entry.sessionID !== 'string') continue;
      const directory = normalizeDirectoryKey(entry.directory ?? '');
      const refs = Array.isArray(entry.refs)
        ? entry.refs
        : typeof entry.registry?.list === 'function'
          ? entry.registry.list()
          : [];
      for (const ref of refs) {
        if (!ref || typeof ref.id !== 'string') continue;
        next.set(agentRunKey(directory, entry.sessionID, ref.id), projectAgentRun({ sessionID: entry.sessionID, directory, ref, childSessionIdFor: this.#childSessionIdFor }));
      }
    }
    if (this.#diskScan) {
      const directories = new Set([
        ...[...next.values()].map((row) => row.directory),
        ...previousDirectories,
        ...this.#ensureDirectories,
      ]);
      for (const directory of directories) {
        for (const cold of this.#diskScan(directory) ?? []) {
          if (!cold || typeof cold.agentId !== 'string' || typeof cold.sessionID !== 'string') continue;
          const key = agentRunKey(directory, cold.sessionID, cold.agentId);
          if (next.has(key)) continue; // registry row wins over disk row
          next.set(key, {
            key,
            sessionID: cold.sessionID,
            directory,
            agentId: cold.agentId,
            displayName: cold.displayName ?? cold.agentId,
            kind: cold.kind ?? 'sub',
            ...(cold.parentId ? { parentId: cold.parentId } : {}),
            status: 'historical',
            createdAt: cold.createdAt ?? 0,
            lastActivity: cold.lastActivity ?? 0,
            hasTranscript: cold.hasTranscript ?? true,
            ...(cold.childSessionID ? { childSessionID: cold.childSessionID } : {}),
          });
        }
      }
    }
    this.#rows = next;
    this.#revision += 1;
    this.#generatedAt = this.#now();
    for (const directory of previousDirectories) this.#dirty.add(directory);
    this.notify();
    return this.snapshot();
  }

  /** 列表端点入口：先预热引擎在该目录的磁盘缓存再重建，让没有任何
   * 活跃行的目录也能浮出历史行（仅 refresh 只扫描已有行的目录）。 */

  /**
   * List-endpoint entry: warm the engine's disk cache for a directory, then
   * rebuild so historical rows surface even for directories with no live
   * rows (refresh alone only scans directories that already have rows).
   */
  async ensureDirectory(directory: string): Promise<AgentRunsSnapshot> {
    const key = normalizeDirectoryKey(directory);
    if (!key) return this.snapshot();
    this.#ensureDirectories.add(key);
    await this.#warmDiskScan?.(key);
    return this.refresh();
  }

  /** 标脏并调度合并发布（每目录最终一个事件）；已有定时器在途时直接返回。 */

  /** Mark dirty and schedule the coalesced publish (one event per directory). */
  notify(directory: string | null = null): void {
    if (directory) this.#dirty.add(normalizeDirectoryKey(directory));
    else for (const row of this.#rows.values()) this.#dirty.add(row.directory);
    if (this.#timer) return;
    this.#timer = this.#setTimer(() => {
      this.#timer = null;
      this.flush();
    }, this.#coalesceMs);
  }

  /** 立即发布全部脏目录（每目录一份全量快照）。引擎也可在 dispose 时
   * 调用，确保尾部快照不丢失。 */

  /**
   * Publish pending dirty directories immediately (one full snapshot per
   * directory). Engine may also call this at dispose so a trailing snapshot
   * is never lost.
   */
  flush(): void {
    if (this.#dirty.size === 0) return;
    const dirty = this.#dirty;
    this.#dirty = new Set();
    for (const directory of dirty) {
      this.#publish(
        OMP_AGENTS_UPDATED,
        { agentRuns: this.#rowsFor(directory), revision: this.#revision },
        { directory, durable: true },
      );
    }
  }

  /** 当前行（可按目录过滤），按 TUI hub 风格排序：状态优先级
   * running > idle > parked > aborted > historical，同状态按最近活动
   * 在前；未知状态排最后。 */

  /** Current rows, optionally directory-filtered, sorted TUI-hub style. */
  #rowsFor(directory: string | null): OmpAgentRun[] {
    const wanted = directory ? normalizeDirectoryKey(directory) : null;
    return [...this.#rows.values()]
      .filter((row) => !wanted || row.directory === wanted)
      .sort((a, b) => {
        // SAFETY: both statuses are AgentRunStatus members (rows come from the
        // aggregator); unknown values intentionally rank last via `?? 9`.
        const rank = (status: string) => STATUS_ORDER[status as keyof typeof STATUS_ORDER] ?? 9;
        return (rank(a.status) - rank(b.status)) || (b.lastActivity ?? 0) - (a.lastActivity ?? 0);
      });
  }

  /** GET /omp/agent-runs 的快照：{agentRuns, generatedAt, revision}。 */

  /** GET /omp/agent-runs snapshot: { agentRuns, generatedAt, revision }. */
  snapshot(directory: string | null = null): AgentRunsSnapshot {
    return {
      agentRuns: this.#rowsFor(directory),
      generatedAt: this.#generatedAt,
      revision: this.#revision,
    };
  }

  /** 按 目录+会话+代理 取单行，无则 null。未带 directory 的全局查找在
   * 同一 id 命中多个目录时拒绝猜测、返回 null（与活跃注册表语义对齐）。 */

  /** One row by directory+session+agent, or null. */
  row(sessionID: string, agentId: string, directory?: string | null): OmpAgentRun | null {
    if (directory) return this.#rows.get(agentRunKey(directory, sessionID, agentId)) ?? null;
    // Unscoped lookup: same session id under two directories is ambiguous —
    // refuse to guess (live-registry parity) rather than pick one.
    let found: OmpAgentRun | null = null;
    for (const candidate of this.#rows.values()) {
      if (candidate.sessionID !== sessionID || candidate.agentId !== agentId) continue;
      if (found !== null) return null;
      found = candidate;
    }
    return found;
  }

  /** 取消挂起的合并发布并清空脏集合（引擎 dispose 钩子）。 */

  /** Stop any pending publish (engine dispose hook). */
  dispose(): void {
    if (this.#timer) this.#clearTimer(this.#timer);
    this.#timer = null;
    this.#dirty = new Set();
  }
}

/** 单个 parked 代理的复活描述符：定位三元组 + 可选 ref 快照 + 引擎
 * 提供的 revive 闭包。 */
export interface ParkedAgentDescriptor {
  /** 拥有者会话 id。 */
  sessionID: string;
  /** 代理 id。 */
  agentId: string;
  /** 所属目录 —— 会话 id 跨目录会撞（plan §3.1）。 */
  /** Owning directory — session ids collide across directories (plan §3.1). */
  directory?: string;
  /** 注册表引用快照（可选）。 */
  ref?: AgentRefLike;
  /** 复活闭包：重建 AgentSession 并接回私有注册表。 */
  revive: () => Promise<object>;
}

/**
 * PARKED 行的内存复活描述符表（R2-M5）。只有进程内的 parked 代理可
 * 复活；进程重启后所有磁盘行都是 historical（持久化描述符是独立的
 * P2 工作流）。引擎在 park 子代理时登记描述符；描述符的 revive 闭包
 * 对会话私有注册表重放 createAgentSession（ref 绑定 CAS，
 * agent-registry.ts:170-174）。
 */

/**
 * In-memory revival descriptors for PARKED rows (R2-M5). Only in-process
 * parked agents are revivable; after a process restart every disk row is
 * historical (persistent descriptors are a separate P2 workstream). The
 * engine registers a descriptor when it parks a sub agent; the descriptor's
 * revive closure replays createAgentSession against the session's private
 * registry (ref-bound CAS, agent-registry.ts:170-174).
 */
export class ParkedAgentDescriptors {
  /** 描述符表：复合 key → 描述符。 */
  #map = new Map<string, ParkedAgentDescriptor>();

  /** 登记一个描述符（按 agentRunKey 复合键存储）；revive 非函数时抛 TypeError。 */

  /**
   * @param {{ sessionID: string, agentId: string, ref?: object,
   *           revive: () => Promise<object> }} descriptor
   */
  register({ sessionID, agentId, directory, ref, revive }: ParkedAgentDescriptor): void {
    if (typeof revive !== 'function') throw new TypeError('descriptor.revive is required');
    this.#map.set(agentRunKey(directory ?? '', sessionID, agentId), { sessionID, agentId, directory, ref, revive });
  }

  /** 是否存在指定 (sessionID, agentId[, directory]) 的描述符。 */
  has(sessionID: string, agentId: string, directory?: string): boolean {
    return this.#map.has(agentRunKey(directory ?? '', sessionID, agentId));
  }

  /** 消费式认领描述符（单次 —— 复活失败由引擎重新登记）；不存在返回 null。 */

  /** Consume the descriptor (single claim — a failed revive re-registers). */
  claim(sessionID: string, agentId: string, directory?: string | null): ParkedAgentDescriptor | null {
    const key = agentRunKey(directory ?? '', sessionID, agentId);
    const descriptor = this.#map.get(key);
    if (descriptor) this.#map.delete(key);
    return descriptor ?? null;
  }

  /**
   * 丢弃某个会话拥有的全部描述符（docs/plan.md §6）：revive 闭包持有
   * 注册表/运行时引用，拥有者 evict/delete/dispose 时必须释放。按
   * directory+id 圈定 —— 只按 id 释放会误删另一目录同 id 会话的
   * parked 描述符。聚合器的行不受影响（它们是 transcript 投影而非
   * 描述符）。返回删除的数量。
   */

  /**
   * Drop every descriptor owned by one session (docs/plan.md §6): the
   * revive closures hold registry/runtime references, so owner
   * evict/delete/dispose must release them. Scoped by directory+id —
   * releasing by id alone would tear down another directory's parked
   * descriptors under a same-id session. Aggregator rows survive — they
   * are transcript projections, not descriptors.
   */
  releaseForSession(directory: string, sessionID: string): number {
    const prefix = agentRunSessionPrefix(directory, sessionID);
    let dropped = 0;
    for (const key of this.#map.keys()) {
      if (key.startsWith(prefix)) {
        this.#map.delete(key);
        dropped += 1;
      }
    }
    return dropped;
  }

  /** 当前登记的描述符数量。 */
  get size() {
    return this.#map.size;
  }
}

/**
 * jobs 快照形状（§5.6）—— {ownerSessionID, running, recent, delivery}
 * （上游注入点落地后即 SDK 的 AsyncJobSnapshot，R12）。handleJobsRequest
 * 把它摊在自己的 ownerSessionID 之上合成 200 响应体；行内容对本模块
 * 保持不透明。
 */

/**
 * Jobs snapshot shape (§5.6) — { ownerSessionID, running, recent, delivery }
 * (= SDK AsyncJobSnapshot once the upstream injection point lands, R12).
 * handleJobsRequest spreads it over its own ownerSessionID into the 200
 * body; the rows themselves stay opaque to this module.
 */
export interface JobsSnapshot {
  /** 持有 AsyncJobManager 的会话 id（可为 null）。 */
  ownerSessionID?: string | null;
  /** 进行中的任务行。 */
  running?: unknown[];
  /** 最近完成的任务行。 */
  recent?: unknown[];
  /** 投递状态信息（形状由引擎决定）。 */
  delivery?: unknown;
}

/** 引擎注入的 agent-run 行为钩子（§5.5.2）：revive/kill/chat 三个动作
 * 外加 jobsSnapshot；缺哪个，对应路径就回 500 hook-unavailable。 */
export interface AgentRunActions {
  /** 复活 parked 子代理（消费描述符后由引擎执行）。 */
  revive?: (descriptor: ParkedAgentDescriptor, row: OmpAgentRun) => Promise<void> | void;
  /** 中止代理并留下墓碑。 */
  kill?: (row: OmpAgentRun) => Promise<void> | void;
  /** 对活跃行发 prompt/steer 消息。 */
  chat?: (row: OmpAgentRun, message: { text: string; mode: 'prompt' | 'steer' }) => Promise<void> | void;
  /** jobs 快照钩子：(ownerSessionID, recentLimit) → JobsSnapshot。 */
  jobsSnapshot?: (ownerSessionID: string | null, recentLimit: number) => Promise<JobsSnapshot>;
}

/** POST /omp/agent-runs/... 的请求体（字段为 unknown，handler 运行时校验）。 */
export interface AgentRunActionBody {
  /** 动作种类：revive | kill | chat。 */
  kind?: unknown;
  /** chat 必填的消息文本。 */
  text?: unknown;
  /** chat 模式：prompt | steer（缺省 prompt）。 */
  mode?: unknown;
}

/** handleAgentRunAction 的输入：聚合器/描述符表/行为钩子 + 路由参数与请求体。 */
export interface AgentRunActionInput {
  /** 行查询源（聚合器或鸭子类型，需支持 row()）。 */
  aggregator: { row(sessionID: string, agentId: string, directory?: string | null): OmpAgentRun | null };
  /** parked 描述符表（revive/chat-on-parked 时消费）。 */
  descriptors: ParkedAgentDescriptors;
  /** 引擎行为钩子（revive/kill/chat）。 */
  actions: AgentRunActions;
  /** 拥有者会话 id。 */
  sessionID: string;
  /** 目标代理 id。 */
  agentId: string;
  /** 作用域目录（可选；提供时二次校验行归属）。 */
  directory?: string | null;
  /** 已解析的请求体（缺省空对象）。 */
  body?: AgentRunActionBody;
}

/**
 * POST /omp/agent-runs/{sessionID}/{agentId} 的门控核心（§5.5.2）。
 * M5 规则在此强制执行；实际行为由引擎注入的钩子完成：
 *   actions.revive(descriptor, row) — parked 子代理 → 重新活跃
 *   actions.kill(row)               — 中止 + 墓碑
 *   actions.chat(row, {text, mode}) — 对活跃行 prompt/steer
 * 行不存在或目录不匹配 → 404；historical 行拒绝一切动作（409）；
 * 钩子缺失 → 500 hook-unavailable；kind 非法 → 400。
 * 调用前须先 refresh 聚合器，行状态才准确。
 */

/**
 * POST /omp/agent-runs/{sessionID}/{agentId} gating core (§5.5.2). The
 * M5 rules are enforced HERE; the engine injects the actual behaviors:
 *   actions.revive(descriptor, row) — parked sub agent → live again
 *   actions.kill(row)               — abort + tombstone
 *   actions.chat(row, {text, mode}) — prompt/steer a live row
 * `aggregator` must be refreshed before the call for accurate statuses.
 */
export const handleAgentRunAction = async ({
  aggregator,
  descriptors,
  actions,
  sessionID,
  agentId,
  directory,
  body = {},
}: AgentRunActionInput): Promise<Response> => {
  const row = aggregator.row(sessionID, agentId, directory);
  if (!row) return json({ error: 'agent-run-not-found' }, { status: 404 });
  if (directory && normalizeDirectoryKey(directory) !== row.directory) {
    return json({ error: 'agent-run-not-found' }, { status: 404 });
  }
  // 取指定名称的行为钩子；不是函数时返回 null（调用方回 500 hook-unavailable）。
  const hook = <K extends 'revive' | 'kill' | 'chat'>(name: K) => {
    const fn = actions[name];
    // SAFETY: the typeof guard proved the member is the callable arm.
    return typeof fn === 'function' ? { fn: fn as Required<AgentRunActions>[K] } : null;
  };
  if (body.kind === 'revive') {
    if (row.status === 'historical') {
      // R2-M5: disk rows are transcript-view only, forever historical until
      // persistent revival descriptors land (§8.9).
      return json({ error: 'historical', revivable: false }, { status: 409 });
    }
    if (row.status !== 'parked') {
      return json({ error: 'not-parked', status: row.status }, { status: 409 });
    }
    const descriptor = descriptors.claim(sessionID, agentId, directory);
    if (!descriptor) {
      return json({ error: 'reviver-unavailable', revivable: false }, { status: 409 });
    }
    const reviveHook = hook('revive');
    if (!reviveHook) return json({ error: 'hook-unavailable', hook: 'revive' }, { status: 500 });
    await reviveHook.fn(descriptor, row);
    return json({ ok: true, status: 'running' });
  }
  if (body.kind === 'kill') {
    if (row.status === 'historical') {
      return json({ error: 'historical', revivable: false }, { status: 409 });
    }
    const killHook = hook('kill');
    if (!killHook) return json({ error: 'hook-unavailable', hook: 'kill' }, { status: 500 });
    await killHook.fn(row);
    return json({ ok: true, status: 'aborted' });
  }
  if (body.kind === 'chat') {
    if (row.status === 'historical') {
      return json({ error: 'historical', revivable: false }, { status: 409 });
    }
    if (typeof body.text !== 'string' || body.text.length === 0) {
      return json({ error: 'text-required' }, { status: 400 });
    }
    const rawMode = body.mode ?? 'prompt';
    if (rawMode !== 'prompt' && rawMode !== 'steer') {
      return json({ error: 'invalid-mode' }, { status: 400 });
    }
    const mode = rawMode === 'steer' ? 'steer' : 'prompt';
    if (row.status === 'parked') {
      // chat on parked = revive first (TUI parity), then prompt/steer.
      const descriptor = descriptors.claim(sessionID, agentId, directory);
      if (!descriptor) {
        return json({ error: 'reviver-unavailable', revivable: false }, { status: 409 });
      }
      const reviveHook = hook('revive');
      if (!reviveHook) return json({ error: 'hook-unavailable', hook: 'revive' }, { status: 500 });
      await reviveHook.fn(descriptor, row);
    }
    const chatHook = hook('chat');
    if (!chatHook) return json({ error: 'hook-unavailable', hook: 'chat' }, { status: 500 });
    await chatHook.fn(row, { text: body.text, mode });
    return json({ ok: true, status: 'running' });
  }
  return json({ error: 'invalid-kind' }, { status: 400 });
};

// ---------------------------------------------------------------------------
// §5.6 jobs (master R12)
// ---------------------------------------------------------------------------

/** 501 响应中携带的 jobs 不可用原因码：SDK 当前是单管理器模型（C2）。 */

export const JOBS_UNAVAILABLE_REASON = 'sdk-single-manager';

/** handleJobsRequest 的输入：活跃会话 id 顺序表、能力开关、可选的
 * 快照钩子与 recent 上限。 */
export interface JobsRequestInput {
  /** 按物化顺序的活跃会话 id（首者即 owner）。 */
  liveSessionIds?: string[];
  /** capabilities.jobs 开关（false 时恒 501）。 */
  jobsEnabled?: boolean;
  /** 快照钩子（能力开启后必需，否则仍回 501 no-snapshot-hook）。 */
  snapshot?: (ownerSessionID: string | null, recentLimit: number) => Promise<JobsSnapshot>;
  /** recent 行数上限（缺省 5）。 */
  recentLimit?: number;
}

/**
 * GET /omp/jobs 的 handler 核心（§5.6）。capabilities.jobs 为 false 时
 * 响应恒为结构化 501（绝不是 404），并携带 ownerSessionID，让任意
 * 顺序到达的每个会话都得到同一个确定性答案（C2：只有第一个物化的
 * 顶层会话持有进程的 AsyncJobManager，sdk.ts:1599-1616）。
 * liveSessionIds 是引擎钩子，按物化顺序返回活跃会话 id；ownerSessionID
 * 即首个 id。待上游注入点落地、能力翻转后，注入的
 * snapshot(ownerSessionID, recentLimit) 钩子以
 * {ownerSessionID, running, recent, delivery}（= AsyncJobSnapshot）答 200。
 */

/**
 * GET /omp/jobs core (§5.6). While capabilities.jobs is false the response
 * is ALWAYS the structured 501 — never a 404 — carrying ownerSessionID so
 * every session, in any order, gets one deterministic answer (C2: only the
 * first materialized top-level session holds the process AsyncJobManager,
 * sdk.ts:1599-1616). `liveSessionIds` is the engine hook returning live
 * session ids in materialization order; ownerSessionID = first id.
 *
 * When the upstream injection point lands and the capability flips, the
 * injected `snapshot(ownerSessionID, recentLimit)` hook answers 200 with
 * { ownerSessionID, running, recent, delivery } (= AsyncJobSnapshot).
 */
export const handleJobsRequest = async ({ liveSessionIds = [], jobsEnabled = false, snapshot, recentLimit = 5 }: JobsRequestInput): Promise<Response> => {
  const ownerSessionID = liveSessionIds[0] ?? null;
  if (!jobsEnabled) {
    return json(
      { error: 'jobs-unavailable', reason: JOBS_UNAVAILABLE_REASON, ownerSessionID },
      { status: 501 },
    );
  }
  if (typeof snapshot !== 'function') {
    return json(
      { error: 'jobs-unavailable', reason: 'no-snapshot-hook', ownerSessionID },
      { status: 501 },
    );
  }
  const body = await snapshot(ownerSessionID, recentLimit);
  return json({ ownerSessionID, ...body });
};

// ---------------------------------------------------------------------------
// artifacts browsing — host-level read-only supervision of local:// roots
// (spec 04 §1 "artifacts 目录浏览"; capability `artifacts`)
// ---------------------------------------------------------------------------

/** 每个会话最多回报的文件行数；引擎遍历会在超过此数一行后即停，
 * 使 truncated 精确、列表响应不会无界增长。 */

/** Max file rows reported per session; the engine walk stops one past this
 *  so `truncated` is exact and no listing response grows unbounded. */
export const ARTIFACTS_MAX_FILES_PER_SESSION = 2000;

/** 引擎 local:// 文件行的原始（宽松）形状：ref/size/modifiedAt 均为
 * unknown，经 normalizedArtifactsRows 收窄。 */
type ArtifactsFileRow = { ref?: unknown; size?: unknown; modifiedAt?: unknown };

/** 把引擎返回的文件行归一化：滤掉 null/undefined 与无有效 ref 的项，
 * size/modifiedAt 一律经 Number() 收敛为数字（非法值为 0）。 */
const normalizedArtifactsRows = (files: ArtifactsFileRow[] | null | undefined) =>
  (Array.isArray(files) ? files : [])
    .filter((file): file is ArtifactsFileRow & { ref: string } => file !== null && file !== undefined && typeof file.ref === 'string' && file.ref.length > 0)
    .map((file) => ({
      ref: file.ref,
      size: Number(file.size) || 0,
      modifiedAt: Number(file.modifiedAt) || 0,
    }));

/**
 * GET /omp/artifacts 的 handler 核心（公网路径 /api/omp/artifacts）。
 *
 * 只回报单个会话的文件行：ref 是 local:// 后缀（如 'PLAN.md'、
 * 'scratch/notes.md'），按 mtime 降序、按会话封顶。浏览器按会话组织是
 * 设计使然（文件属于会话；切会话即切树），故不提供目录级会话索引。
 *
 * filesFor 返回 null 表示"该目录不认识这个会话"（→ 404
 * session-not-found）；local:// 根不存在则是权威空（files: []），绝不
 * 当失败处理。响应不带绝对路径（R7）：ref 相对会话自身的根。
 */

/**
 * GET /omp/artifacts handler core (public path /api/omp/artifacts).
 *
 * ONE session's file rows: `ref` is the local:// suffix (e.g. 'PLAN.md',
 * 'scratch/notes.md'), mtime desc, bounded per session. The browser is
 * per-session by design (files belong to the session; switching sessions
 * switches trees), so no directory-wide session index is offered.
 *
 * `filesFor` returning null means "session unknown to this directory"
 * (→ 404 session-not-found); an absent local:// root is authoritative empty
 * (`files: []`), never failure. Responses carry no absolute paths (R7): refs
 * are relative to the session's own root.
 *
 * @param {{ directory: string | null, sessionID?: string | null,
 *           filesFor: (sessionID: string, directory: string) => Promise<{ files: Array<{ref: string, size: number, modifiedAt: number}>, truncated?: boolean } | null> }} input
 */
export const handleArtifactsList = async ({ directory, sessionID, filesFor }: { directory?: unknown; sessionID?: unknown; filesFor?: (sessionID: string, directory: string) => ArtifactsListResult | null | Promise<ArtifactsListResult | null> }) => {
  if (typeof directory !== 'string' || directory.length === 0) {
    return json({ error: 'directory-required' }, { status: 400 });
  }
  if (typeof sessionID !== 'string' || sessionID.length === 0) {
    return json({ error: 'session-required' }, { status: 400 });
  }
  if (typeof filesFor !== 'function') {
    return json({ error: 'hook-unavailable', hook: 'localFiles' }, { status: 500 });
  }
  const result = await filesFor(sessionID, directory);
  if (!result) return json({ error: 'session-not-found' }, { status: 404 });
  const files = normalizedArtifactsRows(result.files)
    .sort((a, b) => b.modifiedAt - a.modifiedAt)
    .slice(0, ARTIFACTS_MAX_FILES_PER_SESSION);
  return json({
    directory,
    sessionID,
    files,
    truncated: Boolean(result.truncated) || files.length >= ARTIFACTS_MAX_FILES_PER_SESSION,
  });
};

// ---------------------------------------------------------------------------
// Domain assembly + mount surface (coordinator integration)
// ---------------------------------------------------------------------------

/** 本域 POST 路由接受的已解析 JSON 体（各路由的超集；每个 handler
 * 只读取自己的字段并做防御式运行时校验）。 */

/** Parsed JSON body accepted by this domain's POST routes (superset; each
 * handler reads only its own fields, defensively). */
export interface UriRequestBody {
  /** 完整 URI 字符串（resolve，与 scheme/ref 二选一）。 */
  u?: unknown;
  /** scheme 部分（与 ref 搭配，resolve）。 */
  scheme?: unknown;
  /** scheme 内的引用部分（resolve）。 */
  ref?: unknown;
  /** 会话 id（resolve/action）。 */
  sessionID?: unknown;
  /** 作用域目录（resolve/open/action）。 */
  directory?: unknown;
  /** 只解析路径标记（resolve）。 */
  pathOnly?: unknown;
  /** 资源 token（open）。 */
  token?: string | null;
  /** 动作种类（agent-run action）。 */
  kind?: unknown;
  /** chat 消息文本（agent-run action）。 */
  text?: unknown;
  /** chat 模式（agent-run action）。 */
  mode?: unknown;
}

/** 读取请求的 JSON 体；解析失败（非 JSON/空体）安全返回空对象，
 * 字段级校验交由各 handler 完成。 */
const readJsonBody = async (request: Request): Promise<UriRequestBody> => {
  try {
    // SAFETY: handlers below runtime-validate the fields they read.
    return (await request.json()) as UriRequestBody;
  } catch {
    return {};
  }
};

/** directoryOf 的取值来源：查询参数、x-opencode-directory 请求头或
 * JSON body 字段（resolve/open/action 路由都会用到）。 */

/** directoryOf sources: query params, x-opencode-directory header, or a
 * JSON body field (resolve/open/action routes). */
interface DirectorySource {
  /** URL 查询参数载体（如 URLSearchParams）。 */
  query?: { get(key: string): string | null } | null;
  /** 请求头载体（Headers 或鸭子类型）。 */
  headers?: { get(name: string): string | null } | null;
  /** 已解析的 JSON 体（directory 字段来源之一）。 */
  body?: { directory?: unknown } | null;
}

/** 按 body → query → header（x-opencode-directory，URL 解码）的优先级
 * 取 directory 并经 normalizeDirectoryKey 归一化；三处皆无返回 null。 */
const directoryOf = ({ query, headers, body }: DirectorySource = {}): string | null => {
  const fromBody = body && typeof body.directory === 'string' ? body.directory : null;
  const fromQuery = query?.get('directory');
  const fromHeader = headers?.get?.('x-opencode-directory');
  const raw = fromBody ?? fromQuery ?? (fromHeader ? decodeURIComponent(fromHeader) : null);
  return raw ? normalizeDirectoryKey(raw) : null;
};

/** uri.info 的 token 查询载体：URLSearchParams（挂载路径）或纯 {token}
 * 对象（handler 直调）。 */

/** Token query for uri.info: URLSearchParams (mount) or a plain {token}
 * carrier. */
export interface UriTokenQueryLike {
  /** 可选的 get(key) 访问器（URLSearchParams 载体形态）。 */
  get?(key: string): string | null;
  /** 直接携带的 token 字段（{token} 载体形态）。 */
  token?: string | null;
}

/** Injected engine hooks + overridable services for createUriDomain. Every
 * member is optional; routes fail loudly when a required hook is absent
 * (see createUriDomain JSDoc for the per-hook contracts). */
/** 单个会话的 local:// artifacts 列表结果（引擎 #listLocalFiles）。 */

/** One session's local:// artifacts listing (engine #listLocalFiles). */
export interface ArtifactsListResult {
  /** 文件行（ref 相对会话自身的 local:// 根；size/modifiedAt 可选）。 */
  files: Array<{ ref: string; size?: number; modifiedAt?: number }>;
  /** true = 引擎遍历超过每会话上限被截断。 */
  truncated?: boolean;
}

/**
 * createUriDomain 的注入依赖：引擎钩子 + 可覆盖服务。所有成员可选；
 * 必需钩子缺失时对应路由会大声失败（500 hook-unavailable）。
 * 各钩子的逐项契约见 createUriDomain 的 JSDoc。
 */

export interface UriDomainDeps {
  /** 能力开关提供者：返回 OmpFeatures 的函数或记录本身（缺省 ompFeatures）。 */
  features?: () => OmpFeatures;
  /** token 服务实例（缺省新建 UriTokenService）。 */
  tokens?: UriTokenService;
  /** parked 描述符表（缺省新建）。 */
  descriptors?: ParkedAgentDescriptors;
  /** 可注入的 InternalUrlRouter（缺省在解析时取全局单例）。 */
  router?: InternalUrlRouter;
  /** 会话 → local:// 协议选项钩子（resolve 端点必需）。 */
  localOptionsFor?: LocalOptionsForHook;
  /** 目录 → wire 会话记录（engine.listSessions 的返回）。 */
  sessionTreeData?: (directory: string | null) => Promise<SessionTreeData>;
  /** (sessionID, directory) → 已建好的 {tree} 快照或 null（未知会话）；
   * 冷 manager 的关闭/释放由引擎负责（plan §7.1）。 */
  entryTreeFor?: (sessionID: string, directory: string | null) => Promise<{ tree: EntryTreeSnapshot } | null>;
  /** 目录 → 冷扫描行（historical 行的来源）。 */
  diskScan?: (directory: string) => DiskScanRow[] | null | undefined;
  /** 引擎钩子：为目录填充冷扫描缓存（异步、一次性）。 */
  /** Engine hook: fill its disk-scan cache for a directory (async, one shot). */
  warmDiskScan?: (directory: string) => Promise<void>;
  /** transcript 头缓存查找（为 parked 行补 childSessionID）。 */
  childSessionIdFor?: (sessionFile: string) => string | undefined;
  /** 事件发布回调（引擎传 ompBus.publish）。 */
  publish?: (type: string, payload: AgentsUpdatedPayload, scope: AgentRunsPublishScope) => void;
  /** 活跃会话私有注册表快照（聚合器的数据源）。 */
  agentsSnapshot?: () => AgentsSnapshotEntry[];
  /** 按物化顺序返回活跃会话 id（jobs owner 判定）。 */
  liveSessionIds?: () => string[];
  /** agent-run 行为钩子（revive/kill/chat/jobsSnapshot）。 */
  actions?: AgentRunActions;
  /** 每会话的 local:// 文件行（artifacts.v1）；null = 该目录不认识此会话。 */
  /** Per-session local:// file rows (artifacts.v1); null = unknown session. */
  localFiles?: (sessionID: string, directory: string) => ArtifactsListResult | null | Promise<ArtifactsListResult | null>;
}

/**
 * 宿主路由器（host.ts 的 fetch）提供的路由注册上下文：{params, url,
 * headers}；url/headers 可缺省，便于测试直调 handler。
 */

/** Route-registration context supplied by the host router (host.ts fetch:
 * {params, url, headers}); url/headers optional for direct handler calls. */
export interface UriRouteContext {
  /** 路由模式参数（tokenID/sessionID/agentId 等）。 */
  params: Record<string, string>;
  /** 已解析的请求 URL（可缺省，直调时）。 */
  url?: URL;
  /** 请求头载体（可缺省，直调时）。 */
  headers?: Headers;
}

/** 单条 /omp 路由的 handler 签名：Request + 可选路由上下文 → Response。 */

export type UriRouteHandler = (request: Request, ctx?: UriRouteContext) => Response | Promise<Response>;

/** mount() 的注册目标 —— 与 domain-plugins.ts 的 DomainRoute 同形。 */

/** mount() target — same shape as DomainRoute in domain-plugins.ts. */
export type UriRoute = (method: string, pattern: string, handler: UriRouteHandler) => void;

/**
 * createUriDomain 返回的第 04 章域面：既供 engine.uriDomain 消费
 * （mount 挂载 / dispose 释放），也供 endpoints 与测试直调各 handler 核心。
 */

/** The chapter-04 domain surface returned by createUriDomain: consumed by
 * engine.uriDomain (mount/dispose) and by endpoints/tests (handler cores). */
export interface UriDomain {
  /** 资源 token 服务实例。 */
  tokens: UriTokenService;
  /** agent-runs 聚合器实例。 */
  aggregator: AgentRunsAggregator;
  /** parked 复活描述符表（R2-M5，仅进程内可复活）。 */
  descriptors: ParkedAgentDescriptors;
  /** artifacts 浏览面：list 即 GET /omp/artifacts 的核心。 */
  artifacts: { list: (input: { directory?: string | null; sessionID?: string | null }) => Response | Promise<Response> };
  /** local:// URI 桥接面：resolve / open / info / content。 */
  uri: {
    /** POST /omp/uri/resolve：解析 local:// 资源并签发不透明 token。 */
    resolve: (input: { body?: UriResolveBody | null }) => Promise<Response>;
    /** POST /omp/uri/open：兑换 token 取文本内容。 */
    open: (input: { body?: { token?: string | null } | null; directory?: string | null }) => Promise<Response>;
    /** GET /omp/uri/info：token 元数据（不消耗读取次数）。 */
    info: (input: { query?: UriTokenQueryLike | null; directory?: string | null }) => Response;
    /** 兑换 token 取原始字节，并以存储的 content type 内联返回。 */
    /** Raw bytes endpoint (GET /omp/uri/tokens/{id}/content): streams the
     * token's file body with its stored content type. */
    content: (input: { id?: string | null; directory?: string | null }) => Promise<Response>;
  };
  /** 会话树面：目录级 fork 树 + 单会话 entry 树。 */
  tree: {
    /** 目录级会话树投影（buildSessionTree 的直调包装）。 */
    sessionTree: (input: { directory?: string | null }) => Promise<SessionTreeProjection>;
    /** GET /omp/sessions/{sessionID}/tree 单会话 entry 树（§5.4.1 只读视图）。 */
    entryTree: (input: { sessionID: string; directory?: string | null }) => Promise<Response>;
  };
  /** agent-runs 面：列表 + 动作。 */
  agentRuns: {
    /** GET /omp/agent-runs 列表快照（先按需 ensureDirectory 预热历史行）。 */
    list: (input: { directory?: string | null }) => Promise<Response>;
    /** POST /omp/agent-runs/{sessionID}/{agentId} 动作门控（revive/kill/chat）。 */
    action: (input: { sessionID?: string; agentId?: string; directory?: string | null; body?: AgentRunActionBody | null }) => Promise<Response>;
  };
  /** GET /omp/jobs 域方法（§5.6：结构化 501 或快照 200）。 */
  jobs: (input: { recentLimit?: number }) => Promise<Response>;
  /** 把 /omp 路由挂载到宿主路由器（公网路径为 /api/omp/...）。 */
  mount: (route: UriRoute) => void;
  /** 释放域资源：当前即聚合器 dispose（取消挂起的合并发布并清空脏集合）。 */
  dispose: () => void;
}

/**
 * 构建第 04 章（协议与实体）域对象。引擎钩子全部注入、此处不接线：
 * - localOptionsFor(sessionID, directory) → LocalProtocolOptions | null
 *     用 createLocalProtocolOptions(sessionID, directory,
 *     liveSessionManagerOrColdArtifactsDir) 构建；resolve 端点与引擎
 *     #materialize（sdk createAgentSession 的 localProtocolOptions）共用。
 * - sessionTreeData(directory) → wire 会话记录（engine.listSessions）。
 * - entryTreeFor(sessionID, directory) → {tree} | null：已构建好的快照；
 *     冷 manager 的关闭/释放由引擎负责（plan §7.1）。
 * - agentsSnapshot() → [{sessionID, directory, registry}]（活跃 host 会话
 *     的私有注册表；引擎在 #materialize 时留存）。
 * - localFiles(sessionID, directory) → {files, truncated} | null（引擎对该
 *     会话 local:// 根的遍历；null = 会话未知；根缺失 = 权威空）。artifacts 浏览用。
 * - publish(type, payload, scope) → engine.ompBus.publish。
 * - liveSessionIds() → 按物化顺序的活跃会话 id（jobs owner 判定）。
 * - actions.{revive,kill,chat} → agent-run 行为钩子（§5.5.2）。
 */

/**
 * Build the chapter-04 domain. Engine hooks (all injected, none wired here):
 * - localOptionsFor(sessionID, directory) → LocalProtocolOptions | null
 *     build with createLocalProtocolOptions(sessionID, directory,
 *     liveSessionManagerOrColdArtifactsDir). Used at BOTH the resolve
 *     endpoint and #materialize (sdk createAgentSession localProtocolOptions).
 * - sessionTreeData(directory) → wire session records (engine.listSessions).
 * - entryTreeFor(sessionID, directory) → { tree } | null: an already-built
 *     snapshot; the engine owns the cold manager's close/release (plan §7.1).
 * - agentsSnapshot() → [{ sessionID, directory, registry }] (private
 *     registries of live host sessions; engine retains them at #materialize).
 * - localFiles(sessionID, directory) → { files: [{ref,size,modifiedAt}],
 *     truncated } | null (engine walk of that session's local:// root; null
 *     = session unknown; absent root = authoritative empty). Artifacts browse.
 * - publish(type, payload, scope) → engine.ompBus.publish.
 * - liveSessionIds() → ordered live session ids (jobs owner).
 * - actions.{revive,kill,chat} → agent-run behaviors (§5.5.2).
 */
export const createUriDomain = ({
  features = ompFeatures,
  tokens = new UriTokenService(),
  descriptors = new ParkedAgentDescriptors(),
  router,
  localOptionsFor,
  sessionTreeData,
  localFiles,
  entryTreeFor,
  agentsSnapshot,
  diskScan,
  warmDiskScan,
  childSessionIdFor,
  publish,
  liveSessionIds = () => [],
  actions = {},
}: UriDomainDeps = {}): UriDomain => {
  // SAFETY: OmpFeatures is a boolean flag record; the hook form returns it,
  // the raw form is the record itself — both reads are flag lookups.
  // 能力开关读取：features 兼容钩子（返回 OmpFeatures）与原始记录两种形态，
  // 统一收敛为 Record<string, boolean> 供 featureOn 查询。
  const flags = (): Record<string, boolean> => {
    const source = features?.() ?? features;
    // SAFETY: OmpFeatures members are boolean flags (omp-parity contract).
    return (source ?? {}) as Record<string, boolean>;
  };
  // 单个能力开关是否开启（布尔化）。
  const featureOn = (key: string) => Boolean(flags()[key]);

  // 域内聚合器：桥接 agentsSnapshot/diskScan/childSessionIdFor/warmDiskScan/publish
  // 注入；publish 未注入时以空函数兜底（保证构造校验通过）。
  const runs = new AgentRunsAggregator({
    snapshot: () => (typeof agentsSnapshot === 'function' ? agentsSnapshot() : []),
    ...(publish ? { publish } : { publish: () => {} }),
    ...(diskScan ? { diskScan } : {}),
    ...(childSessionIdFor ? { childSessionIdFor } : {}),
    ...(warmDiskScan ? { warmDiskScan } : {}),
  });

  // 能力门控：开关未开时返回 featureUnavailable 响应；已开返回 null 放行。
  const gate = (key: string) => (featureOn(key) ? null : featureUnavailable(key));

  // uri.resolve 域方法：localOptionsFor 钩子未接线时显式 500（uri.v1 已开但引擎没接的场景）。
  const uriResolve = async ({ body }: { body?: UriResolveBody | null }) => {
    if (typeof localOptionsFor !== 'function') {
      // Only reachable with uri.v1 flipped on but no engine hook wired.
      return json({ error: 'hook-unavailable', hook: 'localOptionsFor' }, { status: 500 });
    }
    return handleUriResolve({ body: body ?? undefined, localOptionsFor, tokens, ...(router ? { router } : {}) });
  };
  // artifacts.list 域方法：localFiles 钩子未接线时显式 500（能力已开但引擎没接的场景）。
  const artifactsList = ({ directory, sessionID }: { directory?: string | null; sessionID?: string | null }) => {
    if (typeof localFiles !== 'function') {
      // Only reachable with artifacts flipped on but no engine hook wired.
      return json({ error: 'hook-unavailable', hook: 'localFiles' }, { status: 500 });
    }
    return handleArtifactsList({ directory, sessionID, filesFor: localFiles });
  };
  // uri.open 域方法：从请求体取 token，交 token 服务兑换文本内容。
  const uriOpen = async ({ body, directory }: { body?: UriRequestBody | null; directory?: string | null }) => {
    const token = typeof body?.token === 'string' ? body.token : undefined;
    return tokens.open(token, { directory: directory ?? undefined });
  };
  // uri.info 域方法：兼容 URLSearchParams 与 {token} 两种查询载体，取 token 查元数据。
  const uriInfo = ({ query, directory }: { query?: UriTokenQueryLike | null; directory?: string | null }) => {
    const token = query?.get?.('token') ?? query?.token ?? undefined;
    return tokens.describe(token, { directory: directory ?? undefined });
  };
  // uri.content 域方法：兑换原始字节后以内联形式返回 —— no-store 禁缓存、
  // nosniff 防 MIME 嗅探，文件名剥离引号防响应头注入。
  const uriContent = async ({ id, directory }: { id?: string | null; directory?: string | null }) => {
    if (typeof id !== 'string' || !id) return json({ error: 'token-required' }, { status: 400 });
    const result = await tokens.openRaw(id, { directory: directory ?? undefined });
    if (result.ok === false) return result.response;
    return new Response(result.bytes, {
      status: 200,
      headers: {
        'Content-Type': result.contentType,
        'Content-Length': String(result.bytes.byteLength),
        'Cache-Control': 'no-store',
        'Content-Disposition': `inline; filename="${result.filename.replaceAll('"', '')}"`,
        'X-Content-Type-Options': 'nosniff',
      },
    });
  };
  // tree.sessionTree 域方法：目录级会话树投影（无数据源钩子时按空数组投影）。
  const sessionTree = async ({ directory }: { directory?: string | null }) =>
    buildSessionTree((await sessionTreeData?.(directory ?? null)) ?? []);
  // tree.entryTree 域方法：单会话 entry 树快照；sessionID 缺失 400，未知会话 404。
  const entryTree = async ({ sessionID, directory }: { sessionID?: string | null; directory?: string | null }) => {
    if (typeof sessionID !== 'string' || !sessionID) return json({ error: 'sessionID required' }, { status: 400 });
    const found = await entryTreeFor?.(sessionID, directory ?? null);
    if (!found?.tree) return json({ error: 'session-not-found' }, { status: 404 });
    return json(found.tree);
  };
  // agentRuns.list 域方法：历史行惰性浮出 —— 先为请求目录预热引擎磁盘
  // 缓存（已扫描过则为空操作），再返回该目录快照。
  const agentRuns = async ({ directory }: { directory?: string | null }) => {
    // Historical rows surface lazily: warm the engine's disk cache for the
    // requested directory first (no-op once scanned), then answer.
    if (directory) await runs.ensureDirectory(directory);
    return json(runs.snapshot(directory ?? null));
  };
  // agentRuns.action 域方法：动作门控核心的薄封装（缺失参数补默认空值）。
  const agentRunAction = ({ sessionID, agentId, directory, body }: { sessionID?: string; agentId?: string; directory?: string | null; body?: AgentRunActionBody | null }) =>
    handleAgentRunAction({ aggregator: runs, descriptors, actions, sessionID: sessionID ?? '', agentId: agentId ?? '', directory, body: body ?? {} });
  // jobs 域方法：读取 jobs.v1 能力开关与注入的 jobsSnapshot 钩子，转交 handleJobsRequest。
  const jobs = ({ recentLimit }: { recentLimit?: number }) =>
    handleJobsRequest({
      liveSessionIds: liveSessionIds(),
      jobsEnabled: featureOn('jobs.v1'),
      ...(actions.jobsSnapshot ? { snapshot: actions.jobsSnapshot } : {}),
      recentLimit,
    });

  // 把全部 /omp 路由注册到宿主路由器：每条路由先过能力门控（gate），
  // 再解析请求体/查询参数并转交对应域方法（公网路径为 /api/omp/...）。
  /** Mount /omp routes on the omp-host router (public paths /api/omp/...). */
  const mount = (route: UriRoute) => {
    route('POST', '/omp/uri/resolve', async (request) => {
      const blocked = gate('uri.v1');
      if (blocked) return blocked;
      const body = await readJsonBody(request);
      return uriResolve({ body });
    });
    route('POST', '/omp/uri/open', async (request: Request, ctx?: UriRouteContext) => {
      const blocked = gate('uri.v1');
      if (blocked) return blocked;
      const body = await readJsonBody(request);
      return uriOpen({ body, directory: directoryOf({ body, query: ctx?.url?.searchParams, headers: ctx?.headers }) });
    });
    route('GET', '/omp/uri/tokens/{tokenID}/content', async (request: Request, ctx?: UriRouteContext) => {
      const blocked = gate('uri.v1');
      if (blocked) return blocked;
      const url = new URL(request.url);
      return uriContent({
        id: ctx?.params.tokenID,
        directory: directoryOf({ query: url.searchParams, headers: ctx?.headers }) ?? undefined,
      });
    });
    route('GET', '/omp/uri/info', async (request: Request, ctx?: UriRouteContext) => {
      const blocked = gate('uri.v1');
      if (blocked) return blocked;
      const url = new URL(request.url);
      return uriInfo({ query: url.searchParams, directory: directoryOf({ query: url.searchParams, headers: ctx?.headers }) });
    });
    route('GET', '/omp/sessions/{sessionID}/tree', async (request: Request, ctx?: UriRouteContext) => {
      const blocked = gate('tree.v1');
      if (blocked) return blocked;
      const url = new URL(request.url);
      const directory = directoryOf({ query: url.searchParams, headers: ctx?.headers });
      const sessionID = ctx?.params.sessionID;
      if (!sessionID) return json({ error: 'sessionID required' }, { status: 400 });
      const subtree = buildSessionSubtree(sessionID, (await sessionTreeData?.(directory ?? null)) ?? []);
      if (!subtree) return json({ error: 'session-not-found' }, { status: 404 });
      // §5.4 task shape {leafId, nodes:[{id,parentId,title,time}]}. The
      // per-session ENTRY tree (spec §5.4.1) is tree.entryTree / buildEntryTreeSnapshot.
      return json(subtree);
    });
    route('GET', '/omp/artifacts', async (request: Request, ctx?: UriRouteContext) => {
      const blocked = gate('artifacts');
      if (blocked) return blocked;
      const url = new URL(request.url);
      return artifactsList({
        directory: directoryOf({ query: url.searchParams, headers: ctx?.headers }),
        sessionID: url.searchParams.get('sessionID'),
      });
    });
    route('GET', '/omp/agent-runs', async (request: Request, ctx?: UriRouteContext) => {
      const blocked = gate('agentRuns.v1');
      if (blocked) return blocked;
      const url = new URL(request.url);
      return agentRuns({ directory: url.searchParams.get('directory') });
    });
    route('POST', '/omp/agent-runs/{sessionID}/{agentId}', async (request: Request, ctx?: UriRouteContext) => {
      const blocked = gate('agentRuns.v1');
      if (blocked) return blocked;
      const body = await readJsonBody(request);
      const url = new URL(request.url);
      return agentRunAction({
        sessionID: ctx?.params.sessionID,
        agentId: ctx?.params.agentId,
        directory: directoryOf({ body, query: url.searchParams }),
        body,
      });
    });
    route('GET', '/omp/jobs', async (request) => {
      const url = new URL(request.url);
      const recentLimit = Number(url.searchParams.get('recentLimit') ?? 5) || 5;
      return jobs({ recentLimit });
    });
  };

  return {
    tokens,
    aggregator: runs,
    descriptors,
    artifacts: { list: artifactsList },
    uri: { resolve: uriResolve, open: uriOpen, info: uriInfo, content: uriContent },
    tree: { sessionTree, entryTree },
    agentRuns: { list: agentRuns, action: agentRunAction },
    jobs,
    mount,
    dispose: () => runs.dispose(),
  };
};
