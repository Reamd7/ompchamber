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
import path from 'node:path';
import crypto from 'node:crypto';
import {
  ModelRegistry,
  SessionManager,
  createAgentSession,
  discoverAuthStorage,
} from '@oh-my-pi/pi-coding-agent';
import { SessionMetaRegistry, normalizeDirectoryKey } from './registry.js';
import { WireEventBus } from './events.js';
import {
  StreamProjector,
  projectConversation,
  projectUserMessage,
  splitModelSelector,
  paginateProjectedMessages,
} from './projection.js';

const IDLE_SESSION_TTL_MS = 30 * 60 * 1000;
const MAX_LIVE_SESSIONS = 12;

const textOfContent = (content) => {
  if (typeof content === 'string') return content;
  if (!Array.isArray(content)) return '';
  return content.filter((b) => b && b.type === 'text').map((b) => b.text).join('');
};

const modelSelector = (model) => (model ? `${model.provider}/${model.id}` : undefined);

export class OmpHostEngine {
  constructor({ agentDir } = {}) {
    this.authStorage = null;
    this.modelRegistry = null;
    this.registry = new SessionMetaRegistry({ agentDir });
    this.bus = new WireEventBus();
    /** @type {Map<string, HostSession>} */
    this.sessions = new Map();
    /** @type {Map<string, { agent: string, prompt: string, tools?: string[] }>} */
    this.customAgents = new Map();
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
      this.#loadCustomAgents();
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
  #metaConfigPath() {
    return path.join(this.registry.registryRoot, 'openchamber-agents.json');
  }

  #loadCustomAgents() {
    try {
      const parsed = JSON.parse(fs.readFileSync(this.#metaConfigPath(), 'utf8'));
      for (const agent of Array.isArray(parsed?.agents) ? parsed.agents : []) {
        if (agent && typeof agent.name === 'string') this.customAgents.set(agent.name, agent);
      }
    } catch {
      // No custom agents yet.
    }
  }

  saveCustomAgents() {
    fs.mkdirSync(this.registry.registryRoot, { recursive: true });
    fs.writeFileSync(
      this.#metaConfigPath(),
      JSON.stringify({ agents: [...this.customAgents.values()] }, null, 2),
    );
  }

  upsertCustomAgent(agent) {
    this.customAgents.set(agent.name, agent);
    this.saveCustomAgents();
  }

  deleteCustomAgent(name) {
    this.customAgents.delete(name);
    this.saveCustomAgents();
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
  #wireSession(info, directoryKey, meta) {
    const selector = meta?.model ? splitModelSelector(meta.model) : null;
    return {
      id: info.id,
      slug: info.id,
      projectID: this.#projectId(directoryKey),
      directory: normalizeDirectoryKey(info.cwd || directoryKey),
      parentID: meta?.parentID,
      title: meta?.title ?? info.title ?? 'Untitled',
      ...(meta?.agent ? { agent: meta.agent } : {}),
      ...(selector ? { model: { id: selector.modelID, providerID: selector.providerID } } : {}),
      version: 'omp',
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
      out.push(this.#wireSession(info, directoryKey, metas.get(info.id)));
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
      ...(agent ? { agent } : {}),
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
    this.bus.emit('session.deleted', { sessionID, info: info ?? { id: sessionID } }, directoryKey);
    return true;
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
    const live = this.sessions.get(sessionID);
    if (live?.agentSession) {
      const meta = this.registry.get(directoryKey, sessionID);
      return projectConversation(live.agentSession.messages, {
        sessionID,
        directory: directoryKey,
        agent: meta?.agent ?? 'build',
      });
    }
    const file = await this.#findSessionFile(sessionID, directoryKey);
    if (!file) return null;
    const manager = await SessionManager.open(file.path);
    try {
      const context = manager.buildSessionContext({ transcript: true });
      const meta = this.registry.get(directoryKey, sessionID);
      return projectConversation(context.messages ?? [], {
        sessionID,
        directory: directoryKey,
        agent: meta?.agent ?? 'build',
      });
    } finally {
      await manager.close();
    }
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
    const existing = this.sessions.get(sessionId);
    if (existing?.agentSession) return existing;
    const file = await this.#findSessionFile(sessionId, directoryKey);
    if (!file) return null;
    const manager = await SessionManager.open(file.path, this.#sessionDirFor(directoryKey));
    const meta = this.registry.get(directoryKey, sessionId);
    const model = this.#resolveModel(
      meta?.model ? splitModelSelector(meta.model) : undefined,
    ) ?? this.modelRegistry.getAvailable()[0];
    const customAgent = meta?.agent && meta.agent !== 'build' && meta.agent !== 'plan'
      ? this.customAgents.get(meta.agent)
      : null;
    const { session } = await createAgentSession({
      cwd: directoryKey,
      sessionManager: manager,
      authStorage: this.authStorage,
      modelRegistry: this.modelRegistry,
      ...(model ? { model } : {}),
      ...(meta?.agent === 'plan' ? { planYolo: { autoApproveOnResolve: true } } : {}),
      ...(customAgent
        ? {
            systemPrompt: customAgent.prompt,
            ...(Array.isArray(customAgent.tools) && customAgent.tools.length > 0
              ? { toolNames: customAgent.tools }
              : {}),
          }
        : {}),
    });
    const hostSession = existing ?? {
      sessionId,
      directory: directoryKey,
      agentSession: null,
      unsubscribe: null,
      projector: null,
      lastTouched: Date.now(),
      lastUserWireId: null,
      currentAgent: meta?.agent ?? 'build',
    };
    hostSession.agentSession = session;
    hostSession.lastTouched = Date.now();
    hostSession.unsubscribe = session.subscribe((event) => {
      try {
        this.#handleEngineEvent(hostSession, event);
      } catch (error) {
        console.error('[omp-host] event projection error:', error);
      }
    });
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

  #handleEngineEvent(hostSession, event) {
    const { sessionId, directory } = hostSession;
    const session = hostSession.agentSession;
    if (!session) return;
    switch (event.type) {
      case 'message_start': {
        if (event.message?.role !== 'assistant') return;
        hostSession.projector = new StreamProjector({
          sessionID: sessionId,
          directory,
          agent: hostSession.currentAgent,
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
        hostSession.projector.finishAssistant(
          event.message,
          hostSession.turnToolResults ?? new Map(),
        );
        return;
      }
      case 'tool_execution_start': {
        hostSession.projector?.toolStarted(event.toolCallId, event.toolName, event.args);
        return;
      }
      case 'tool_execution_end': {
        hostSession.projector?.toolFinished(event.toolCallId, {
          output: typeof event.result === 'string' ? event.result : JSON.stringify(event.result ?? ''),
          error: event.isError ? String(event.result ?? 'Tool error') : undefined,
        });
        const results = hostSession.turnToolResults ?? new Map();
        results.set(event.toolCallId, {
          content: typeof event.result === 'string' ? [{ type: 'text', text: event.result }] : [],
          isError: Boolean(event.isError),
          timestamp: Date.now(),
        });
        hostSession.turnToolResults = results;
        return;
      }
      case 'agent_start': {
        hostSession.turnToolResults = new Map();
        this.bus.emit('session.status', { sessionID: sessionId, status: { type: 'busy' } }, directory);
        return;
      }
      case 'agent_end': {
        const projector = hostSession.projector;
        if (projector?.current) {
          projector.finishAssistant(
            session.getLastAssistantMessage() ?? { content: [], timestamp: Date.now(), usage: {}, model: '' },
            hostSession.turnToolResults ?? new Map(),
          );
        }
        hostSession.projector = null;
        hostSession.turnToolResults = null;
        this.registry.update(directory, sessionId, { timeUpdated: Date.now() });
        const info = this.#wireSessionFromLive(hostSession);
        this.bus.emit('session.updated', { sessionID: sessionId, info }, directory);
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
      case 'notice': {
        if (event.level === 'error') console.error('[omp-host]', event.message);
        return;
      }
      default:
        return;
    }
  }

  /**
   * Send a user prompt. Returns the wire user message.
   */
  async prompt({ sessionID, directory, text, model, agent, images, delivery, messageID }) {
    await this.#boot();
    const directoryKey = normalizeDirectoryKey(directory);
    const hostSession = (await this.#materialize(sessionID, directoryKey));
    if (!hostSession) return null;
    const session = hostSession.agentSession;
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
    const agentName = agent ?? meta?.agent ?? 'build';
    if (agentName !== hostSession.currentAgent) {
      // Agent switch: the agent definition shapes the session's system prompt
      // and toolset at construction, so rebuild the AgentSession over the same
      // transcript.
      this.#disposeSession(hostSession);
      this.registry.update(directoryKey, sessionID, { agent: agentName });
      const rebuilt = await this.#materialize(sessionID, directoryKey);
      if (!rebuilt) return null;
      return this.prompt({ sessionID, directory: directoryKey, text, model, agent: agentName, images, delivery, messageID });
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
        agent: agentName,
        model: session.model,
        ...(typeof messageID === 'string' && messageID ? { wireId: messageID } : {}),
      },
    );
    hostSession.lastUserWireId = wire.info.id;
    this.bus.emit('message.updated', { sessionID, info: wire.info }, directoryKey);
    for (const part of wire.parts) {
      this.bus.emit(
        'message.part.updated',
        { sessionID, part, time: Date.now() },
        directoryKey,
      );
    }

    const textOnly = content.length === 1 && content[0].type === 'text' ? content[0].text : text ?? '';
    const imageContents = content.filter((block) => block.type === 'image');
    const useSteer = delivery === 'steer' && session.isStreaming;
    if (useSteer) {
      await session.steer(textOnly, imageContents);
    } else {
      await session.prompt(textOnly, { images: imageContents });
    }
    if (!meta?.timeCreated) {
      this.registry.update(directoryKey, sessionID, { timeCreated: wire.info.time.created });
      session.maybeStartTitleGeneration(text);
    }
    return wire;
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
    const previousLeaf = manager.getLeafId();
    manager.branch(messageID);
    this.registry.update(directoryKey, sessionID, {
      revert: { messageID, previousLeaf },
      timeUpdated: Date.now(),
    });
    const session = this.#wireSessionFromLive(hostSession);
    session.revert = { messageID };
    this.bus.emit('session.updated', { sessionID, info: session }, directoryKey);
    return session;
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

  async getSessionStatuses({ directory }) {
    await this.#boot();
    const statuses = {};
    for (const [id, live] of this.sessions) {
      if (live.directory !== normalizeDirectoryKey(directory)) continue;
      statuses[id] = live.agentSession?.isStreaming ? { type: 'busy' } : { type: 'idle' };
    }
    return statuses;
  }

  async shutdown() {
    clearInterval(this.sweeper);
    for (const session of this.sessions.values()) {
      this.#disposeSession(session);
    }
    this.sessions.clear();
  }

  availableModels() {
    return this.modelRegistry?.getAvailable() ?? [];
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
