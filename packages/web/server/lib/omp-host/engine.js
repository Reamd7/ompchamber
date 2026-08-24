// omp engine manager: embeds @oh-my-pi/pi-coding-agent sessions behind the
// OpenCode-compatible wire surface.
//
// One HostSession per OpenChamber session id. Transcripts live in omp's
// SessionManager JSONL files (cwd-derived directory); OpenChamber-specific
// metadata lives in the sidecar registry. Cold reads project the persisted
// transcript without materializing an agent; the first prompt (or any live
// operation) materializes a full AgentSession whose event stream is projected
// into wire events on the host bus.

import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import crypto from 'node:crypto';
import {
  AgentRegistry,
  ModelRegistry,
  SessionManager,
  BUILTIN_TOOLS,
  createAgentSession,
  discoverAuthStorage,
} from '@oh-my-pi/pi-coding-agent';
import { discoverAgents, refreshAgentDiscovery } from '@oh-my-pi/pi-coding-agent/task';
import { getConfigDirs } from '@oh-my-pi/pi-coding-agent/config';
import { initializeExtensions } from '@oh-my-pi/pi-coding-agent/modes/runtime-init';
import { getSessionSlashCommands } from '@oh-my-pi/pi-coding-agent/extensibility/extensions/get-commands-handler';
import { SessionMetaRegistry, normalizeDirectoryKey } from './registry.js';
import { WireEventBus, OmpEventBus } from './events.js';
import {
  StreamProjector,
  normalizeToolExecutionResult,
  projectConversation,
  projectCustomMessage,
  projectDividerMessage,
  projectUserMessage,
  wireMessageId,
  deterministicWireId,
  resolveWireIdToEntryId,
  splitModelSelector,
  paginateProjectedMessages,
} from './projection.js';
import { createSettingsStore } from './domain-models.js';
import { createDomainDialogs } from './domain-dialogs.js';
import {
  ModeDomainError,
  createModesDomain,
  mapBackedStore,
  migrateSidecarAgents,
  personaFor,
  serializeAgentMarkdown,
} from './domain-modes.js';
import { createDomainChrome } from './domain-chrome.js';
import { ompFeatures } from './omp-parity.js';
import { revealCommand } from './domain-plugins.js';
import { createUriDomain, createLocalProtocolOptions, buildEntryTreeSnapshot } from './domain-uri.js';
const IDLE_SESSION_TTL_MS = 30 * 60 * 1000;
// Cap on concurrently live top-level sessions; idle sessions beyond this
// are swept after IDLE_SESSION_TTL_MS. Referenced by #sweepIdleSessions —
// it was missing entirely and killed the engine on the first 60s sweep.
const MAX_LIVE_SESSIONS = 16;

/**
 * Session-level persona key (02 §5.1 D-B3): unset and the deleted
 * build/plan pair map to the standard session; any other name is a persona.
 */
const personaKeyFor = (name) => (!name || name === 'build' || name === 'plan' ? 'standard' : name);

/** Wire `agent` projection: the standard session keeps the legacy 'build' id. */
const wireAgentFor = (personaKey) => (personaKey === 'standard' ? 'build' : personaKey);

const textOfContent = (content) => {
  if (typeof content === 'string') return content;
  if (!Array.isArray(content)) return '';
  return content.filter((b) => b && b.type === 'text').map((b) => b.text).join('');
};

const modelSelector = (model) => (model ? `${model.provider}/${model.id}` : undefined);

export class OmpHostEngine {
  /** In-flight #materialize dedup (`${directory}\0${sessionId}` → promise); prevents duplicate AgentSessions on lease/prompt races. */
  #materializeInFlight = new Map();

  constructor({ agentDir } = {}) {
    this.authStorage = null;
    this.modelRegistry = null;
    this.registry = new SessionMetaRegistry({ agentDir });
    this.bus = new WireEventBus();
    /** omp-native event channel (spec 05 §5.2, master D6-R1 single authority). */
    this.ompBus = new OmpEventBus();
    /** @type {Map<string, HostSession>} */
    this.sessions = new Map();
    /**
     * Canonical-read wire id -> client messageID echoed at prompt time.
     *
     * The wire contract requires the server to echo the client's messageID so
     * the optimistic UI message reconciles in place, but pi's persisted
     * UserMessage carries no id field to store it in. This map bridges the two:
     * prompt() records the pending client id, the user message_start event
     * captures pi's canonical message identity, and cold projections resolve
     * the same wire id both live and on re-fetch. Without it a re-fetch during
     * or after the turn projected a second, different id for the same message
     * and the UI rendered the user's prompt twice.
     * @type {Map<string, string>}
     */
    this.wireIdOverrides = new Map();
    /** Personas (OC-original optional layer, spec 02 §5.2/R12). */
    this.personas = new Map();
    /** Per-directory keyed Settings store (spec 06 §5.1, master R6). */
    this.settingsStore = null;
    // Approval/ask dialog domain (spec 03, master R10/R11/R13). Lease-driven
    // hasUI: unattended sessions never hold a lease → SDK fail-closed.
    this.dialogs = createDomainDialogs({
      onSessionUiAttached: ({ directory, sessionId }) => {
        void this.#attachDialogUi(directory, sessionId).catch((error) => {
          console.warn('[omp-host] failed to attach dialog UI:', error?.message ?? error);
        });
      },
      onSessionUiDetached: ({ directory, sessionId }) => this.#detachDialogUi(directory, sessionId),
      onDiagnostic: (note) => console.warn('[omp-host] dialog lifecycle:', note),
    });
    // Modes/plan/goal/personas/agent-definitions domain (spec 02).
    this.modesDomain = createModesDomain({
      publishFor: (sessionId, directoryKey) => (type, payload, options = {}) =>
        this.ompBus.publish(type, payload, {
          directory: directoryKey,
          sessionID: sessionId,
          durable: options.durable !== false,
        }),
      appendFor: (sessionId, directoryKey) => (mode, data) => {
        const hostSession = this.sessions.get(sessionId);
        const manager = hostSession?.agentSession?.sessionManager;
        if (!manager?.appendModeChange) return undefined;
        const entryId = manager.appendModeChange(mode, data);
        this.#syncPlanProposalHandler(hostSession, mode);
        return entryId;
      },
      sessionContextFor: (sessionId, directoryKey) => {
        const hostSession = this.sessions.get(sessionId);
        try {
          return hostSession?.agentSession?.sessionManager?.buildSessionContext?.();
        } catch {
          return undefined;
        }
      },
      // omp agent discovery chain as the definitions authority (02 §5.2):
      // reads come from discoverAgents (project > user > extensions >
      // bundled), writes are .md files in the user/project agents dirs.
      agentDefinitions: {
        discover: (directory) => discoverAgents(directory ?? process.cwd()),
        writeFile: async (filePath, content) => {
          await fs.promises.mkdir(path.dirname(filePath), { recursive: true });
          await fs.promises.writeFile(filePath, content, 'utf8');
        },
        deleteFile: async (filePath) => {
          try {
            await fs.promises.unlink(filePath);
            return true;
          } catch {
            return false;
          }
        },
        // Hot reload (02 §5.2 refresh): the SDK memoizes create-time discovery
        // per cwd and every task tool advertises that list to the model;
        // refreshAgentDiscovery republishes the fresh set to live sessions.
        onDefinitionsChanged: (directory) => refreshAgentDiscovery(directory ?? process.cwd()),
        // Reveal in file manager (plugins.v1 parity): reuse the plugins
        // domain's platform builder instead of a second opener implementation.
        revealFile: async (filePath) => {
          const { execFile } = await import('node:child_process');
          const { promisify } = await import('node:util');
          const { command, args } = revealCommand(process.platform, filePath);
          await promisify(execFile)(command, args, { windowsHide: true });
        },
        userAgentsDir: this.#userAgentsDir(),
        projectAgentsDirFor: (directory) => path.join(path.resolve(directory), '.omp', 'agents'),
      },
      personasStore: mapBackedStore(this.personas, () => this.savePersonas()),
      allowedTools: new Set(Object.keys(BUILTIN_TOOLS ?? {})),
      settingsProjectScopes: true,
      // Effective task.* override read for the definitions projection
      // (02 §5.2): the keyed Settings merged view per directory.
      overridesFor: async (directoryKey) => {
        const store = this.settingsStore;
        if (!store?.settingsFor) return null;
        try {
          const settings = await store.settingsFor(directoryKey);
          return {
            disabledAgents: settings.get('task.disabledAgents'),
            modelOverrides: settings.get('task.agentModelOverrides'),
            prewalk: settings.get('task.agentPrewalk'),
            advisor: settings.get('task.agentAdvisor'),
          };
        } catch {
          return null;
        }
      },
    });
    // Extension chrome table (spec 09 §5): string-payload widget/status
    // projection mirroring RpcExtensionUIRequest. Volatile events; the
    // snapshot GET is the reconnect authority (D2).
    this.chrome = createDomainChrome({
      publishFor: (directory, payload) =>
        this.ompBus.publish('omp.chrome.updated', payload, { directory, durable: false }),
    });
    // URI bridge / session tree / agent-runs / jobs (spec 04). The factory
    // is synchronous and every engine dependency is a lazy closure, so it is
    // created here (not in async #boot) and mounted synchronously by
    // endpoints.js at route-registration time.
    this.uriDomain = createUriDomain({
      features: () => ompFeatures(),
      localOptionsFor: (sessionId, directoryKey) =>
        createLocalProtocolOptions(sessionId, directoryKey, this.#sessionDirFor(directoryKey)),
      sessionTreeData: async (directory) => this.listSessions({ directory }),
      entryTreeFor: async (sessionID, directory) => {
        const file = await this.#findSessionFile(sessionID, normalizeDirectoryKey(directory));
        if (!file) return null;
        const manager = await SessionManager.open(file.path);
        return { manager };
      },
      agentsSnapshot: () =>
        [...this.sessions.values()]
          .filter((hostSession) => hostSession.agentRegistry && hostSession.agentSession)
          .map((hostSession) => ({
            sessionID: hostSession.sessionId,
            directory: hostSession.directory,
            registry: hostSession.agentRegistry,
          })),
      publish: (type, payload, scope) => this.ompBus.publish(type, payload, scope),
      liveSessionIds: () => [...this.sessions.keys()],
    });
    this.bootError = null;
    this.bootPromise = null;
    this.sweeper = setInterval(() => this.#sweepIdleSessions(), 60_000);
    this.sweeper.unref?.();
  }

  async #boot() {
    if (this.bootPromise) return this.bootPromise;
    this.bootPromise = (async () => {
      this.authStorage = await discoverAuthStorage(this.registry.agentDir);
      this.modelRegistry = new ModelRegistry(this.authStorage);
      await this.modelRegistry.refresh();
      this.#loadPersonas();
      // Sidecar → omp agent migration (02 §6.2) runs before the request
      // surface opens; failure keeps the sidecar and never blocks boot.
      await this.#migrateAgentsSidecar().catch((error) => {
        console.warn('[omp-host] agent sidecar migration failed:', error?.message ?? error);
      });
      // Per-directory keyed Settings store (spec 06 §5.1, master R6). The
      // boot instance doubles as the global-write executor; sessions inject
      // their directory's instance via options.settings (sdk.ts:1273-1275).
      if (!this.settingsStore) {
        try {
          this.settingsStore = await createSettingsStore({
            cwd: process.cwd(),
            agentDir: this.registry.agentDir,
          });
        } catch (error) {
          // Degrade to no-injection (pre-R6 behavior) instead of bricking
          // every session; the settings endpoints surface the error.
          console.warn('[omp-host] settings store unavailable:', error?.message ?? error);
          this.settingsStore = null;
        }
      }
    })();
    try {
      await this.bootPromise;
    } catch (error) {
      this.bootError = error;
      this.bootPromise = null;
      throw error;
    }
    return this.bootPromise;
  }

  async #setDialogUiContext(hostSession, directory, sessionId, hasUI) {
    const uiContext = hasUI
      ? this.dialogs.uiContextFor(directory, sessionId, {
          chrome: this.chrome.bridgeHandlersFor(directory, sessionId),
        })
      : undefined;
    if (hasUI && !hostSession.extensionUiInitialized) {
      if (!hostSession.extensionUiPromise) {
        hostSession.extensionUiPromise = initializeExtensions(hostSession.agentSession, {
          uiContext,
          mode: 'json',
          reportSendError: (action, error) => {
            console.warn(`[omp-host] ${action} failed:`, error?.message ?? error);
          },
          reportRuntimeError: (error) => {
            console.warn('[omp-host] extension runtime error:', error?.error ?? error);
          },
          onShutdown: () => {},
        }).then(() => {
          hostSession.extensionUiInitialized = true;
        }).finally(() => {
          hostSession.extensionUiPromise = null;
        });
      }
      await hostSession.extensionUiPromise;
    }
    hostSession.sdkResult.setToolUIContext(uiContext, hasUI);
  }

  /**
   * Lease attach/detach → SDK tool UI context (R13: lease is hasUI
   * authority). A UI lease IS a session access: a client viewing the session
   * implies the engine should hold it live, so an attach that races ahead of
   * lazy materialization pulls the session in instead of dropping the
   * extension UI initialization on the floor.
   */
  async #attachDialogUi(directory, sessionId) {
    const hostSession = this.sessions.get(sessionId)
      ?? await this.#materialize(sessionId, directory);
    if (!hostSession) return;
    await this.#setDialogUiContext(hostSession, directory, sessionId, true);
  }

  #detachDialogUi(directory, sessionId) {
    const hostSession = this.sessions.get(sessionId);
    if (!hostSession) return;
    try {
      hostSession.sdkResult?.setToolUIContext?.(undefined, false);
    } catch {
      // Session may already be disposed.
    }
  }

  /**
   * Plan mode ↔ xd://propose bridge (spec 02 §5.5): entering plan attaches
   * the review bridge; any other mode clears the handler.
   */
  #syncPlanProposalHandler(hostSession, mode) {
    if (!hostSession?.agentSession?.setPlanProposalHandler) return;
    if (mode === 'plan') {
      const bridge = this.modesDomain.bridgeFor(hostSession.sessionId, hostSession.directory);
      hostSession.agentSession.setPlanProposalHandler(bridge.hookFor(hostSession.agentSession));
      hostSession.planHandlerAttached = true;
    } else if (hostSession.planHandlerAttached) {
      hostSession.agentSession.setPlanProposalHandler(null);
      hostSession.planHandlerAttached = false;
    }
  }

  #personasConfigPath() {
    return path.join(this.registry.registryRoot, 'openchamber-personas.json');
  }

  #loadPersonas() {
    try {
      const parsed = JSON.parse(fs.readFileSync(this.#personasConfigPath(), 'utf8'));
      for (const persona of Array.isArray(parsed?.personas) ? parsed.personas : []) {
        if (persona && typeof persona.name === 'string') this.personas.set(persona.name, persona);
      }
    } catch {
      // No personas yet.
    }
  }

  savePersonas() {
    fs.mkdirSync(this.registry.registryRoot, { recursive: true });
    fs.writeFileSync(
      this.#personasConfigPath(),
      JSON.stringify({ personas: [...this.personas.values()] }, null, 2),
    );
  }

  /** Public settings-store accessor for endpoint handlers. */
  async settingsStoreReady() {
    await this.#boot();
    return this.settingsStore;
  }

  /**
   * omp user-scope agents dir (SDK discovery order: `~/.omp/agent/agents`,
   * pi-utils getConfigDirs with source '.omp'). Falls back to the derived
   * path when config dirs are unavailable.
   */
  #userAgentsDir() {
    try {
      const entry = getConfigDirs('agents', { project: false })
        .find((dir) => dir?.source === '.omp' && typeof dir?.path === 'string');
      if (entry) return entry.path;
    } catch {
      // Derived fallback below.
    }
    return path.join(os.homedir(), '.omp', 'agent', 'agents');
  }

  /**
   * One-time sidecar → omp migration (02 §6.2): each legacy
   * `openchamber-agents.json` record becomes a user-scope worker `.md`
   * (frontmatter description/tools, body prompt) plus a mirrored persona so
   * existing `meta.agent` sessions keep resolving. Runs before the request
   * surface opens; any failure keeps the sidecar for an idempotent retry.
   */
  async #migrateAgentsSidecar() {
    const sidecarPath = path.join(this.registry.registryRoot, 'openchamber-agents.json');
    const userAgentsDir = this.#userAgentsDir();
    let done = false;
    const result = await migrateSidecarAgents({
      loadRecords: () => {
        const parsed = JSON.parse(fs.readFileSync(sidecarPath, 'utf8'));
        return Array.isArray(parsed?.agents) ? parsed.agents : [];
      },
      agentExists: async (name) => {
        const { agents } = await discoverAgents(process.cwd());
        return agents.some((agent) => agent?.name === name);
      },
      writeAgent: async (record) => {
        await fs.promises.mkdir(userAgentsDir, { recursive: true });
        await fs.promises.writeFile(
          path.join(userAgentsDir, `${record.name}.md`),
          serializeAgentMarkdown({
            name: record.name,
            description: typeof record.description === 'string' && record.description.trim()
              ? record.description
              : record.name,
            systemPrompt: typeof record.prompt === 'string' ? record.prompt : '',
            ...(Array.isArray(record.tools) && record.tools.length > 0 ? { tools: record.tools } : {}),
          }),
          'utf8',
        );
      },
      personaExists: (name) => this.personas.has(name),
      mirrorPersona: (record) => {
        this.personas.set(record.name, {
          name: record.name,
          ...(record.description ? { description: record.description } : {}),
          ...(typeof record.prompt === 'string' && record.prompt ? { systemPrompt: record.prompt } : {}),
          ...(Array.isArray(record.tools) && record.tools.length > 0 ? { tools: record.tools } : {}),
        });
      },
      markDone: () => {
        try {
          fs.renameSync(sidecarPath, `${sidecarPath}.migrated-${Date.now()}`);
          this.savePersonas();
          done = true;
        } catch (error) {
          console.warn('[omp-host] sidecar migration markDone failed:', error?.message ?? error);
        }
      },
      log: (message, error) => console.warn('[omp-host] agent sidecar migration:', message, error ?? ''),
    });
    if (done && result.migrated > 0) {
      console.log(`[omp-host] migrated ${result.migrated} sidecar agent(s) to ${userAgentsDir} (+persona mirrors)`);
    }
    return result;
  }

  #sweepIdleSessions() {
    const now = Date.now();
    const entries = [...this.sessions.entries()];
    if (entries.length <= MAX_LIVE_SESSIONS) return;
    for (const [id, session] of entries) {
      if (session.agentSession?.isStreaming) continue;
      if (now - session.lastTouched < IDLE_SESSION_TTL_MS) continue;
      this.#disposeSession(session);
      this.sessions.delete(id);
      if (this.sessions.size <= MAX_LIVE_SESSIONS) break;
    }
  }

  #disposeSession(session) {
    if (!session.agentSession) return;
    session.unsubscribe?.();
    session.unsubscribe = null;
    // Domain release: modes tracker + pending dialogs for this session (R11).
    this.modesDomain?.release?.(session.sessionId, session.directory);
    this.dialogs?.registry?.abortForSession?.({
      directory: session.directory,
      sessionId: session.sessionId,
    }, 'session disposed');
    const agentSession = session.agentSession;
    session.agentSession = null;
    void agentSession.dispose().catch(() => {});
  }
  #sessionDirFor(cwd) {
    return SessionManager.getDefaultSessionDir(cwd, this.registry.agentDir);
  }

  #projectId(directoryKey) {
    const hash = crypto.createHash('sha256').update(directoryKey).digest('hex');
    return `prj_${hash.slice(0, 20)}`;
  }

  /**
   * Wire Session record for an omp SessionInfo + registry metadata.
   */
  #wireSession(info, directoryKey, meta, live) {
    // The live session's actual model wins over the sidecar projection — a
    // roles-resolved session (no registry selector) still reports the model
    // it is really running (spec 01 §5.5 badge seeding).
    const selector = meta?.model
      ? splitModelSelector(meta.model)
      : live?.agentSession?.model
        ? { providerID: live.agentSession.model.provider, modelID: live.agentSession.model.id }
        : null;
    return {
      id: info.id,
      slug: info.id,
      projectID: this.#projectId(directoryKey),
      directory: normalizeDirectoryKey(info.cwd || directoryKey),
      parentID: meta?.parentID,
      title: meta?.title ?? info.title ?? 'Untitled',
      ...(personaKeyFor(meta?.persona ?? meta?.agent) !== 'standard'
        ? { agent: wireAgentFor(personaKeyFor(meta?.persona ?? meta?.agent)) }
        : {}),
      ...(selector ? { model: { id: selector.modelID, providerID: selector.providerID } } : {}),
      ...(meta?.metadata ? { metadata: meta.metadata } : {}),
      time: {
        created: info.created instanceof Date ? info.created.getTime() : Date.parse(info.created) || Date.now(),
        updated: info.modified instanceof Date ? info.modified.getTime() : Date.parse(info.modified) || Date.now(),
        ...(meta?.timeArchived ? { archived: meta.timeArchived } : {}),
      },
    };
  }

  async listSessions({ directory }) {
    await this.#boot();
    const directoryKey = normalizeDirectoryKey(directory);
    const infos = await SessionManager.list(directory, this.#sessionDirFor(directory), undefined);
    const metas = this.registry.entries(directoryKey);
    const out = [];
    const seen = new Set();
    for (const info of infos) {
      seen.add(info.id);
      out.push(this.#wireSession(info, directoryKey, metas.get(info.id), this.sessions.get(info.id)));
    }
    // Registry-only sessions (omp transcript pruned externally) stay listed so
    // deletion/archival bookkeeping keeps working.
    for (const [id, meta] of metas) {
      if (seen.has(id)) continue;
      out.push(
        this.#wireSession(
          {
            id,
            cwd: directory,
            title: meta.title,
            created: new Date(meta.timeCreated ?? Date.now()),
            modified: new Date(meta.timeUpdated ?? Date.now()),
          },
          directoryKey,
          meta,
        ),
      );
    }
    return out;
  }

  async listAllSessions({ archived } = {}) {
    await this.#boot();
    const infos = await SessionManager.listAll();
    const byDirectory = new Map();
    for (const info of infos) {
      const directoryKey = normalizeDirectoryKey(info.cwd);
      const meta = this.registry.get(directoryKey, info.id);
      if (archived === false && meta?.timeArchived) continue;
      const list = byDirectory.get(directoryKey) ?? [];
      list.push(this.#wireSession(info, directoryKey, meta));
      byDirectory.set(directoryKey, list);
    }
    return byDirectory;
  }

  async createSession({ directory, title, parentID, agent, model }) {
    await this.#boot();
    const cwd = normalizeDirectoryKey(directory);
    const sessionFile = SessionManager.createEmptySessionFile(cwd);
    const manager = await SessionManager.open(sessionFile, this.#sessionDirFor(cwd));
    const sessionId = manager.getSessionId();
    const now = Date.now();
    this.registry.update(cwd, sessionId, {
      timeCreated: now,
      timeUpdated: now,
      ...(title ? { title } : {}),
      ...(parentID ? { parentID } : {}),
      // The wire `agent` param is a persona name (or the legacy build/plan
      // ids, which normalize away); store the normalized persona key.
      ...(agent && personaKeyFor(agent) !== 'standard' ? { persona: personaKeyFor(agent) } : {}),
      ...(model ? { model } : {}),
    });
    await manager.close();
    const session = this.#wireSession(
      {
        id: sessionId,
        cwd,
        title,
        created: new Date(now),
        modified: new Date(now),
      },
      cwd,
      this.registry.get(cwd, sessionId),
    );
    this.bus.emit('session.created', { sessionID: sessionId, info: session }, cwd);
    return session;
  }

  async getSession({ sessionID, directory }) {
    await this.#boot();
    const directoryKey = normalizeDirectoryKey(directory);
    const live = this.sessions.get(sessionID);
    if (live) return this.#wireSessionFromLive(live);
    const file = await this.#findSessionFile(sessionID, directoryKey);
    if (!file) return null;
    const manager = await SessionManager.open(file.path);
    try {
      const info = this.#infoFromManager(manager, file.path, directoryKey);
      return this.#wireSession(info, directoryKey, this.registry.get(directoryKey, sessionID));
    } finally {
      await manager.close();
    }
  }

  #infoFromManager(manager, filePath, directoryKey) {
    const header = manager.getHeader();
    const entries = manager.getEntries();
    const last = entries[entries.length - 1];
    const created = header ? Date.parse(header.timestamp) : Date.now();
    const modified = last ? Date.parse(last.timestamp) : created;
    return {
      id: manager.getSessionId(),
      cwd: manager.getCwd() || directoryKey,
      title: manager.getSessionName(),
      created: new Date(created),
      modified: new Date(Number.isFinite(modified) ? modified : created),
    };
  }

  async #findSessionFile(sessionID, directoryKey) {
    const dir = this.#sessionDirFor(directoryKey);
    const infos = await SessionManager.list(directoryKey, dir);
    const hit = infos.find((info) => info.id === sessionID);
    if (hit) return { path: hit.path, dir };
    return null;
  }

  async updateSession({ sessionID, directory, title, metadata, timeArchived }) {
    await this.#boot();
    const directoryKey = normalizeDirectoryKey(directory);
    const patch = { timeUpdated: Date.now() };
    if (typeof title === 'string') patch.title = title;
    if (metadata !== undefined) patch.metadata = metadata;
    if (timeArchived !== undefined) patch.timeArchived = timeArchived || undefined;
    const meta = this.registry.update(directoryKey, sessionID, patch);
    const live = this.sessions.get(sessionID);
    if (live && typeof title === 'string') {
      await live.agentSession?.setSessionName(title, 'user').catch(() => {});
    }
    const session = await this.getSession({ sessionID, directory: directoryKey });
    if (session) {
      this.bus.emit('session.updated', { sessionID, info: session }, directoryKey);
    }
    return session ?? this.#wireSession({ id: sessionID, cwd: directoryKey, created: new Date(), modified: new Date() }, directoryKey, meta);
  }

  async deleteSession({ sessionID, directory }) {
    await this.#boot();
    const directoryKey = normalizeDirectoryKey(directory);
    const file = await this.#findSessionFile(sessionID, directoryKey);
    const live = this.sessions.get(sessionID);
    const info = live ? this.#wireSessionFromLive(live) : await this.getSession({ sessionID, directory: directoryKey });
    this.#disposeSession(live ?? {});
    this.sessions.delete(sessionID);
    if (file) {
      try {
        fs.rmSync(file.path, { force: true });
      } catch {
        // Deleting a missing transcript is success.
      }
    }
    this.registry.remove(directoryKey, sessionID);
    this.bus.emit('session.deleted', { sessionID }, directoryKey);
    return info;
  }
  #wireSessionFromLive(live) {
    const meta = this.registry.get(live.directory, live.sessionId);
    const agentSession = live.agentSession;
    const now = Date.now();
    return this.#wireSession(
      {
        id: live.sessionId,
        cwd: live.directory,
        title: agentSession?.sessionManager.getSessionName() ?? meta?.title,
        created: new Date(meta?.timeCreated ?? now),
        modified: new Date(meta?.timeUpdated ?? now),
      },
      live.directory,
      meta,
      live,
    );
  }


  /** Cold message projection from the persisted transcript. */
  async getMessages({ sessionID, directory }) {
    return this.#projectedMessages(sessionID, directory);
  }

  /**
   * Paged cold projection for the message-history route: applies the
   * limit/before window over the full projection and reports the
   * next-older cursor (see paginateProjectedMessages).
   */
  async getMessagesPage({ sessionID, directory, limit, before }) {
    const projected = await this.#projectedMessages(sessionID, directory);
    if (!projected) return null;
    return paginateProjectedMessages(projected, { limit, before });
  }
  async #projectedMessages(sessionID, directory) {
    await this.#boot();
    const directoryKey = normalizeDirectoryKey(directory);
    const wireIdFor = this.#wireIdResolver(directoryKey, sessionID);
    const meta = this.registry.get(directoryKey, sessionID);
    const live = this.sessions.get(sessionID);
    const liveCount = live?.agentSession?.messages?.length ?? -1;

    // File transcript (transcript: true) is the display truth: it keeps the
    // full history including pre-compaction user turns and divider entries.
    // A live session's runtime context is post-compaction (users folded into
    // the summary), which used to blank the UI — so read the live list only
    // when it is at least as complete as the file.
    const file = await this.#findSessionFile(sessionID, directoryKey);
    if (file) {
      const manager = await SessionManager.open(file.path);
      try {
        const context = manager.buildSessionContext({ transcript: true });
        const fileMessages = context.messages ?? [];
        if (liveCount < 0 || liveCount >= fileMessages.length) {
          if (liveCount >= 0 && liveCount >= fileMessages.length) {
            return projectConversation(live.agentSession.messages, {
              sessionID,
              directory: directoryKey,
              agent: wireAgentFor(personaKeyFor(meta?.persona ?? meta?.agent)),
              wireIdFor,
            });
          }
        }
        if (fileMessages.length > 0 || liveCount < 0) {
          return projectConversation(fileMessages, {
            sessionID,
            directory: directoryKey,
            agent: wireAgentFor(personaKeyFor(meta?.persona ?? meta?.agent)),
            wireIdFor,
          });
        }
      } finally {
        await manager.close().catch(() => {});
      }
    }
    if (liveCount >= 0) {
      return projectConversation(live.agentSession.messages, {
        sessionID,
        directory: directoryKey,
        agent: wireAgentFor(personaKeyFor(meta?.persona ?? meta?.agent)),
        wireIdFor,
      });
    }
    return null;
  }

  #wireIdResolver(directoryKey, sessionID) {
    if (this.wireIdOverrides.size === 0) return undefined;
    const prefix = `${directoryKey}\u0000${sessionID}\u0000`;
    return (message) => {
      if (message?.role !== 'user' && message?.role !== 'assistant') return undefined;
      return this.wireIdOverrides.get(
        prefix + deterministicWireId(message),
      );
    };
  }

  /**
   * Keep a finished assistant turn's cold-projection id aligned with the id
   * the streaming projector already emitted. Live streaming derives the wire
   * id at message_start (empty content, start timestamp); the persisted
   * message finalizes both, so a re-fetch would otherwise project a second,
   * different id for the same message and the UI would render it twice.
   */
  #bridgeAssistantWireId(hostSession, finalMessage) {
    const liveId = hostSession.projector?.current?.id;
    if (!liveId) return;
    const seed = textOfContent(finalMessage.content) || (finalMessage.content?.[0]?.name ?? '');
    const coldId = wireMessageId('assistant', finalMessage.timestamp, seed);
    if (coldId === liveId) return;
    this.wireIdOverrides.set(`${hostSession.directory}\u0000${hostSession.sessionId}\u0000${coldId}`, liveId);
  }

  #resolveModel(selector) {
    if (!selector) return undefined;
    const available = this.modelRegistry.getAvailable();
    const wanted = `${selector.providerID}/${selector.modelID}`;
    return (
      available.find((model) => `${model.provider}/${model.id}` === wanted) ??
      available.find((model) => model.id === selector.modelID)
    );
  }

  async #materialize(sessionId, directoryKey) {
    // Concurrent materialization dedup: a UI-lease attach racing the first
    // prompt (leases.acquire is fire-and-forget) used to build two
    // AgentSessions for one id — the loser never entered `sessions`, leaked
    // its event subscription, and extension UI initialized twice. Callers
    // now share one in-flight promise per session.
    const flightKey = `${directoryKey}\u0000${sessionId}`;
    const inFlight = this.#materializeInFlight.get(flightKey);
    if (inFlight) return inFlight;
    const run = this.#materializeNow(sessionId, directoryKey).finally(() => {
      this.#materializeInFlight.delete(flightKey);
    });
    this.#materializeInFlight.set(flightKey, run);
    return run;
  }

  async #materializeNow(sessionId, directoryKey) {
    const existing = this.sessions.get(sessionId);
    if (existing?.agentSession) return existing;
    const file = await this.#findSessionFile(sessionId, directoryKey);
    if (!file) return null;
    const manager = await SessionManager.open(file.path, this.#sessionDirFor(directoryKey));
    const meta = this.registry.get(directoryKey, sessionId);
    // Model comes from the session's persisted selector when set; otherwise
    // createAgentSession resolves the settings default (defaultModel /
    // defaultProvider) exactly like the TUI. Pinning getAvailable()[0] here
    // used to override the user's configured default with whichever model
    // happened to sort first.
    const model = this.#resolveModel(
      meta?.model ? splitModelSelector(meta.model) : undefined,
    );
    // Persona overlay (02 §5.1 D-B2): 'build'/'plan'/unset → standard
    // session; a persona name → top-level systemPrompt/toolset override;
    // unknown name (deleted persona) → degrade to standard with a notice.
    const personaState = personaFor(meta, this.personas);
    if (personaState.status === 'missing') {
      console.warn(`[omp-host] session ${sessionId} references unknown persona "${personaState.name}"; using a standard session`);
    }
    const persona = personaState.status === 'active' ? personaState.persona : null;
    const agentRegistry = new AgentRegistry();
    const { session, setToolUIContext } = await createAgentSession({
      cwd: directoryKey,
      sessionManager: manager,
      authStorage: this.authStorage,
      modelRegistry: this.modelRegistry,
      // Per-directory keyed Settings injection (spec 06 §5.1, master R6):
      // the session consumes this directory's global+project layering.
      // Absent store (degraded boot) falls back to the SDK singleton.
      ...(this.settingsStore
        ? { settings: await this.settingsStore.settingsFor(directoryKey) }
        : {}),
      // One registry per session: the SDK's global registry admits a single
      // "Main" agent per process generation, and omp-host embeds several
      // concurrent top-level sessions. The instance is retained on the
      // host session for the agent-runs aggregator (spec 04 §5.5).
      agentRegistry,
      // R13: hasUI authority is the per-session UI lease, never the
      // capability. No lease at creation → fail-closed for approval tools.
      hasUI: this.dialogs.hasUISnapshotFor(directoryKey, sessionId).hasUI,
      // R7/R8: local:// resolution stays session-pinned; zero global mutation.
      localProtocolOptions: createLocalProtocolOptions(
        sessionId,
        directoryKey,
        this.#sessionDirFor(directoryKey),
      ),
      ...(model ? { model } : {}),
      // Persona overlay (02 §5.1 D-B2): constructor-time systemPrompt and
      // toolset come from the persona resource; the deleted build/plan
      // agent pair and the planYolo mapping never reach createAgentSession
      // (plan mode is a session mode driven by the mode endpoints, §5.8).
      ...(persona?.systemPrompt ? { systemPrompt: persona.systemPrompt } : {}),
      ...(Array.isArray(persona?.tools) && persona.tools.length > 0
        ? { toolNames: persona.tools }
        : {}),
    });
    const hostSession = existing ?? {
      sessionId,
      directory: directoryKey,
      agentSession: null,
      currentPersona: personaKeyFor(meta?.persona ?? meta?.agent),
      projector: null,
      lastTouched: Date.now(),
      pendingUserWireId: null,
      lastUserWireId: null,
      // Transcript roles without dedicated SDK events (custom/dividers) are
      // tail-synced at agent_end / compaction_end; this set remembers what
      // was already emitted so syncs are idempotent (spec 05 §5.5).
      syncedEntryKeys: existing?.syncedEntryKeys ?? new Set(),
      // Wire id of the most recently settled assistant message — feeds
      // omp.retry.started.supersededMessageID (spec 05 §5.3.2).
      lastAssistantWireId: null,
      // agent_end {isTerminal:false} keeps the session busy until a later
      // terminal settle (spec 05 §5.7); status snapshots must not downgrade.
      awaitingAsyncSince: null,
      // Retained for the agent-runs aggregator (spec 04 §5.5).
      agentRegistry,
      // CreateAgentSessionResult handle for setToolUIContext (spec 03 R13).
      sdkResult: { setToolUIContext },
      extensionUiInitialized: false,
      extensionUiPromise: null,
      planHandlerAttached: false,
      // Plugin application snapshot (plugins.v1): the discovery set this
      // session bound at materialization — feeds the Settings → Plugins
      // "applied in sessions" projection and stays frozen for the session's
      // lifetime (TS extension modules are not rebound by reload).
      appliedPlugins: null,
    };
    hostSession.appliedPlugins = await this.#snapshotAppliedPlugins(directoryKey);
    hostSession.agentSession = session;
    hostSession.lastTouched = Date.now();
    hostSession.unsubscribe = session.subscribe((event) => {
      try {
        this.#handleEngineEvent(hostSession, event);
      } catch (error) {
        console.error('[omp-host] event projection error:', error);
      }
    });
    // Modes tracker for this session (cold-recovery + mode_change appends).
    this.modesDomain?.trackerFor(sessionId, directoryKey);
    // The freshly materialized host session is not published in `sessions`
    // until all setup below succeeds. Apply and await the lease context
    // directly; routing through #attachDialogUi would miss it in that short
    // window and leave extension approvals/headless commands unusable.
    if (this.dialogs.hasUISnapshotFor(directoryKey, sessionId).hasUI) {
      await this.#setDialogUiContext(hostSession, directoryKey, sessionId, true);
    }
    session.sessionManager?.onSessionNameChanged?.(() => {
      const info = this.#wireSessionFromLive(hostSession);
      this.registry.update(directoryKey, sessionId, {
        title: info.title,
        timeUpdated: Date.now(),
      });
      this.bus.emit('session.updated', { sessionID: sessionId, info }, directoryKey);
    });
    this.sessions.set(sessionId, hostSession);
    return hostSession;
  }


  /**
   * Discovery-set snapshot for a freshly materialized session (plugins.v1).
   * Runs right after createAgentSession so the SDK's discovery caches are warm
   * and return exactly what the session just bound. Failures degrade to null
   * — never block session setup on the settings projection.
   */
  async #snapshotAppliedPlugins(directoryKey) {
    try {
      const { discoverExtensionPaths } = await import('@oh-my-pi/pi-coding-agent/extensibility/extensions');
      const { getEnabledPlugins } = await import('@oh-my-pi/pi-coding-agent/extensibility/plugins');
      const directory = directoryKey ?? process.cwd();
      const [extensionPaths, plugins] = await Promise.all([
        discoverExtensionPaths([], directory),
        getEnabledPlugins(directory),
      ]);
      return {
        appliedAt: Date.now(),
        extensionPaths: extensionPaths.map((item) => path.resolve(item)),
        pluginNames: plugins.map((plugin) => plugin.name),
      };
    } catch (error) {
      console.warn('[omp-host] applied-plugins snapshot failed:', error?.message ?? error);
      return null;
    }
  }

  /** Live per-session plugin application snapshots (plugins.v1 projection). */
  appliedPluginsSnapshots() {
    return [...this.sessions.values()]
      .filter((hostSession) => hostSession.agentSession && hostSession.appliedPlugins)
      .map((hostSession) => ({
        sessionId: hostSession.sessionId,
        directory: hostSession.directory,
        ...hostSession.appliedPlugins,
      }));
  }

  /**
   * Hot-reload plugin state for live sessions in a directory (plugins.v1):
   * mirrors omp's `/reload-plugins` — invalidate the process-global discovery
   * caches, republish task/agent definitions, and refresh skills + slash
   * commands on every live session of that directory. TS extension module
   * bindings stay frozen (sessions rebind at next materialization).
   */
  async reloadAppliedPlugins(directory, sessionId = null) {
    const directoryKey = normalizeDirectoryKey(directory ?? process.cwd());
    let projectRegistryPath = null;
    try {
      const { resolveActiveProjectRegistryPath } = await import('@oh-my-pi/pi-coding-agent/discovery/helpers');
      projectRegistryPath = await resolveActiveProjectRegistryPath(directoryKey);
    } catch {
      projectRegistryPath = null;
    }
    try {
      const { clearPluginRootsAndCaches } = await import('@oh-my-pi/pi-coding-agent/discovery/helpers');
      clearPluginRootsAndCaches(projectRegistryPath ? [projectRegistryPath] : undefined);
    } catch (error) {
      console.warn('[omp-host] reload cache invalidation failed:', error?.message ?? error);
    }
    try {
      const { refreshAgentDiscovery } = await import('@oh-my-pi/pi-coding-agent/task');
      await refreshAgentDiscovery(directoryKey);
    } catch (error) {
      console.warn('[omp-host] reload agent discovery refresh failed:', error?.message ?? error);
    }
    let sessionsRefreshed = 0;
    for (const hostSession of this.sessions.values()) {
      if (hostSession.directory !== directoryKey || !hostSession.agentSession) continue;
      if (sessionId && hostSession.sessionId !== sessionId) continue;
      try {
        await hostSession.agentSession.refreshSkills?.();
        sessionsRefreshed += 1;
      } catch (error) {
        console.warn('[omp-host] reload skills refresh failed:', hostSession.sessionId, error?.message ?? error);
      }
    }
    return { sessionsRefreshed };
  }
  /**
   * omp-native publish helper (spec 05 §5.2.1 envelope; master D6-R1 single
   * channel). Payload never carries directory/sessionID.
   */
  #ompPublish(hostSession, type, payload, { durable } = {}) {
    return this.ompBus.publish(type, payload, {
      directory: hostSession.directory,
      sessionID: hostSession.sessionId,
      durable: Boolean(durable),
    });
  }

  /**
   * Project + emit one live custom/hook message on both tracks (spec 05
   * §5.1 row 9). Returns the projected wire message id. `display:false`
   * messages emit only the omp event (UI won't build a card; cold projection
   * drops them too — double guard, 05 §5.8.2 T3).
   */
  #emitCustomLive(hostSession, message) {
    const { sessionId, directory } = hostSession;
    const projected = projectCustomMessage(message, {
      sessionID: sessionId,
      agent: wireAgentFor(hostSession.currentPersona),
      parentID: hostSession.lastUserWireId || undefined,
    });
    const text = textOfContent(message.content);
    if (message.display !== false) {
      this.bus.emit('message.updated', { sessionID: sessionId, info: projected.info }, directory);
      for (const part of projected.parts) {
        this.bus.emit('message.part.updated', { sessionID: sessionId, part, time: Date.now() }, directory);
      }
    }
    this.#ompPublish(hostSession, 'omp.custom.appended', {
      message: {
        wireMessageID: projected.info.id,
        customType: message.customType ?? '',
        attribution: message.attribution,
        timestamp: message.timestamp,
        text,
        ...(message.details !== undefined ? { details: message.details } : {}),
        display: message.display !== false,
      },
    }, { durable: true });
    hostSession.syncedEntryKeys?.add(`${message.role}:${message.customType ?? ''}:${message.timestamp}`);
    return projected.info.id;
  }

  /**
   * Tail-sync: project transcript roles that have no dedicated SDK event
   * (custom injected out-of-band, compaction/branch dividers) so they appear
   * live without a refetch (spec 05 §5.5). Idempotent per (role,type,ts).
   * @returns {{ projected: Array<{wireId: string, role: string}>, lastCompactionId: string | null }}
   */
  #tailSyncTranscript(hostSession) {
    const session = hostSession.agentSession;
    const out = { projected: [], lastCompactionId: null };
    if (!session?.messages) return out;
    const messages = session.messages;
    const pending = [];
    for (let i = messages.length - 1; i >= 0; i -= 1) {
      const message = messages[i];
      if (!message || typeof message !== 'object') continue;
      const role = message.role;
      if (role !== 'custom' && role !== 'hookMessage' && role !== 'compactionSummary' && role !== 'branchSummary') continue;
      const key = `${role}:${message.customType ?? ''}:${message.timestamp}`;
      if (hostSession.syncedEntryKeys?.has(key)) break;
      pending.push(message);
    }
    pending.reverse();
    for (const message of pending) {
      const key = `${message.role}:${message.customType ?? ''}:${message.timestamp}`;
      hostSession.syncedEntryKeys?.add(key);
      if (message.role === 'compactionSummary' || message.role === 'branchSummary') {
        const projected = projectDividerMessage(message, {
          sessionID: hostSession.sessionId,
          agent: wireAgentFor(hostSession.currentPersona),
          parentID: hostSession.lastUserWireId || undefined,
        });
        this.bus.emit('message.updated', { sessionID: hostSession.sessionId, info: projected.info }, hostSession.directory);
        for (const part of projected.parts) {
          this.bus.emit('message.part.updated', { sessionID: hostSession.sessionId, part, time: Date.now() }, hostSession.directory);
        }
        out.projected.push({ wireId: projected.info.id, role: message.role });
        if (message.role === 'compactionSummary') out.lastCompactionId = projected.info.id;
      } else {
        if (message.display === false || !textOfContent(message.content).trim()) continue;
        const wireId = this.#emitCustomLive(hostSession, message);
        out.projected.push({ wireId, role: message.role });
      }
    }
    return out;
  }

  /**
   * Full disposition of the SDK AgentSessionEvent union (spec 05 §5.1/§5.1.1,
   * master D2/D6): every one of the 24 members has an explicit case — wire
   * track, omp track, dual, or a justified intentional-ignore. The trailing
   * default is defense-in-depth only; scripts/check-event-coverage.mjs is
   * the real CI guard against unregistered SDK additions.
   */
  #handleEngineEvent(hostSession, event) {
    const { sessionId, directory } = hostSession;
    const session = hostSession.agentSession;
    if (!session) return;
    switch (event.type) {
      case 'message_start': {
        if (event.message?.role === 'user') {
          const pending = hostSession.pendingUserWireId;
          hostSession.pendingUserWireId = null;
          if (pending) {
            const canonicalId = wireMessageId(
              'user',
              event.message.timestamp,
              textOfContent(event.message.content),
            );
            this.wireIdOverrides.set(`${directory}\u0000${sessionId}\u0000${canonicalId}`, pending);
          }
          return;
        }
        if (event.message?.role !== 'assistant') return;
        hostSession.projector = new StreamProjector({
          sessionID: sessionId,
          directory,
          agent: wireAgentFor(hostSession.currentPersona),
          emit: (type, properties, dir) => this.bus.emit(type, properties, dir),
        });
        hostSession.projector.setParentID(hostSession.lastUserWireId ?? '');
        hostSession.projector.startAssistant(event.message);
        return;
      }
      case 'message_update': {
        const projector = hostSession.projector;
        if (!projector || !projector.current) return;
        const inner = event.assistantMessageEvent;
        if (!inner) return;
        if (inner.type === 'text_delta' && typeof inner.delta === 'string') {
          projector.textDelta(inner.delta);
        } else if (inner.type === 'thinking_delta' && typeof inner.delta === 'string') {
          projector.thinkingDelta(inner.delta);
        } else if (inner.type === 'toolcall_end' && inner.toolCall) {
          projector.toolStarted(inner.toolCall.id, inner.toolCall.name, inner.toolCall.arguments);
        }
        return;
      }
      case 'message_end': {
        if (event.message?.role !== 'assistant' || !hostSession.projector) return;
        const finished = hostSession.projector.finishAssistant(
          event.message,
          hostSession.turnToolResults ?? new Map(),
        );
        this.#bridgeAssistantWireId(hostSession, event.message);
        if (finished?.id) {
          hostSession.lastAssistantWireId = finished.id;
          const usage = event.message.usage ?? {};
          this.#ompPublish(hostSession, 'omp.usage.turn', {
            messageID: finished.id,
            usage,
            ...(event.message.ttft !== undefined ? { ttftMs: event.message.ttft } : {}),
            ...(event.message.duration !== undefined ? { durationMs: event.message.duration } : {}),
            timestamp: event.message.timestamp ?? Date.now(),
          }, { durable: true });
        }
        return;
      }
      case 'tool_execution_start': {
        hostSession.projector?.toolStarted(event.toolCallId, event.toolName, event.args, {
          ...(event.intent ? { title: event.intent } : {}),
        });
        return;
      }
      case 'tool_execution_update': {
        // Partial results (05 §5.6): running-state append; never terminal —
        // tool_execution_end owns completion.
        hostSession.projector?.toolPartial(event.toolCallId, {
          text: typeof event.partialResult === 'string'
            ? event.partialResult
            : event.partialResult?.text ?? event.partialResult?.output,
          asyncState: event.partialResult?.details?.async?.state,
        });
        return;
      }
      case 'tool_execution_end': {
        // The SDK result is an AgentToolResult {content, details}; normalize
        // once so the transient part and the final finishAssistant projection
        // carry the same text output and structured details (spec 03 §5.4.1).
        const { content, text, details } = normalizeToolExecutionResult(event.result);
        hostSession.projector?.toolFinished(event.toolCallId, {
          output: text,
          error: event.isError ? (text || 'Tool error') : undefined,
          ...(details ? { metadata: { details } } : {}),
        });
        const results = hostSession.turnToolResults ?? new Map();
        results.set(event.toolCallId, {
          content,
          ...(details ? { details } : {}),
          isError: Boolean(event.isError),
          timestamp: Date.now(),
        });
        hostSession.turnToolResults = results;
        return;
      }
      case 'turn_start':
        // Intentional ignore: message_*/tool_execution_* carry the surface;
        // turn boundaries are a TUI-internal concept (05 §5.1.1).
        return;
      case 'turn_end':
        // Intentional ignore: same reasoning as turn_start.
        return;
      case 'agent_start': {
        hostSession.turnToolResults = new Map();
        hostSession.awaitingAsyncSince = null;
        this.bus.emit('session.status', { sessionID: sessionId, status: { type: 'busy' } }, directory);
        return;
      }
      case 'agent_end': {
        const projector = hostSession.projector;
        if (projector?.current) {
          const finished = projector.finishAssistant(
            session.getLastAssistantMessage() ?? { content: [], timestamp: Date.now(), usage: {}, model: '' },
            hostSession.turnToolResults ?? new Map(),
          );
          if (finished?.id) hostSession.lastAssistantWireId = finished.id;
        }
        hostSession.projector = null;
        hostSession.turnToolResults = null;
        this.registry.update(directory, sessionId, { timeUpdated: Date.now() });
        const info = this.#wireSessionFromLive(hostSession);
        this.bus.emit('session.updated', { sessionID: sessionId, info }, directory);
        this.#tailSyncTranscript(hostSession);
        if (event.isTerminal === false) {
          // Async delivery will resume the session (05 §5.7): scheduling
          // pause, not completion — keep busy so the queue gate stays
          // closed and notifications stay suppressed.
          hostSession.awaitingAsyncSince = Date.now();
          this.bus.emit('session.status', { sessionID: sessionId, status: { type: 'busy' } }, directory);
          this.#ompPublish(hostSession, 'omp.session.settled', { isTerminal: false }, { durable: false });
          return;
        }
        hostSession.awaitingAsyncSince = null;
        this.bus.emit('session.idle', { sessionID: sessionId }, directory);
        return;
      }
      case 'todo_reminder': {
        const todos = (event.todos ?? []).map((todo) => ({
          content: todo.content ?? '',
          status: todo.status ?? 'pending',
          priority: todo.priority ?? 'medium',
        }));
        this.bus.emit('todo.updated', { sessionID: sessionId, todos }, directory);
        return;
      }
      case 'todo_auto_clear': {
        this.bus.emit('todo.updated', { sessionID: sessionId, todos: [] }, directory);
        return;
      }
      case 'notice': {
        if (event.level === 'error') console.error('[omp-host]', event.message);
        this.#ompPublish(hostSession, 'omp.notice.raised', {
          level: event.level,
          message: event.message,
          ...(event.source ? { source: event.source } : {}),
        }, { durable: false });
        return;
      }
      case 'auto_compaction_start': {
        this.#ompPublish(hostSession, 'omp.compaction.started', {
          reason: event.reason,
          action: event.action,
        }, { durable: false });
        return;
      }
      case 'auto_compaction_end': {
        const sync = this.#tailSyncTranscript(hostSession);
        this.#ompPublish(hostSession, 'omp.compaction.ended', {
          action: event.action,
          aborted: Boolean(event.aborted),
          willRetry: Boolean(event.willRetry),
          ...(event.skipped !== undefined ? { skipped: event.skipped } : {}),
          ...(event.errorMessage ? { errorMessage: event.errorMessage } : {}),
          ...(event.result?.tokensBefore !== undefined ? { tokensBefore: event.result.tokensBefore } : {}),
          ...(sync.lastCompactionId ? { wireMessageID: sync.lastCompactionId } : {}),
        }, { durable: false });
        return;
      }
      case 'auto_retry_start': {
        // P1 (05 §5.3.2): status + superseded overlay only. Zero wire
        // mutation — message.part.removed stays P2-gated (master R14).
        this.bus.emit('session.status', {
          sessionID: sessionId,
          status: {
            type: 'retry',
            attempt: event.attempt,
            message: event.errorMessage,
            next: Date.now() + event.delayMs,
          },
        }, directory);
        this.#ompPublish(hostSession, 'omp.retry.started', {
          attempt: event.attempt,
          maxAttempts: event.maxAttempts,
          delayMs: event.delayMs,
          errorMessage: event.errorMessage,
          ...(hostSession.lastAssistantWireId ? { supersededMessageID: hostSession.lastAssistantWireId } : {}),
        }, { durable: false });
        return;
      }
      case 'auto_retry_end': {
        this.bus.emit('session.status', { sessionID: sessionId, status: { type: 'busy' } }, directory);
        this.#ompPublish(hostSession, 'omp.retry.ended', {
          success: Boolean(event.success),
          attempt: event.attempt,
          ...(event.finalError ? { finalError: event.finalError } : {}),
          retryErrors: (event.retryErrors ?? []).map((update) => ({
            messageID: update.persistenceKey ?? update.entryId,
            note: update.note,
            retryRecovery: update.retryRecovery,
          })),
        }, { durable: true });
        return;
      }
      case 'retry_fallback_applied': {
        // Registry truth sync only; the SDK guarantees a follow-up
        // model_changed which emits the wire session.updated (05 §5.4).
        this.registry.update(directory, sessionId, { model: event.to });
        this.#ompPublish(hostSession, 'omp.fallback.applied', {
          from: event.from,
          to: event.to,
          role: event.role,
        }, { durable: true });
        return;
      }
      case 'retry_fallback_succeeded': {
        // Success happened on the fallback model; no registry writeback.
        this.#ompPublish(hostSession, 'omp.fallback.succeeded', {
          model: event.model,
          role: event.role,
        }, { durable: true });
        return;
      }
      case 'model_changed': {
        const selector = modelSelector(session.model);
        this.registry.update(directory, sessionId, { ...(selector ? { model: selector } : {}) });
        const info = this.#wireSessionFromLive(hostSession);
        this.bus.emit('session.updated', { sessionID: sessionId, info }, directory);
        this.#ompPublish(hostSession, 'omp.model.changed', {
          model: session.model
            ? { provider: session.model.provider, id: session.model.id }
            : null,
          ...(session.thinkingLevel !== undefined ? { thinkingLevel: session.thinkingLevel } : {}),
        }, { durable: true });
        return;
      }
      case 'ttsr_triggered': {
        this.#ompPublish(hostSession, 'omp.ttsr.triggered', {
          rules: (event.rules ?? []).map((rule) => ({ name: rule.name })),
        }, { durable: false });
        return;
      }
      case 'irc_message': {
        this.#emitCustomLive(hostSession, event.message);
        return;
      }
      case 'thinking_level_changed': {
        this.#ompPublish(hostSession, 'omp.thinking.changed', {
          thinkingLevel: event.thinkingLevel ?? null,
          ...(event.configured !== undefined ? { configured: event.configured } : {}),
          ...(event.resolved !== undefined ? { resolved: event.resolved } : {}),
        }, { durable: true });
        return;
      }
      case 'goal_updated': {
        this.modesDomain?.trackerFor(sessionId, directory)?.applyGoalUpdate?.(event.goal, event.state);
        this.#ompPublish(hostSession, 'omp.goal.updated', {
          goal: event.goal ?? null,
          ...(event.state !== undefined ? { state: event.state } : {}),
        }, { durable: true });
        return;
      }
      default: {
        // Defense-in-depth only (05 §5.1): the manifest + CI guard own the
        // real coverage check. Never silently swallow an unknown member.
        console.error(`[omp-host] unhandled AgentSessionEvent type: ${event?.type}`);
        this.unknownEventCounts = this.unknownEventCounts ?? new Map();
        this.unknownEventCounts.set(event?.type, (this.unknownEventCounts.get(event?.type) ?? 0) + 1);
        return;
      }
    }
  }

  async getSessionStatuses({ directory }) {
    await this.#boot();
    const AWAITING_ASYNC_TIMEOUT_MS = 10 * 60 * 1000;
    const now = Date.now();
    const statuses = {};
    for (const [id, live] of this.sessions) {
      if (live.directory !== normalizeDirectoryKey(directory)) continue;
      const stale =
        live.awaitingAsyncSince !== null &&
        now - live.awaitingAsyncSince > AWAITING_ASYNC_TIMEOUT_MS;
      if (stale) live.awaitingAsyncSince = null;
      statuses[id] =
        live.agentSession?.isStreaming || live.awaitingAsyncSince !== null
          ? { type: 'busy' }
          : { type: 'idle' };
    }
    return statuses;
  }

  /** Structured customType inventory for the omp transcript read (05 §5.2.1). */
  async getCustomMessages({ sessionID, directory }) {
    const context = await this.#transcriptContext(sessionID, directory);
    if (!context) return null;
    const out = [];
    for (const message of context.messages ?? []) {
      if (!message || typeof message !== 'object') continue;
      if (message.role !== 'custom' && message.role !== 'hookMessage') continue;
      if (message.display === false) continue;
      const projected = projectCustomMessage(message, { sessionID });
      out.push({
        wireMessageID: projected.info.id,
        customType: message.customType ?? '',
        timestamp: message.timestamp,
        attribution: message.attribution,
        text: textOfContent(message.content),
        ...(message.details !== undefined ? { details: message.details } : {}),
      });
    }
    return out;
  }

  /** Per-turn telemetry (05 §5.9): usage/ttft/duration per assistant message. */
  async getTelemetry({ sessionID, directory }) {
    const context = await this.#transcriptContext(sessionID, directory);
    if (!context) return null;
    const directoryKey = normalizeDirectoryKey(directory);
    const wireIdFor = this.#wireIdResolver(directoryKey, sessionID);
    const out = [];
    for (const message of context.messages ?? []) {
      if (!message || message.role !== 'assistant') continue;
      const seed = textOfContent(message.content) || (message.content?.[0]?.name ?? '');
      const baseId = wireMessageId('assistant', message.timestamp, seed);
      const overridden = wireIdFor?.(message);
      const usage = message.usage ?? {};
      out.push({
        messageID: overridden ?? baseId,
        timestamp: message.timestamp,
        input: usage.input ?? 0,
        output: usage.output ?? 0,
        cacheRead: usage.cacheRead ?? 0,
        cacheWrite: usage.cacheWrite ?? 0,
        ...(usage.reasoning !== undefined ? { reasoningTokens: usage.reasoning } : {}),
        totalTokens:
          (usage.input ?? 0) + (usage.output ?? 0) + (usage.cacheRead ?? 0) + (usage.cacheWrite ?? 0),
        ...(message.ttft !== undefined ? { ttftMs: message.ttft } : {}),
        ...(message.duration !== undefined ? { durationMs: message.duration } : {}),
      });
    }
    return out;
  }

  /**
   * Structured session entries (05 §5.2.1): compaction dividers, branch
   * summaries, model/mode changes, ttsr injections, retry recovery notes.
   */
  async getEntries({ sessionID, directory, kinds }) {
    await this.#boot();
    const directoryKey = normalizeDirectoryKey(directory);
    const file = await this.#findSessionFile(sessionID, directoryKey);
    if (!file) return null;
    const wanted = new Set(
      String(kinds ?? '')
        .split(',')
        .map((kind) => kind.trim())
        .filter(Boolean),
    );
    const out = [];
    const manager = await SessionManager.open(file.path);
    try {
      for (const entry of manager.getEntries() ?? []) {
        const kind =
          entry.type === 'compaction' ? 'compaction'
          : entry.type === 'branch_summary' ? 'branch_summary'
          : entry.type === 'model_change' ? 'model_change'
          : entry.type === 'mode_change' ? 'mode_change'
          : entry.type === 'ttsr_injection' ? 'ttsr_injection'
          : null;
        if (!kind || (wanted.size > 0 && !wanted.has(kind))) continue;
        out.push({
          kind,
          id: entry.id,
          timestamp: Date.parse(entry.timestamp ?? '') || undefined,
          ...(kind === 'compaction'
            ? {
                summary: entry.summary,
                tokensBefore: entry.tokensBefore,
                ...(entry.warning ? { warning: entry.warning } : {}),
              }
            : {}),
          ...(kind === 'branch_summary' ? { fromId: entry.fromId, summary: entry.summary } : {}),
          ...(kind === 'model_change' ? { model: entry.model, ...(entry.role ? { role: entry.role } : {}) } : {}),
          ...(kind === 'mode_change' ? { mode: entry.mode, ...(entry.data ? { data: entry.data } : {}) } : {}),
          ...(kind === 'ttsr_injection' ? { rules: entry.rules } : {}),
        });
      }
    } finally {
      await manager.close();
    }
    if (wanted.size === 0 || wanted.has('retry_recovery')) {
      const context = await this.#transcriptContext(sessionID, directory);
      const directoryKeyNow = normalizeDirectoryKey(directory);
      const wireIdFor = this.#wireIdResolver(directoryKeyNow, sessionID);
      for (const message of context?.messages ?? []) {
        if (!message || message.role !== 'assistant' || !message.retryRecovery) continue;
        const seed = textOfContent(message.content) || (message.content?.[0]?.name ?? '');
        const baseId = wireMessageId('assistant', message.timestamp, seed);
        out.push({
          kind: 'retry_recovery',
          messageID: wireIdFor?.(message) ?? baseId,
          timestamp: message.timestamp,
          retryRecovery: message.retryRecovery,
        });
      }
    }
    return out;
  }

  async #transcriptContext(sessionID, directory) {
    await this.#boot();
    const directoryKey = normalizeDirectoryKey(directory);
    const file = await this.#findSessionFile(sessionID, directoryKey);
    if (!file) return null;
    const manager = await SessionManager.open(file.path);
    try {
      return manager.buildSessionContext({ transcript: true });
    } finally {
      await manager.close();
    }
  }

  async prompt({ sessionID, directory, text, model, agent, images, delivery, messageID }) {
    await this.#boot();
    const directoryKey = normalizeDirectoryKey(directory);
    const hostSession = (await this.#materialize(sessionID, directoryKey));
    if (!hostSession) return null;
    const session = hostSession.agentSession;
    if (hostSession.extensionUiPromise) await hostSession.extensionUiPromise;
    hostSession.lastTouched = Date.now();

    // Model switching: resolve and apply when the requested selector differs.
    if (model && (model.providerID || model.modelID)) {
      const target = this.#resolveModel(model);
      if (target && modelSelector(target) !== modelSelector(session.model)) {
        await session.setModel(target).catch((error) => {
          console.error('[omp-host] model switch failed:', error?.message ?? error);
        });
        this.registry.update(directoryKey, sessionID, { model: modelSelector(target) });
      }
    }

    const meta = this.registry.get(directoryKey, sessionID);
    // Persona switch (02 §5.1 D-B3, R2-M3): explicit session-level switch —
    // the wire `agent` parameter and registry meta normalize through
    // personaKeyFor, so the deleted build/plan values and unset all mean
    // "standard" and never trigger a rebuild. A switch to a persona that no
    // longer exists is rejected before any state changes: the session keeps
    // its current persona and the message is not dispatched.
    const nextPersona = personaKeyFor(agent ?? meta?.persona ?? meta?.agent);
    if (nextPersona !== 'standard' && !this.personas.has(nextPersona)) {
      throw new ModeDomainError(404, { error: 'persona-not-found', name: nextPersona });
    }
    if (nextPersona !== hostSession.currentPersona) {
      // The persona shapes the session's system prompt and toolset at
      // construction, so rebuild the AgentSession over the same transcript.
      this.#disposeSession(hostSession);
      this.registry.update(directoryKey, sessionID, {
        persona: nextPersona === 'standard' ? undefined : nextPersona,
        agent: undefined,
      });
      const rebuilt = await this.#materialize(sessionID, directoryKey);
      if (!rebuilt) return null;
      return this.prompt({ sessionID, directory: directoryKey, text, model, agent: nextPersona, images, delivery, messageID });
    }

    const content = [];
    if (typeof text === 'string' && text.length > 0) content.push({ type: 'text', text });
    for (const image of images ?? []) {
      content.push({
        type: 'image',
        data: image.data,
        mimeType: image.mimeType || 'image/png',
      });
    }
    const wire = projectUserMessage(
      {
        role: 'user',
        content: content.length === 1 && content[0].type === 'text' ? content[0].text : content,
        timestamp: Date.now(),
      },
      {
        sessionID,
        agent: wireAgentFor(nextPersona),
        model: session.model,
        ...(typeof messageID === 'string' && messageID ? { wireId: messageID } : {}),
      },
    );
    hostSession.pendingUserWireId = typeof messageID === 'string' && messageID ? messageID : null;
    hostSession.lastUserWireId = wire.info.id;
    this.bus.emit('message.updated', { sessionID, info: wire.info }, directoryKey);
    for (const part of wire.parts) {
      this.bus.emit(
        'message.part.updated',
        { sessionID, part, time: Date.now() },
        directoryKey,
      );
    }

    if (!meta?.timeCreated) {
      this.registry.update(directoryKey, sessionID, { timeCreated: wire.info.time.created });
    }
    // Title generation mirrors the TUI: attempted at submission time on every
    // user message, while the turn runs. pi skips internally once the session
    // is named and retries later messages when an attempt failed or the input
    // was too low-signal to title. Slash commands never title in the TUI
    // (commands are host-level there); guard them here too — an unguarded
    // "/compact" once titled the session with the entire compaction summary.
    if (!text.trimStart().startsWith('/')) {
      session.maybeStartTitleGeneration(text);
    }

    const textOnly = content.length === 1 && content[0].type === 'text' ? content[0].text : text ?? '';
    const imageContents = content.filter((block) => block.type === 'image');
    // Dispatch mirrors the TUI input loop: every submission carries a
    // streaming behavior so a live turn never rejects the prompt. steer
    // injects into the running turn (the TUI's Enter-while-streaming);
    // delivery "queue" waits for the next turn. The behavior is a no-op
    // when idle, and routing through prompt() rather than steer() keeps
    // "/" extension commands working mid-turn (steer() rejects them).
    const streamingBehavior = delivery === 'queue' ? 'followUp' : 'steer';
    await session.prompt(textOnly, { images: imageContents, streamingBehavior });
    return wire;
  }

  /**
   * Session-scoped model switch without sending a turn (spec 01 GAP-02/
   * GAP-04: prompts omit the model; changing it is an explicit setModel).
   * Same resolution + registry bookkeeping as the prompt-time switch.
   * GAP-06: when the target model matches the session's current model, this
   * degrades to a thinking-level-only change (`setThinkingLevel`) — the
   * in-session thinking slot applies through the same endpoint.
   */
  async setSessionModel({ sessionID, directory, model, thinkingLevel }) {
    await this.#boot();
    const directoryKey = normalizeDirectoryKey(directory);
    if (!model || !(model.providerID || model.modelID)) {
      return { ok: false, error: 'model is required' };
    }
    const hostSession = await this.#materialize(sessionID, directoryKey);
    if (!hostSession) return { ok: false, error: 'session not found' };
    const session = hostSession.agentSession;
    hostSession.lastTouched = Date.now();
    const target = this.#resolveModel(model);
    if (!target) return { ok: false, error: 'unknown model' };
    if (modelSelector(target) !== modelSelector(session.model)) {
      await session.setModel(target).catch((error) => {
        console.error('[omp-host] model switch failed:', error?.message ?? error);
      });
      this.registry.update(directoryKey, sessionID, { model: modelSelector(target) });
    }
    if (thinkingLevel !== undefined && typeof session.setThinkingLevel === 'function') {
      await session.setThinkingLevel(thinkingLevel).catch((error) => {
        console.error('[omp-host] thinking level switch failed:', error?.message ?? error);
      });
    }
    return { ok: true, model: modelSelector(session.model) ?? modelSelector(target) };
  }

  async abort({ sessionID, directory }) {
    await this.#boot();
    const live = this.sessions.get(sessionID);
    if (!live?.agentSession) return false;
    await live.agentSession.abort({ reason: 'User aborted' });
    return true;
  }

  async summarize({ sessionID, directory }) {
    await this.#boot();
    const directoryKey = normalizeDirectoryKey(directory);
    const hostSession = await this.#materialize(sessionID, directoryKey);
    if (!hostSession) return false;
    await hostSession.agentSession.compact();
    return true;
  }

  async fork({ sessionID, directory }) {
    await this.#boot();
    const directoryKey = normalizeDirectoryKey(directory);
    const file = await this.#findSessionFile(sessionID, directoryKey);
    if (!file) return null;
    const forked = await SessionManager.forkFrom(file.path, directoryKey, this.#sessionDirFor(directoryKey));
    const forkId = forked.getSessionId();
    const now = Date.now();
    const meta = this.registry.get(directoryKey, sessionID);
    this.registry.update(directoryKey, forkId, {
      parentID: sessionID,
      title: meta?.title ? `${meta.title} (fork)` : 'Forked session',
      timeCreated: now,
      timeUpdated: now,
      ...(meta?.persona ? { persona: meta.persona } : {}),
      ...(meta?.agent ? { agent: meta.agent } : {}),
      ...(meta?.model ? { model: meta.model } : {}),
    });
    await forked.close();
    const session = this.#wireSession(
      { id: forkId, cwd: directoryKey, created: new Date(now), modified: new Date(now) },
      directoryKey,
      this.registry.get(directoryKey, forkId),
    );
    this.bus.emit('session.created', { sessionID: forkId, info: session }, directoryKey);
    return session;
  }

  /**
   * Revert: move the transcript's active branch so `messageID` becomes the
   * last retained message. Records the previous leaf for unrevert.
   */
  async revert({ sessionID, directory, messageID }) {
    await this.#boot();
    const directoryKey = normalizeDirectoryKey(directory);
    const hostSession = await this.#materialize(sessionID, directoryKey);
    if (!hostSession) return null;
    const manager = hostSession.agentSession.sessionManager;
    // The UI sends the wire message id it read from GET messages; branch()
    // wants the engine entry id. Resolve through the same projection the UI
    // saw (native ids pass through unchanged for compat).
    const entryId = resolveWireIdToEntryId(manager.getEntries?.() ?? [], messageID, {
      wireIdFor: this.#wireIdResolver(directoryKey, sessionID),
    });
    manager.branch(entryId ?? messageID);
    const previousLeaf = manager.getLeafId();
    this.registry.update(directoryKey, sessionID, {
      revert: { messageID, previousLeaf },
      timeUpdated: Date.now(),
    });
    const session = this.#wireSessionFromLive(hostSession);
    session.revert = { messageID };
    this.bus.emit('session.updated', { sessionID, info: session }, directoryKey);
    return session;
  }

  /**
   * Live-session extension commands for one directory (09 §5.4 discovery
   * gap): the headless AvailableCommandsSession has no extension runner, so
   * `pi.registerCommand` commands (e.g. user extensions in
   * ~/.omp/agent/extensions) only exist on materialized sessions. The
   * extension factory runs at session creation, so any live session for the
   * directory is a valid source.
   */
  liveCommandsFor(directory) {
    const directoryKey = normalizeDirectoryKey(directory);
    for (const hostSession of this.sessions.values()) {
      if (hostSession.directory !== directoryKey) continue;
      const session = hostSession.agentSession;
      if (!session?.extensionRunner) continue;
      try {
        const commands = getSessionSlashCommands(session) ?? [];
        return Promise.resolve(commands.map((command) => ({
          name: command.name,
          ...(typeof command.description === 'string' && command.description
            ? { description: command.description }
            : {}),
          source: command.source ?? 'extension',
        })));
      } catch {
        return Promise.resolve([]);
      }
    }
    return Promise.resolve([]);
  }

  async unrevert({ sessionID, directory }) {
    await this.#boot();
    const directoryKey = normalizeDirectoryKey(directory);
    const hostSession = await this.#materialize(sessionID, directoryKey);
    if (!hostSession) return null;
    const meta = this.registry.get(directoryKey, sessionID);
    const previousLeaf = meta?.revert?.previousLeaf;
    const manager = hostSession.agentSession.sessionManager;
    if (previousLeaf) {
      manager.branch(previousLeaf);
    } else {
      manager.resetLeaf();
    }
    this.registry.update(directoryKey, sessionID, { revert: undefined, timeUpdated: Date.now() });
    const session = this.#wireSessionFromLive(hostSession);
    this.bus.emit('session.updated', { sessionID, info: session }, directoryKey);
    return session;
  }

  async getTodos({ sessionID, directory }) {
    await this.#boot();
    const directoryKey = normalizeDirectoryKey(directory);
    const hostSession = this.sessions.get(sessionID);
    if (!hostSession?.agentSession) return [];
    const phases = hostSession.agentSession.getTodoPhases();
    const latest = phases[phases.length - 1];
    return (latest?.items ?? latest?.todos ?? []).map((todo) => ({
      content: todo.content ?? '',
      status: todo.status ?? 'pending',
      priority: todo.priority ?? 'medium',
    }));
  }


  async shutdown() {
    clearInterval(this.sweeper);
    try {
      await this.dialogs?.dispose?.('omp-host shutdown');
    } catch {
      // Settle-all is best-effort at shutdown.
    }
    for (const session of this.sessions.values()) {
      this.#disposeSession(session);
    }
    this.sessions.clear();
    this.uriDomain?.dispose?.();
    try {
      await this.settingsStore?.disposeAll?.();
    } catch {
      // Flush is best-effort at shutdown.
    }
  }

  availableModels() {
    return this.modelRegistry?.getAvailable() ?? [];
  }

  /**
   * Reload models from disk (builtin + custom models.yml). Static inputs are
   * mtime-checked inside ModelRegistry.#reloadStaticModels, so a no-op when
   * nothing changed. 'offline' skips network discovery — the provider CRUD
   * domain calls this after writing models.yml so GUI edits are live without
   * a host restart.
   */
  async refreshModels() {
    await this.#boot();
    if (!this.modelRegistry) return;
    await this.modelRegistry.refresh('offline');
  }

  /** Public boot barrier for endpoint handlers that need registry state. */
  async ready() {
    await this.#boot();
  }

  projectIdFor(directoryKey) {
    return this.#projectId(normalizeDirectoryKey(directoryKey));
  }

  /**
   * Move a session to another project directory: relocate the transcript via
   * omp's SessionManager.moveTo and migrate the sidecar metadata.
   */
  async moveSession({ sessionID, destination }) {
    await this.#boot();
    const toKey = normalizeDirectoryKey(destination);
    const live = this.sessions.get(sessionID);
    const fromKey = live?.directory
      ?? (await this.#locateDirectory(sessionID));
    if (!fromKey) return null;
    const file = await this.#findSessionFile(sessionID, fromKey);
    if (file) {
      const manager = await SessionManager.open(file.path);
      try {
        await manager.moveTo(toKey, this.#sessionDirFor(toKey));
      } finally {
        await manager.close();
      }
    }
    this.registry.move(fromKey, toKey, sessionID);
    const session = await this.getSession({ sessionID, directory: toKey });
    if (session) this.bus.emit('session.updated', { sessionID, info: session }, toKey);
    return session;
  }

  async #locateDirectory(sessionID) {
    for (const [id, live] of this.sessions) {
      if (id === sessionID) return live.directory;
    }
    const byDirectory = await this.listAllSessions({});
    for (const [directory, list] of byDirectory) {
      if (list.some((session) => session.id === sessionID)) return directory;
    }
    return null;
  }
}
