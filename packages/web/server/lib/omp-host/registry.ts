// Per-project sidecar registry for OMPChamber-specific session metadata.
//
// The omp engine (via `SessionManager`) owns session transcripts on disk:
// one JSONL per session under the cwd-derived session directory. OpenCode's
// wire model carries metadata omp does not persist (time.archived, parentID,
// wire-level titles overriding omp titles, revert pointers, per-session
// model/agent selections, custom agents, config passthrough). This registry
// stores that metadata next to the omp session directory in one JSON file per
// project directory, keyed by omp session id.
//
// Parent linkage is two distinct contracts:
// - `parentID` — subagent parentage (wire POST /session body.parentID). The
//   shared UI treats a wire session with parentID as a subagent session
//   (read-only composer, hidden from the switcher, listed under the parent's
//   work-status subagents).
// - `forkParentID` — fork lineage for the session-tree projection (§5.4).
//   A user fork is a normal promptable session, so it must NOT carry
//   parentID. engine.fork is the only writer.
//
// Invariants:
// - Registry writes are atomic (temp file + rename) so a crash never leaves
//   a torn index.
// - Unknown/missing omp session files simply drop out of listings; registry
//   entries without a transcript are lazily pruned on directory scans.
/**
 * OMPChamber 专属会话元数据的按项目 sidecar 注册表。
 *
 * omp 引擎（SessionManager）拥有磁盘上的会话转录（每个会话一个
 * JSONL）；OpenCode 线缆模型还携带 omp 不持久化的元数据（time.archived、
 * parentID、覆盖 omp 标题的 wire 标题、revert 指针、每会话 model/agent
 * 选择、自定义 agent、config 透传）。本注册表把这些元数据按项目目录
 * 存成单个 JSON 文件，以 omp session id 为键。父子关系是两个不同契约：
 * parentID 仅表 subagent 从属（wire POST /session）；forkParentID 表
 * fork 谱系（仅 engine.fork 写入，会话树投影读取）。不变量：写入原子
 * （临时文件 + rename，崩溃不留撕裂索引）；无转录的注册项在目录扫描时
 * 惰性清出列表。
 */

import fs from 'node:fs';
import path from 'node:path';
import os from 'node:os';
import crypto from 'node:crypto';

/** 每个项目目录的注册表文件名（存于 registryRoot 下）。 */
const REGISTRY_FILE = 'ompchamber-session-meta.json';

/** 目录键归一化：反斜杠→正斜杠、盘符大写、去尾部斜杠——注册表与
 *  live registry 共用的唯一目录规范形。 */
export const normalizeDirectoryKey = (directory: string | null | undefined): string => {
  let normalized = String(directory ?? '').replaceAll('\\', '/');
  if (normalized.length >= 2 && normalized[1] === ':') {
    normalized = normalized[0].toUpperCase() + normalized.slice(1);
  }
  if (normalized.length > 1 && normalized.endsWith('/')) {
    normalized = normalized.slice(0, -1);
  }
  return normalized;
};

/** 默认 omp agent 目录：~/.omp/agent。 */
const defaultAgentDir = () => path.join(os.homedir(), '.omp', 'agent');

/** OMPChamber 侧补充的会话元数据（与 omp 转录并存，合并语义见 update）。 */
export interface SessionMeta {
  /** wire 层覆盖的会话标题。 */
  title?: string;
  /** subagent 父会话 id（wire POST /session 的 body.parentID 语义）。 */
  parentID?: string;
  /** fork 谱系父 id（engine.fork 专写）；会话树投影读取，用户 fork
   *  不携带 parentID，保持可正常对话。 */
  /** Fork lineage (engine.fork writes it): wire `parentID` stays reserved
   * for subagent sessions; the session-tree projection reads this. */
  forkParentID?: string;
  /** 会话 persona 选择。 */
  persona?: string;
  /** 会话 agent 选择（含自定义 agent 名）。 */
  agent?: string;
  /** 会话 model 选择（provider/id 形式）。 */
  model?: string;
  /** 创建时间（epoch 毫秒）。 */
  timeCreated?: number;
  /** 最后更新时间（epoch 毫秒）。 */
  timeUpdated?: number;
  /** 归档时间（epoch 毫秒；存在即归档）。 */
  timeArchived?: number;
  /** 用户自定 metadata：任意 JSON，引擎不解释。 */
  metadata?: Record<string, SessionMetadataValue>;
  /** revert 指针：回到 messageID，先前叶子为 previousLeaf。 */
  revert?: { messageID: string; previousLeaf: string };
}

/** 用户自定会话元数据的 JSON 值域（引擎侧不透明）。 */
/** User-authored session metadata: arbitrary JSON, opaque to the engine. */
export type SessionMetadataValue =
  | string
  | number
  | boolean
  | null
  | SessionMetadataValue[]
  | { [key: string]: SessionMetadataValue };

/** SessionMetaRegistry 构造选项。 */
export interface SessionMetaRegistryOptions {
  /** omp agent 目录覆盖；缺省依次取 OMP_AGENT_DIR 与 ~/.omp/agent。 */
  agentDir?: string;
}

/** 按项目目录组织的会话元数据注册表：内存缓存 + 原子 JSON 持久化。 */
export class SessionMetaRegistry {
  /** 生效的 omp agent 目录。 */
  agentDir: string;
  /** 注册表根目录（agentDir/ompchamber-registry）。 */
  registryRoot: string;
  /** 目录键 →（session id → meta）的内存缓存。 */
  cache: Map<string, Map<string, SessionMeta>>;

  /** 构造：确定 agentDir（参数 > OMP_AGENT_DIR > 默认）、注册表根与空缓存。 */
  /**
   * @param {object} [options]
   * @param {string} [options.agentDir] omp agent directory override.
   */
  constructor({ agentDir }: SessionMetaRegistryOptions = {}) {
    this.agentDir = agentDir || process.env.OMP_AGENT_DIR || defaultAgentDir();
    this.registryRoot = path.join(this.agentDir, 'ompchamber-registry');
    this.cache = new Map();
  }

  /** 目录键的注册表文件路径：sha256 前 24 个十六进制字符 + .json，
   *  避免目录名直接成为文件系统路径。 */
  #registryPath(directoryKey: string): string {
    const digest = crypto.createHash('sha256').update(directoryKey).digest('hex').slice(0, 24);
    return path.join(this.registryRoot, digest + '.json');
  }

  /** 加载并缓存某目录的元数据表：读不到文件按空表处理；同时执行一次
   *  parentID→forkParentID 迁移（历史版本把 fork 谱系写进了 parentID），
   *  迁移命中则立即持久化。 */
  #load(directoryKey: string): Map<string, SessionMeta> {
    const cached = this.cache.get(directoryKey);
    if (cached) return cached;
    let entries: Record<string, SessionMeta> = {};
    try {
      entries = JSON.parse(fs.readFileSync(this.#registryPath(directoryKey), 'utf8'));
    } catch {
      entries = {};
    }
    const map = new Map(Object.entries(entries));
    // One-time migration: every released version wrote fork lineage into
    // `parentID` (engine.fork was its only writer), which made user forks
    // project as read-only subagent sessions on the wire. Move those edges
    // to `forkParentID`. Entries that already carry `forkParentID` keep any
    // `parentID` untouched — that pairing means genuine subagent parentage.
    let migrated = false;
    for (const [id, meta] of map) {
      if (meta?.parentID !== undefined && meta.parentID.length > 0 && !('forkParentID' in meta)) {
        const { parentID, ...rest } = meta;
        map.set(id, { ...rest, forkParentID: parentID });
        migrated = true;
      }
    }
    this.cache.set(directoryKey, map);
    if (migrated) this.#persist(directoryKey, map);
    return map;
  }

  /** 原子持久化：先写 `file.pid.tmp` 临时文件再 rename，崩溃不会留下
   *  撕裂的索引 JSON。 */
  #persist(directoryKey: string, map: Map<string, SessionMeta>): void {
    const file = this.#registryPath(directoryKey);
    fs.mkdirSync(this.registryRoot, { recursive: true });
    const temp = file + '.' + process.pid + '.tmp';
    fs.writeFileSync(temp, JSON.stringify(Object.fromEntries(map), null, 2));
    fs.renameSync(temp, file);
  }

  /** 读取单个会话的元数据；不存在返回 null。 */
  /** Metadata for one session, or null. */
  get(directoryKey: string, sessionId: string): SessionMeta | null {
    return this.#load(normalizeDirectoryKey(directoryKey)).get(sessionId) ?? null;
  }

  /** 合并更新单个会话的元数据（浅合并 patch）并持久化，返回合并结果。 */
  /** Merge-update metadata for one session and persist. */
  update(directoryKey: string, sessionId: string, patch: SessionMeta): SessionMeta | undefined {
    const key = normalizeDirectoryKey(directoryKey);
    const map = this.#load(key);
    const current = map.get(sessionId) ?? {};
    map.set(sessionId, { ...current, ...patch });
    this.#persist(key, map);
    return map.get(sessionId);
  }

  /** 删除单个会话的元数据；不存在则不做任何写盘。 */
  /** Remove one session's metadata. */
  remove(directoryKey: string, sessionId: string): void {
    const key = normalizeDirectoryKey(directoryKey);
    const map = this.#load(key);
    if (!map.delete(sessionId)) return;
    this.#persist(key, map);
  }

  /** 取整个目录的元数据表（session id → meta，含缓存实例）。 */
  /** All metadata entries for a directory (sessionId -> meta). */
  entries(directoryKey: string): Map<string, SessionMeta> {
    return this.#load(normalizeDirectoryKey(directoryKey));
  }

  /** 目录静默时丢弃其内存缓存（plan §6）：JSON 文件仍是权威，下次访问
   *  重新加载；缓存只保留仍持有 live 状态的目录。 */
  /**
   * Drop one directory's in-memory map when it goes quiet (plan §6): the
   * JSON file stays authoritative, so the next access simply reloads. Bound
   * the cache to directories that still hold live state.
   */
  release(directoryKey: string): void {
    this.cache.delete(normalizeDirectoryKey(directoryKey));
  }

  /** 把一个会话的元数据搬到另一目录（worktree/控制面移动）；
   *  返回搬走的元数据或 null（同目录时等价于 get）。 */
  /**
   * Move a session's metadata to another directory (worktree/control-plane
   * moves). Returns the previous metadata or null.
   */
  move(fromDirectory: string, toDirectory: string, sessionId: string): SessionMeta | null {
    const from = normalizeDirectoryKey(fromDirectory);
    const to = normalizeDirectoryKey(toDirectory);
    if (from === to) return this.get(from, sessionId);
    const fromMap = this.#load(from);
    const meta = fromMap.get(sessionId);
    if (!meta) return null;
    fromMap.delete(sessionId);
    this.#persist(from, fromMap);
    const toMap = this.#load(to);
    toMap.set(sessionId, meta);
    this.#persist(to, toMap);
    return meta;
  }
}
