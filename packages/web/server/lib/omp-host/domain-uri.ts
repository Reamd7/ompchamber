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

import crypto from 'node:crypto';
import fs from 'node:fs/promises';
import path from 'node:path';
import { InternalUrlRouter } from '@oh-my-pi/pi-coding-agent/internal-urls/router';
import type { InternalResource } from '@oh-my-pi/pi-coding-agent/internal-urls/types';
import type { LocalProtocolOptions } from '@oh-my-pi/pi-coding-agent/internal-urls/local-protocol';
import { normalizeDirectoryKey } from './registry.ts';
import { errorText, ompFeatures, featureUnavailable } from './omp-parity.ts';
import type { OmpFeatures } from './omp-parity.ts';

const json = <T,>(data: T, init?: ResponseInit): Response => Response.json(data, init);

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

/** Cap for one raw token redemption (spec §5.2.4 byte stream; the artifact
 *  inline ceiling — previewable media, not arbitrary large-file transfer). */
const MAX_RAW_BYTES = 8 * 1024 * 1024;

const extensionOf = (value: string) => {
  const base = String(value ?? '').replaceAll('\\', '/').split('/').pop() ?? '';
  const dot = base.lastIndexOf('.');
  return dot === -1 ? '' : base.slice(dot).toLowerCase();
};

// ---------------------------------------------------------------------------
// §5.2 local:// URI bridge
// ---------------------------------------------------------------------------

/** Schemes whose HOST-side read resolution is enabled (master R7: P1 =
 *  local:// only; agent/history/artifact stay off until upstream per-resolve
 *  ResolveContext.artifactsDirs, R2-H2). */
const ENABLED_READ_SCHEMES = ['local'];

/** Router-mediated writes: none. LocalProtocolHandler exposes no write hook
 *  (router.ts:147-150 throws), so `write` stays empty — local:// writes are
 *  performed by the session's own write tool against the same pinned
 *  localProtocolOptions (master R8: P1 local-only, same directory+session). */
const ENABLED_WRITE_SCHEMES: string[] = [];

/** §5.2.2: `u` length ≤ 2 KiB. */
const MAX_URL_LENGTH = 2048;

/** Endpoint inline-content cap (task §5.2.1 note; the SDK local handler
 *  already refuses >1 MiB text and known-binary files). */
const MAX_INLINE_BYTES = 2 * 1024 * 1024;

/** Scheme × read/write capability matrix (R2-M11 shape, P1 values). */
export const uriCapabilities = () => ({
  read: [...ENABLED_READ_SCHEMES],
  write: [...ENABLED_WRITE_SCHEMES],
});

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

/**
 * Opaque resource tokens — the authorized stand-in for the SDK
 * InternalResource.sourcePath echo (R7 / §5.2.4; SDK types.ts:25 keeps that
 * field "for debugging, not exposed to agent"). Ids carry no path or
 * resource information; the absolute path exists only in process memory and
 * is never serialized into any response.
 */
export interface UriTokenOptions {
  ttlMs?: number;
  maxReads?: number;
  now?: () => number;
}

export interface UriTokenResource {
  resourceUrl: string;
  directory: string;
  sessionID?: string;
  contentType?: string;
  size?: number;
  immutable?: boolean;
  absolutePath: string;
}

export interface UriTokenIssueResult {
  id: string;
  expiresAt: number;
}

interface UriTokenEntry {
  absolutePath: string;
  resourceUrl: string;
  directory: string;
  sessionID?: string;
  contentType?: string;
  size?: number;
  immutable?: boolean;
  issuedAt: number;
  expiresAt: number;
  maxReads: number;
  reads: number;
}

export class UriTokenService {
  ttlMs: number;
  maxReads: number;
  now: () => number;

  /** @param {{ ttlMs?: number, maxReads?: number, now?: () => number }} [options] */
  constructor({ ttlMs = 10 * 60 * 1000, maxReads = 32, now = () => Date.now() }: UriTokenOptions = {}) {
    this.ttlMs = ttlMs;
    this.maxReads = maxReads;
    this.now = now;
  }

  #entries = new Map<string, UriTokenEntry>();

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
    this.#sweep();
    return { id, expiresAt };
  }

  #sweep(): void {
    const now = this.now();
    for (const [id, entry] of this.#entries) {
      if (entry.expiresAt <= now || entry.reads >= entry.maxReads) this.#entries.delete(id);
    }
  }

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

export interface UriResolveBody {
  u?: unknown;
  scheme?: unknown;
  ref?: unknown;
  sessionID?: unknown;
  directory?: unknown;
  pathOnly?: unknown;
}

/** Engine hook: local:// protocol options for one session. Cold paths
 * resolve the session file first, so async returns are part of the contract
 * (spec 04 §5.2.3: the hook may hit the SessionManager index). */
export type LocalOptionsForHook = (
  sessionID: string,
  directory: string,
) => LocalProtocolOptions | null | Promise<LocalProtocolOptions | null>;

export interface UriResolveInput {
  body?: UriResolveBody;
  localOptionsFor: LocalOptionsForHook;
  tokens: UriTokenService;
  router?: InternalUrlRouter;
}

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

export interface WireSessionRecord {
  id: string;
  title?: string;
  parentID?: string;
  /** Fork lineage (registry forkParentID): the wire parentID stays reserved
   * for subagent sessions; the session tree reads this instead. */
  forkParentID?: string;
  time?: { created?: number; updated?: number };
}

export type SessionTreeData = WireSessionRecord[] | { sessions?: WireSessionRecord[] } | null | undefined;

export interface SessionTreeNodeProjection {
  id: string;
  parentId: string | null;
  title: string;
  time: { created: number; updated: number };
}

export interface SessionTreeProjection {
  leafId: string | null;
  nodes: SessionTreeNodeProjection[];
}

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
const previewOf = (text: string | null | undefined, limit: number = 80): string => {
  const flat = String(text ?? '').replace(/\s+/g, ' ').trim();
  return flat.length > limit ? `${flat.slice(0, limit)}…` : flat;
};

export interface EntryLike {
  type: string;
  id: string;
  parentId?: string | null;
  timestamp?: string;
  message?: { role?: unknown; content?: unknown };
  summary?: string | null;
  mode?: unknown;
  targetId?: string;
  label?: string;
}

export interface EntryTreeNodeLike {
  entry?: EntryLike;
  children?: EntryTreeNodeLike[];
  label?: string;
}

export interface EntryTreeManagerLike {
  getTree?(): EntryTreeNodeLike[];
  getEntries?(): unknown[];
  getLeafId?(): string | null;
}

/** Entry-type-specific fields surfaced for one tree node ("gist"): role +
 *  preview for messages, toolName for tool-call blocks, mode for mode
 *  changes; absent entirely for types with nothing to show. */
export interface EntryNodeGist {
  role?: unknown;
  toolName?: string;
  preview?: string;
  mode?: unknown;
}

export interface EntrySnapshotNode {
  id: string;
  parentId: string | null;
  type: string;
  timestamp?: string;
  label?: string;
  gist?: EntryNodeGist;
}

export interface EntryTreeSnapshot {
  sessionID: string;
  directory: string | null;
  leafId: string | null;
  pathToLeaf: string[];
  revision: number;
  nodes: EntrySnapshotNode[];
}

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

/** §5.2/§5.4 event names live in omp-event-registry.json (chapter 05 owns
 *  the channel; this module is the producer). Payload never repeats the
 *  envelope's directory/sessionID. */
export const OMP_AGENTS_UPDATED = 'omp.agents.updated';
export const OMP_TREE_UPDATED = 'omp.tree.updated';

/** D04-4: streaming sessions reject navigate with 409 {busy:true}. */
export const navigateBusyResponse = () => json({ busy: true }, { status: 409 });

export interface NavigateRequestRaw {
  targetId?: string;
  summarize?: boolean;
  customInstructions?: string;
  allowAskReopen?: boolean;
  reanswerAskResult?: unknown;
}

export interface NavigateRequest {
  targetId: string;
  summarize: boolean;
  customInstructions: string | null;
  allowAskReopen: boolean;
  reanswerAskResult: unknown;
}

export type NavigateRequestResult =
  | { ok: true; value: NavigateRequest; response?: undefined }
  | { ok: false; response: Response; value?: undefined };

/**
 * navigateTree request contract (§5.4.2). Engine hook: validate with this,
 * then `agentSession.navigateTree(targetId, options)` (agent-session.ts:8273)
 * and publish OMP_TREE_UPDATED. Not wired here.
 * @returns {{ ok: true, value: object } | { ok: false, response: Response }}
 */
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

export type LabelRequestResult =
  | { ok: true; value: { targetId: string; label: unknown }; response?: undefined }
  | { ok: false; response: Response; value?: undefined };

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

export interface TreeUpdatedPayload {
  leafId: string | null;
  kind: string;
  entryId?: string;
}

/**
 * omp.tree.updated payload contract (§5.0/§5.4): light delta — the client
 * re-pulls GET /omp/sessions/{id}/tree on receipt. Engine publishes with
 * scope {directory, sessionID, durable: true} after navigateTree/label
 * commits. Not wired here.
 */
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

/** Row statuses — SDK AgentStatus plus the omp-host-only `historical`
 *  (cold-scan rows: transcript-view only, never revivable in-process). */
export const AGENT_RUN_STATUSES = ['running', 'idle', 'parked', 'aborted', 'historical'] as const;
export type AgentRunStatus = (typeof AGENT_RUN_STATUSES)[number];

const STATUS_ORDER = { running: 0, idle: 1, parked: 2, aborted: 3, historical: 4 } satisfies Record<AgentRunStatus, number>;

const agentRunKey = (sessionID: string, agentId: string): string => `${sessionID}::${agentId}`;

export interface AgentRunHistory {
  agent?: string;
  modelRole?: string;
  resolvedModel?: string;
  metrics?: unknown;
  readOnly?: boolean;
  outputPath?: string;
  patchPath?: string;
  branchName?: string;
}

export interface AgentRefLike {
  id: string;
  displayName?: string;
  kind?: string;
  parentId?: string;
  status?: string;
  /**
   * Live AgentSession while the agent is running (SDK: null exactly when
   * parked/aborted). Structurally narrowed to the public stats accessor —
   * the only surface the snapshot needs.
   */
  session?: {
    getSessionStats?: () => {
      tokens?: { total?: number };
      cost?: number;
    };
    /** The child session's own id (read-only drill-in resolves by it). */
    sessionId?: string;
  } | null;
  sessionFile?: string | null;
  createdAt?: number;
  lastActivity?: number;
  activity?: string;
  history?: AgentRunHistory;
}

/** Snapshot-time metrics for a running agent; absent once parked/aborted. */
export interface OmpAgentRunLive {
  tokens: number;
  cost: number;
  durationMs: number;
}

export interface OmpAgentRun {
  key: string;
  sessionID: string;
  directory: string;
  agentId: string;
  displayName: string;
  kind: string;
  parentId?: string;
  status: string;
  createdAt: number;
  lastActivity: number;
  /** The run's own session id — set when resolvable (live session or warmed cache). */
  childSessionID?: string;
  activity?: string;
  history?: AgentRunHistory;
  live?: OmpAgentRunLive;
  hasTranscript: boolean;
}

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
    key: agentRunKey(sessionID, ref.id),
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

export interface AgentsSnapshotEntry {
  sessionID: string;
  directory?: string;
  refs?: AgentRefLike[];
  registry?: { list(): AgentRefLike[] };
}

export interface DiskScanRow {
  sessionID: string;
  agentId: string;
  directory?: string;
  displayName?: string;
  kind?: string;
  parentId?: string;
  hasTranscript?: boolean;
  /** The run's own session id (transcript header) — read-only drill-in target. */
  childSessionID?: string;
  createdAt?: number;
  lastActivity?: number;
}

export interface AgentRunsPublishScope {
  directory: string;
  durable?: boolean;
}

export interface AgentRunsSnapshot {
  agentRuns: OmpAgentRun[];
  generatedAt: number;
  revision: number;
}

/** omp.agents.updated event payload (§5.5.1): the coalesced full-row
 *  snapshot + revision for one dirty directory. */
export interface AgentsUpdatedPayload {
  agentRuns: OmpAgentRun[];
  revision: number;
}

/** Timer handle returned by an injected setTimeout and handed back to the
 *  matching clearTimeout — Node's Timeout object or a browser-style numeric
 *  id (tests inject numeric handles). Stored opaquely, never inspected. */
export type AgentRunsTimerHandle = NodeJS.Timeout | number;

export interface AgentRunsAggregatorDeps {
  snapshot?: () => AgentsSnapshotEntry[] | null | undefined;
  publish?: (type: string, payload: AgentsUpdatedPayload, scope: AgentRunsPublishScope) => void;
  diskScan?: (directory: string) => DiskScanRow[] | null | undefined;
  /** Engine hook: fill its disk-scan cache for a directory (async, one shot). */
  warmDiskScan?: (directory: string) => Promise<void>;
  childSessionIdFor?: (sessionFile: string) => string | undefined;
  coalesceMs?: number;
  setTimeout?: (fn: () => void, ms?: number) => AgentRunsTimerHandle;
  clearTimeout?: (timer: AgentRunsTimerHandle) => void;
  now?: () => number;
}

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
    this.#diskScan = diskScan;
    this.#childSessionIdFor = childSessionIdFor;
    this.#warmDiskScan = warmDiskScan;
    this.#coalesceMs = coalesceMs;
    this.#setTimer = setTimer;
    this.#clearTimer = clearTimer;
    this.#now = now;
  }

  #snapshot: () => AgentsSnapshotEntry[] | null | undefined;
  #publish: (type: string, payload: AgentsUpdatedPayload, scope: AgentRunsPublishScope) => void;
  #diskScan: ((directory: string) => DiskScanRow[] | null | undefined) | undefined;
  #childSessionIdFor: ((sessionFile: string) => string | undefined) | undefined;
  #ensureDirectories = new Set<string>();
  #warmDiskScan: ((directory: string) => Promise<void>) | undefined;
  #coalesceMs: number;
  #setTimer: (fn: () => void, ms?: number) => AgentRunsTimerHandle;
  #clearTimer: (timer: AgentRunsTimerHandle) => void;
  #now: () => number;
  #rows = new Map<string, OmpAgentRun>();
  #revision = 0;
  #generatedAt = 0;
  #timer: AgentRunsTimerHandle | null = null;
  #dirty = new Set<string>();

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
        next.set(agentRunKey(entry.sessionID, ref.id), projectAgentRun({ sessionID: entry.sessionID, directory, ref, childSessionIdFor: this.#childSessionIdFor }));
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
          const key = agentRunKey(cold.sessionID, cold.agentId);
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

  /** GET /omp/agent-runs snapshot: { agentRuns, generatedAt, revision }. */
  snapshot(directory: string | null = null): AgentRunsSnapshot {
    return {
      agentRuns: this.#rowsFor(directory),
      generatedAt: this.#generatedAt,
      revision: this.#revision,
    };
  }

  /** One row by two-part address, or null. */
  row(sessionID: string, agentId: string): OmpAgentRun | null {
    return this.#rows.get(agentRunKey(sessionID, agentId)) ?? null;
  }

  /** Stop any pending publish (engine dispose hook). */
  dispose(): void {
    if (this.#timer) this.#clearTimer(this.#timer);
    this.#timer = null;
    this.#dirty = new Set();
  }
}

export interface ParkedAgentDescriptor {
  sessionID: string;
  agentId: string;
  ref?: AgentRefLike;
  revive: () => Promise<object>;
}

/**
 * In-memory revival descriptors for PARKED rows (R2-M5). Only in-process
 * parked agents are revivable; after a process restart every disk row is
 * historical (persistent descriptors are a separate P2 workstream). The
 * engine registers a descriptor when it parks a sub agent; the descriptor's
 * revive closure replays createAgentSession against the session's private
 * registry (ref-bound CAS, agent-registry.ts:170-174).
 */
export class ParkedAgentDescriptors {
  #map = new Map<string, ParkedAgentDescriptor>();

  /**
   * @param {{ sessionID: string, agentId: string, ref?: object,
   *           revive: () => Promise<object> }} descriptor
   */
  register({ sessionID, agentId, ref, revive }: ParkedAgentDescriptor): void {
    if (typeof revive !== 'function') throw new TypeError('descriptor.revive is required');
    this.#map.set(agentRunKey(sessionID, agentId), { sessionID, agentId, ref, revive });
  }

  has(sessionID: string, agentId: string): boolean {
    return this.#map.has(agentRunKey(sessionID, agentId));
  }

  /** Consume the descriptor (single claim — a failed revive re-registers). */
  claim(sessionID: string, agentId: string): ParkedAgentDescriptor | null {
    const key = agentRunKey(sessionID, agentId);
    const descriptor = this.#map.get(key);
    if (descriptor) this.#map.delete(key);
    return descriptor ?? null;
  }

  get size() {
    return this.#map.size;
  }
}

/**
 * Jobs snapshot shape (§5.6) — { ownerSessionID, running, recent, delivery }
 * (= SDK AsyncJobSnapshot once the upstream injection point lands, R12).
 * handleJobsRequest spreads it over its own ownerSessionID into the 200
 * body; the rows themselves stay opaque to this module.
 */
export interface JobsSnapshot {
  ownerSessionID?: string | null;
  running?: unknown[];
  recent?: unknown[];
  delivery?: unknown;
}

export interface AgentRunActions {
  revive?: (descriptor: ParkedAgentDescriptor, row: OmpAgentRun) => Promise<void> | void;
  kill?: (row: OmpAgentRun) => Promise<void> | void;
  chat?: (row: OmpAgentRun, message: { text: string; mode: 'prompt' | 'steer' }) => Promise<void> | void;
  jobsSnapshot?: (ownerSessionID: string | null, recentLimit: number) => Promise<JobsSnapshot>;
}

export interface AgentRunActionBody {
  kind?: unknown;
  text?: unknown;
  mode?: unknown;
}

export interface AgentRunActionInput {
  aggregator: { row(sessionID: string, agentId: string): OmpAgentRun | null };
  descriptors: ParkedAgentDescriptors;
  actions: AgentRunActions;
  sessionID: string;
  agentId: string;
  directory?: string | null;
  body?: AgentRunActionBody;
}

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
  const row = aggregator.row(sessionID, agentId);
  if (!row) return json({ error: 'agent-run-not-found' }, { status: 404 });
  if (directory && normalizeDirectoryKey(directory) !== row.directory) {
    return json({ error: 'agent-run-not-found' }, { status: 404 });
  }
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
    const descriptor = descriptors.claim(sessionID, agentId);
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
      const descriptor = descriptors.claim(sessionID, agentId);
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

export const JOBS_UNAVAILABLE_REASON = 'sdk-single-manager';

export interface JobsRequestInput {
  liveSessionIds?: string[];
  jobsEnabled?: boolean;
  snapshot?: (ownerSessionID: string | null, recentLimit: number) => Promise<JobsSnapshot>;
  recentLimit?: number;
}

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

/** Max file rows reported per session; the engine walk stops one past this
 *  so `truncated` is exact and no listing response grows unbounded. */
export const ARTIFACTS_MAX_FILES_PER_SESSION = 2000;

type ArtifactsFileRow = { ref?: unknown; size?: unknown; modifiedAt?: unknown };

const normalizedArtifactsRows = (files: ArtifactsFileRow[] | null | undefined) =>
  (Array.isArray(files) ? files : [])
    .filter((file): file is ArtifactsFileRow & { ref: string } => file !== null && file !== undefined && typeof file.ref === 'string' && file.ref.length > 0)
    .map((file) => ({
      ref: file.ref,
      size: Number(file.size) || 0,
      modifiedAt: Number(file.modifiedAt) || 0,
    }));

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

/** Parsed JSON body accepted by this domain's POST routes (superset; each
 * handler reads only its own fields, defensively). */
export interface UriRequestBody {
  u?: unknown;
  scheme?: unknown;
  ref?: unknown;
  sessionID?: unknown;
  directory?: unknown;
  pathOnly?: unknown;
  token?: string | null;
  kind?: unknown;
  text?: unknown;
  mode?: unknown;
}

const readJsonBody = async (request: Request): Promise<UriRequestBody> => {
  try {
    // SAFETY: handlers below runtime-validate the fields they read.
    return (await request.json()) as UriRequestBody;
  } catch {
    return {};
  }
};

/** directoryOf sources: query params, x-opencode-directory header, or a
 * JSON body field (resolve/open/action routes). */
interface DirectorySource {
  query?: { get(key: string): string | null } | null;
  headers?: { get(name: string): string | null } | null;
  body?: { directory?: unknown } | null;
}

const directoryOf = ({ query, headers, body }: DirectorySource = {}): string | null => {
  const fromBody = body && typeof body.directory === 'string' ? body.directory : null;
  const fromQuery = query?.get('directory');
  const fromHeader = headers?.get?.('x-opencode-directory');
  const raw = fromBody ?? fromQuery ?? (fromHeader ? decodeURIComponent(fromHeader) : null);
  return raw ? normalizeDirectoryKey(raw) : null;
};

/** Token query for uri.info: URLSearchParams (mount) or a plain {token}
 * carrier. */
export interface UriTokenQueryLike {
  get?(key: string): string | null;
  token?: string | null;
}

/** Injected engine hooks + overridable services for createUriDomain. Every
 * member is optional; routes fail loudly when a required hook is absent
 * (see createUriDomain JSDoc for the per-hook contracts). */
/** One session's local:// artifacts listing (engine #listLocalFiles). */
export interface ArtifactsListResult {
  files: Array<{ ref: string; size?: number; modifiedAt?: number }>;
  truncated?: boolean;
}

export interface UriDomainDeps {
  features?: () => OmpFeatures;
  tokens?: UriTokenService;
  descriptors?: ParkedAgentDescriptors;
  router?: InternalUrlRouter;
  localOptionsFor?: LocalOptionsForHook;
  sessionTreeData?: (directory: string | null) => Promise<SessionTreeData>;
  entryTreeFor?: (sessionID: string, directory: string | null) => Promise<{ manager: EntryTreeManagerLike } | null>;
  diskScan?: (directory: string) => DiskScanRow[] | null | undefined;
  /** Engine hook: fill its disk-scan cache for a directory (async, one shot). */
  warmDiskScan?: (directory: string) => Promise<void>;
  childSessionIdFor?: (sessionFile: string) => string | undefined;
  publish?: (type: string, payload: AgentsUpdatedPayload, scope: AgentRunsPublishScope) => void;
  agentsSnapshot?: () => AgentsSnapshotEntry[];
  liveSessionIds?: () => string[];
  actions?: AgentRunActions;
  /** Per-session local:// file rows (artifacts.v1); null = unknown session. */
  localFiles?: (sessionID: string, directory: string) => ArtifactsListResult | null | Promise<ArtifactsListResult | null>;
}

/** Route-registration context supplied by the host router (host.ts fetch:
 * {params, url, headers}); url/headers optional for direct handler calls. */
export interface UriRouteContext {
  params: Record<string, string>;
  url?: URL;
  headers?: Headers;
}

export type UriRouteHandler = (request: Request, ctx?: UriRouteContext) => Response | Promise<Response>;

/** mount() target — same shape as DomainRoute in domain-plugins.ts. */
export type UriRoute = (method: string, pattern: string, handler: UriRouteHandler) => void;

/** The chapter-04 domain surface returned by createUriDomain: consumed by
 * engine.uriDomain (mount/dispose) and by endpoints/tests (handler cores). */
export interface UriDomain {
  tokens: UriTokenService;
  aggregator: AgentRunsAggregator;
  descriptors: ParkedAgentDescriptors;
  artifacts: { list: (input: { directory?: string | null; sessionID?: string | null }) => Response | Promise<Response> };
  uri: {
    resolve: (input: { body?: UriResolveBody | null }) => Promise<Response>;
    open: (input: { body?: { token?: string | null } | null; directory?: string | null }) => Promise<Response>;
    info: (input: { query?: UriTokenQueryLike | null; directory?: string | null }) => Response;
    /** Raw bytes endpoint (GET /omp/uri/tokens/{id}/content): streams the
     * token's file body with its stored content type. */
    content: (input: { id?: string | null; directory?: string | null }) => Promise<Response>;
  };
  tree: {
    sessionTree: (input: { directory?: string | null }) => Promise<SessionTreeProjection>;
    entryTree: (input: { sessionID: string; directory?: string | null }) => Promise<Response>;
  };
  agentRuns: {
    list: (input: { directory?: string | null }) => Promise<Response>;
    action: (input: { sessionID?: string; agentId?: string; directory?: string | null; body?: AgentRunActionBody | null }) => Promise<Response>;
  };
  jobs: (input: { recentLimit?: number }) => Promise<Response>;
  mount: (route: UriRoute) => void;
  dispose: () => void;
}

/**
 * Build the chapter-04 domain. Engine hooks (all injected, none wired here):
 * - localOptionsFor(sessionID, directory) → LocalProtocolOptions | null
 *     build with createLocalProtocolOptions(sessionID, directory,
 *     liveSessionManagerOrColdArtifactsDir). Used at BOTH the resolve
 *     endpoint and #materialize (sdk createAgentSession localProtocolOptions).
 * - sessionTreeData(directory) → wire session records (engine.listSessions).
 * - entryTreeFor(sessionID, directory) → { manager } | null (live session's
 *     SessionManager, or cold read-only SessionManager.open — no agent).
 * - agentsSnapshot() → [{ sessionID, directory, registry }] (private
 *     registries of live host sessions; engine retains them at #materialize).
 * - diskScan(directory) → cold rows for historical projection (optional).
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
  const flags = (): Record<string, boolean> => {
    const source = features?.() ?? features;
    // SAFETY: OmpFeatures members are boolean flags (omp-parity contract).
    return (source ?? {}) as Record<string, boolean>;
  };
  const featureOn = (key: string) => Boolean(flags()[key]);

  const runs = new AgentRunsAggregator({
    snapshot: () => (typeof agentsSnapshot === 'function' ? agentsSnapshot() : []),
    ...(publish ? { publish } : { publish: () => {} }),
    ...(diskScan ? { diskScan } : {}),
    ...(childSessionIdFor ? { childSessionIdFor } : {}),
    ...(warmDiskScan ? { warmDiskScan } : {}),
  });

  const gate = (key: string) => (featureOn(key) ? null : featureUnavailable(key));

  const uriResolve = async ({ body }: { body?: UriResolveBody | null }) => {
    if (typeof localOptionsFor !== 'function') {
      // Only reachable with uri.v1 flipped on but no engine hook wired.
      return json({ error: 'hook-unavailable', hook: 'localOptionsFor' }, { status: 500 });
    }
    return handleUriResolve({ body: body ?? undefined, localOptionsFor, tokens, ...(router ? { router } : {}) });
  };
  const artifactsList = ({ directory, sessionID }: { directory?: string | null; sessionID?: string | null }) => {
    if (typeof localFiles !== 'function') {
      // Only reachable with artifacts flipped on but no engine hook wired.
      return json({ error: 'hook-unavailable', hook: 'localFiles' }, { status: 500 });
    }
    return handleArtifactsList({ directory, sessionID, filesFor: localFiles });
  };
  const uriOpen = async ({ body, directory }: { body?: UriRequestBody | null; directory?: string | null }) => {
    const token = typeof body?.token === 'string' ? body.token : undefined;
    return tokens.open(token, { directory: directory ?? undefined });
  };
  const uriInfo = ({ query, directory }: { query?: UriTokenQueryLike | null; directory?: string | null }) => {
    const token = query?.get?.('token') ?? query?.token ?? undefined;
    return tokens.describe(token, { directory: directory ?? undefined });
  };
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
  const sessionTree = async ({ directory }: { directory?: string | null }) =>
    buildSessionTree((await sessionTreeData?.(directory ?? null)) ?? []);
  const entryTree = async ({ sessionID, directory }: { sessionID: string; directory?: string | null }) => {
    const found = await entryTreeFor?.(sessionID, directory ?? null);
    if (!found?.manager) return json({ error: 'session-not-found' }, { status: 404 });
    return json(buildEntryTreeSnapshot({ sessionID, directory: directory ?? null, manager: found.manager }));
  };
  const agentRuns = async ({ directory }: { directory?: string | null }) => {
    // Historical rows surface lazily: warm the engine's disk cache for the
    // requested directory first (no-op once scanned), then answer.
    if (directory) await runs.ensureDirectory(directory);
    return json(runs.snapshot(directory ?? null));
  };
  const agentRunAction = ({ sessionID, agentId, directory, body }: { sessionID?: string; agentId?: string; directory?: string | null; body?: AgentRunActionBody | null }) =>
    handleAgentRunAction({ aggregator: runs, descriptors, actions, sessionID: sessionID ?? '', agentId: agentId ?? '', directory, body: body ?? {} });
  const jobs = ({ recentLimit }: { recentLimit?: number }) =>
    handleJobsRequest({
      liveSessionIds: liveSessionIds(),
      jobsEnabled: featureOn('jobs.v1'),
      ...(actions.jobsSnapshot ? { snapshot: actions.jobsSnapshot } : {}),
      recentLimit,
    });

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
