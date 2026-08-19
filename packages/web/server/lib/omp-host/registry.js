// Per-project sidecar registry for OpenChamber-specific session metadata.
//
// The omp engine (via `SessionManager`) owns session transcripts on disk:
// one JSONL per session under the cwd-derived session directory. OpenCode's
// wire model carries metadata omp does not persist (time.archived, parentID,
// wire-level titles overriding omp titles, revert pointers, per-session
// model/agent selections, custom agents, config passthrough). This registry
// stores that metadata next to the omp session directory in one JSON file per
// project directory, keyed by omp session id.
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

const REGISTRY_FILE = 'openchamber-session-meta.json';

export const normalizeDirectoryKey = (directory) => {
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

export class SessionMetaRegistry {
  /**
   * @param {object} [options]
   * @param {string} [options.agentDir] omp agent directory override.
   */
  constructor({ agentDir } = {}) {
    this.agentDir = agentDir || process.env.OMP_AGENT_DIR || defaultAgentDir();
    this.registryRoot = path.join(this.agentDir, 'openchamber-registry');
    /** @type {Map<string, Map<string, Record<string, unknown>>>} directoryKey -> sessionId -> meta */
    this.cache = new Map();
  }

  #registryPath(directoryKey) {
    const digest = crypto.createHash('sha256').update(directoryKey).digest('hex').slice(0, 24);
    return path.join(this.registryRoot, digest + '.json');
  }

  #load(directoryKey) {
    const cached = this.cache.get(directoryKey);
    if (cached) return cached;
    let entries = {};
    try {
      entries = JSON.parse(fs.readFileSync(this.#registryPath(directoryKey), 'utf8'));
    } catch {
      entries = {};
    }
    const map = new Map(Object.entries(entries));
    this.cache.set(directoryKey, map);
    return map;
  }

  #persist(directoryKey, map) {
    const file = this.#registryPath(directoryKey);
    fs.mkdirSync(this.registryRoot, { recursive: true });
    const temp = file + '.' + process.pid + '.tmp';
    fs.writeFileSync(temp, JSON.stringify(Object.fromEntries(map), null, 2));
    fs.renameSync(temp, file);
  }

  /** Metadata for one session, or null. */
  get(directoryKey, sessionId) {
    return this.#load(normalizeDirectoryKey(directoryKey)).get(sessionId) ?? null;
  }

  /** Merge-update metadata for one session and persist. */
  update(directoryKey, sessionId, patch) {
    const key = normalizeDirectoryKey(directoryKey);
    const map = this.#load(key);
    const current = map.get(sessionId) ?? {};
    map.set(sessionId, { ...current, ...patch });
    this.#persist(key, map);
    return map.get(sessionId);
  }

  /** Remove one session's metadata. */
  remove(directoryKey, sessionId) {
    const key = normalizeDirectoryKey(directoryKey);
    const map = this.#load(key);
    if (!map.delete(sessionId)) return;
    this.#persist(key, map);
  }

  /** All metadata entries for a directory (sessionId -> meta). */
  entries(directoryKey) {
    return this.#load(normalizeDirectoryKey(directoryKey));
  }

  /**
   * Move a session's metadata to another directory (worktree/control-plane
   * moves). Returns the previous metadata or null.
   */
  move(fromDirectory, toDirectory, sessionId) {
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
