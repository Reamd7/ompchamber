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

import fs from 'node:fs';
import path from 'node:path';
import os from 'node:os';
import crypto from 'node:crypto';

const REGISTRY_FILE = 'ompchamber-session-meta.json';

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

const defaultAgentDir = () => path.join(os.homedir(), '.omp', 'agent');

export interface SessionMeta {
  title?: string;
  parentID?: string;
  /** Fork lineage (engine.fork writes it): wire `parentID` stays reserved
   * for subagent sessions; the session-tree projection reads this. */
  forkParentID?: string;
  persona?: string;
  agent?: string;
  model?: string;
  timeCreated?: number;
  timeUpdated?: number;
  timeArchived?: number;
  metadata?: Record<string, SessionMetadataValue>;
  revert?: { messageID: string; previousLeaf: string };
}

/** User-authored session metadata: arbitrary JSON, opaque to the engine. */
export type SessionMetadataValue =
  | string
  | number
  | boolean
  | null
  | SessionMetadataValue[]
  | { [key: string]: SessionMetadataValue };

export interface SessionMetaRegistryOptions {
  agentDir?: string;
}

export class SessionMetaRegistry {
  agentDir: string;
  registryRoot: string;
  cache: Map<string, Map<string, SessionMeta>>;

  /**
   * @param {object} [options]
   * @param {string} [options.agentDir] omp agent directory override.
   */
  constructor({ agentDir }: SessionMetaRegistryOptions = {}) {
    this.agentDir = agentDir || process.env.OMP_AGENT_DIR || defaultAgentDir();
    this.registryRoot = path.join(this.agentDir, 'ompchamber-registry');
    this.cache = new Map();
  }

  #registryPath(directoryKey: string): string {
    const digest = crypto.createHash('sha256').update(directoryKey).digest('hex').slice(0, 24);
    return path.join(this.registryRoot, digest + '.json');
  }

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
      if (typeof meta?.parentID === 'string' && meta.parentID.length > 0 && !('forkParentID' in meta)) {
        const { parentID, ...rest } = meta;
        map.set(id, { ...rest, forkParentID: parentID });
        migrated = true;
      }
    }
    this.cache.set(directoryKey, map);
    if (migrated) this.#persist(directoryKey, map);
    return map;
  }

  #persist(directoryKey: string, map: Map<string, SessionMeta>): void {
    const file = this.#registryPath(directoryKey);
    fs.mkdirSync(this.registryRoot, { recursive: true });
    const temp = file + '.' + process.pid + '.tmp';
    fs.writeFileSync(temp, JSON.stringify(Object.fromEntries(map), null, 2));
    fs.renameSync(temp, file);
  }

  /** Metadata for one session, or null. */
  get(directoryKey: string, sessionId: string): SessionMeta | null {
    return this.#load(normalizeDirectoryKey(directoryKey)).get(sessionId) ?? null;
  }

  /** Merge-update metadata for one session and persist. */
  update(directoryKey: string, sessionId: string, patch: SessionMeta): SessionMeta | undefined {
    const key = normalizeDirectoryKey(directoryKey);
    const map = this.#load(key);
    const current = map.get(sessionId) ?? {};
    map.set(sessionId, { ...current, ...patch });
    this.#persist(key, map);
    return map.get(sessionId);
  }

  /** Remove one session's metadata. */
  remove(directoryKey: string, sessionId: string): void {
    const key = normalizeDirectoryKey(directoryKey);
    const map = this.#load(key);
    if (!map.delete(sessionId)) return;
    this.#persist(key, map);
  }

  /** All metadata entries for a directory (sessionId -> meta). */
  entries(directoryKey: string): Map<string, SessionMeta> {
    return this.#load(normalizeDirectoryKey(directoryKey));
  }

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
