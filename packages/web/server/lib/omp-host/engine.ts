// omp engine manager: embeds @oh-my-pi/pi-coding-agent sessions behind the
// OpenCode-compatible wire surface.
//
// One HostSession per OMPChamber session id. Transcripts live in omp's
// SessionManager JSONL files (cwd-derived directory); OMPChamber-specific
// metadata lives in the sidecar registry. Cold reads project the persisted
// transcript without materializing an agent; the first prompt (or any live
// operation) materializes a full AgentSession whose event stream is projected
// into wire events on the host bus.

import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import crypto from 'node:crypto';
import { AgentRegistry, ModelRegistry, SessionManager, BUILTIN_TOOLS, createAgentSession, discoverAuthStorage } from '@oh-my-pi/pi-coding-agent';
import { discoverAgents, refreshAgentDiscovery } from '@oh-my-pi/pi-coding-agent/task';
import { getConfigDirs } from '@oh-my-pi/pi-coding-agent/config';
import { initializeExtensions } from '@oh-my-pi/pi-coding-agent/modes/runtime-init';
import { isTodoPhase } from '@oh-my-pi/pi-coding-agent/tools/todo';
import { buildSkillPromptMessage, parseSkillInvocation } from '@oh-my-pi/pi-coding-agent/extensibility/skills';
import { SKILL_PROMPT_MESSAGE_TYPE } from '@oh-my-pi/pi-coding-agent/session/messages';
import { getSessionSlashCommands } from '@oh-my-pi/pi-coding-agent/extensibility/extensions/get-commands-handler';
import type { ExtensionUIContext } from '@oh-my-pi/pi-coding-agent/extensibility/extensions';
import { SessionMetaRegistry, normalizeDirectoryKey } from './registry.ts';
import type { SessionMeta, SessionMetadataValue } from './registry.ts';
import { WireEventBus, OmpEventBus } from './events.ts';
import {
  StreamProjector,
  normalizeToolExecutionResult,
  projectConversation,
  projectCustomMessage,
  projectDeveloperMessage,
  projectDividerMessage,
  projectUserMessage,
  buildTurnStateStamper,
  projectTurnEventDivider,
  wireMessageId,
  deterministicWireId,
  resolveWireIdToEntryId,
  splitModelSelector,
  paginateProjectedMessages,
} from './projection.ts';
import type { UsageInput, ProjectedContentInput, ProjectedMessage, AssistantMessageInput, WireIdMessageInput } from './projection.ts';
import { createSettingsStore } from './domain-models.ts';
import { createDomainDialogs } from './domain-dialogs.ts';
import {
  ModeDomainError,
  createModesDomain,
  mapBackedStore,
  migrateSidecarAgents,
  personaFor,
  serializeAgentMarkdown,
} from './domain-modes.ts';
import type { PreparePlanReviewResult } from './domain-modes.ts';
import { createDomainChrome } from './domain-chrome.ts';
import { errorText, errorCode, ompFeatures } from './omp-parity.ts';
import { revealCommand } from './domain-plugins.ts';
import {
  createUriDomain,
  createLocalProtocolOptions,
  buildEntryTreeSnapshot,
  ARTIFACTS_MAX_FILES_PER_SESSION,
  artifactsDirForSessionFile,
} from './domain-uri.ts';
import { resolveLocalUrlToPath } from '@oh-my-pi/pi-coding-agent/internal-urls/local-protocol';
import type { AgentSession, AgentSessionEvent, AuthStorage, CreateAgentSessionResult, SessionInfo, SessionEntry } from '@oh-my-pi/pi-coding-agent';
import type { CustomMessage, HookMessage } from '@oh-my-pi/pi-coding-agent';
import type { SettingsStore, RegistryModel } from './domain-models.ts';
import type { DialogsDomain } from './domain-dialogs.ts';
import type { ModesDomain } from './domain-modes.ts';
import type { DomainChrome } from './domain-chrome.ts';
import type { UriDomain } from './domain-uri.ts';
const IDLE_SESSION_TTL_MS = 30 * 60 * 1000;
// Cap on concurrently live top-level sessions; idle sessions beyond this
// are swept after IDLE_SESSION_TTL_MS. Referenced by #sweepIdleSessions —
// it was missing entirely and killed the engine on the first 60s sweep.
const MAX_LIVE_SESSIONS = 16;

// Bound on how long engine.abort waits for AgentSession.abort's teardown
// (post-prompt drain + agent idle). The pi drain has no internal timeout on
// the abort path (dispose caps it at 5s; abort does not), so one signal-blind
// tool or never-settling post-prompt task would park the stop request forever.
const ABORT_TEARDOWN_TIMEOUT_MS = 10_000;
// SAFETY: single boundary cast — DialogBridge is the deliberate web
// degradation of the SDK's ExtensionUIContext (stub theme; custom()
// resolves void instead of the generic T). The extension runner only
// consumes the web-capable subset at runtime.
const asExtensionUiContext = <T,>(bridge: T): ExtensionUIContext | undefined =>
  bridge as ExtensionUIContext | undefined;

/**
 * Session-level persona key (02 §5.1 D-B3): unset and the deleted
 * build/plan pair map to the standard session; any other name is a persona.
 */
const personaKeyFor = (name: string | undefined): string => (!name || name === 'build' || name === 'plan' ? 'standard' : name);

/** Wire `agent` projection: the standard session keeps the legacy 'build' id. */
const wireAgentFor = (personaKey: string): string => (personaKey === 'standard' ? 'build' : personaKey);

const textOfContent = (content: ProjectedContentInput | null | undefined): string => {
  if (typeof content === 'string') return content;
  if (!Array.isArray(content)) return '';
  return content
    .filter((b) => b && b.type === 'text')
    .map((b) => b.text)
    .join('');
};

const modelSelector = (model: { provider: string; id: string } | null | undefined): string | undefined => (model ? `${model.provider}/${model.id}` : undefined);

/** Persona record (spec 02 §5.2): mirrored into the personas sidecar. */
interface Persona {
  name: string;
  description?: string;
  systemPrompt?: string;
  tools?: string[];
}

type SdkAgentSession = AgentSession;
type SdkSetToolUIContext = CreateAgentSessionResult['setToolUIContext'];

/** Plugin discovery snapshot frozen at session materialization (plugins.v1). */
interface AppliedPluginsSnapshot {
  appliedAt: number;
  extensionPaths: string[];
  pluginNames: string[];
}

/** Engine-side record for one live omp-host session id. */
interface HostSession {
  sessionId: string;
  directory: string;
  agentSession: SdkAgentSession | null;
  sdkResult: Pick<CreateAgentSessionResult, 'setToolUIContext'>;

  currentPersona: string;
  projector: StreamProjector | null;
  lastTouched: number;
  pendingUserWireId: string | null;
  lastUserWireId: string | null;
  syncedEntryKeys?: Set<string>;
  lastAssistantWireId: string | null;
  awaitingAsyncSince: number | null;
  agentRegistry: AgentRegistry;
  extensionUiInitialized: boolean;
  extensionUiPromise: Promise<unknown> | null;
  planHandlerAttached: boolean;
  appliedPlugins: AppliedPluginsSnapshot | null;
  /** Per-turn tool-result pairing map (tool_execution_end → message_end settle). */
  turnToolResults?: Map<string, { content?: unknown; isError?: boolean; timestamp?: number }> | null;
  unsubscribe?: () => void;
  /** Agent-registry event subscription feeding the agent-runs aggregator. */
  unsubscribeAgentRegistry?: () => void;
}

/** The SessionManager surface #infoFromManager reads (SDK SessionManager). */
type SessionManagerLike = {
  getHeader(): { timestamp?: string } | null | undefined;
  getEntries(): Array<{ timestamp?: string }> | null | undefined;
  getSessionId(): string;
  getCwd(): string | undefined;
  getSessionName(): string | undefined;
};

/** #tailSyncTranscript result: wire ids emitted this pass + divider anchor. */
interface TailSyncTail {
  projected: Array<{ wireId: string | null; role: string }>;
  lastCompactionId: string | null;
}

/** Wire `Session` record as projected by #wireSession (vendored contract's
 * fields, server-side copy; `revert` is attached only by the revert flow). */
interface WireSessionRecord {
  id: string;
  slug: string;
  projectID: string;
  directory: string;
  parentID?: string;
  /** Fork lineage (§5.4): wire parentID stays subagent-only; a user fork
   * must remain a normal promptable session in the shared UI. */
  forkParentID?: string;
  title: string;
  agent?: string;
  model?: { id: string; providerID: string };
  metadata?: Record<string, SessionMetadataValue>;
  time: { created: number; updated: number; archived?: number };
  revert?: { messageID: string };
}
/** The SessionInfo fields the wire projection reads. Synthesized rows
 * (registry-only, live-only) provide exactly these; full SDK SessionInfos
 * are structurally assignable. `created`/`modified` tolerate Date-or-string
 * because cold reads hand through transcript timestamps unparsed. */
interface SessionListInfo {
  id: string;
  cwd: string;
  title?: string;
  created: Date | string;
  modified: Date | string;
}

export class OmpHostEngine {
  /** In-flight #materialize dedup (`${directory}\0${sessionId}` → promise); prevents duplicate AgentSessions on lease/prompt races. */
  #materializeInFlight = new Map<string, Promise<HostSession>>();

  authStorage: AuthStorage | null;
  modelRegistry: ModelRegistry | null;
  registry: SessionMetaRegistry;
  bus: WireEventBus;
  /** omp-native event channel (spec 05 §5.2, master D6-R1 single authority). */
  ompBus: OmpEventBus;
  sessions: Map<string, HostSession>;
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
   */
  wireIdOverrides: Map<string, string>;
  /** Personas (OC-original optional layer, spec 02 §5.2/R12). */
  personas: Map<string, Persona>;
  /** Per-directory keyed Settings store (spec 06 §5.1, master R6). */
  settingsStore: SettingsStore | null;
  // Approval/ask dialog domain (spec 03, master R10/R11/R13). Lease-driven
  // hasUI: unattended sessions never hold a lease → SDK fail-closed.
  dialogs: DialogsDomain;
  // Modes/plan/goal/personas/agent-definitions domain (spec 02).
  modesDomain: ModesDomain;
  chrome: DomainChrome;
  uriDomain: UriDomain;
  bootError: unknown;
  bootPromise: Promise<void> | null;
  sweeper: ReturnType<typeof setInterval>;
  /** Lazily-created counters for AgentSessionEvent members with no manifest case. */
  unknownEventCounts: Map<string, number> | undefined;
  /** How long abort() waits for the agent teardown before force-disposing. */
  abortTeardownTimeoutMs: number;

  constructor({ agentDir, abortTeardownTimeoutMs }: { agentDir?: string; abortTeardownTimeoutMs?: number } = {}) {
    this.authStorage = null;
    this.modelRegistry = null;
    this.registry = new SessionMetaRegistry({ agentDir });
    this.bus = new WireEventBus();
    /** How long abort() waits for the agent teardown before force-disposing (test injectable). */
    this.abortTeardownTimeoutMs = abortTeardownTimeoutMs ?? ABORT_TEARDOWN_TIMEOUT_MS;
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
          console.warn('[omp-host] failed to attach dialog UI:', errorText(error));
        });
      },
      onSessionUiDetached: ({ directory, sessionId }) => this.#detachDialogUi(directory, sessionId),
      onDiagnostic: (note) => console.warn('[omp-host] dialog lifecycle:', note)
    });
    // Modes/plan/goal/personas/agent-definitions domain (spec 02).
    this.modesDomain = createModesDomain({
      publishFor:
        (sessionId, directoryKey) =>
        (type, payload, options = {}) =>
          this.ompBus.publish(type, payload, {
            directory: directoryKey,
            sessionID: sessionId,
            durable: options.durable !== false
          }),
      appendFor: (sessionId, directoryKey) => (mode, data) => {
        const hostSession = this.sessions.get(sessionId);
        const manager = hostSession?.agentSession?.sessionManager;
        if (!manager?.appendModeChange) return undefined;
        const entryId = manager.appendModeChange(mode, data);
        if (hostSession) this.#syncPlanProposalHandler(hostSession, mode);
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
        readFile: async (filePath) => fs.promises.readFile(filePath, 'utf8'),
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
        projectAgentsDirFor: (directory) => path.join(path.resolve(directory), '.omp', 'agents')
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
          const settings = await store.settingsFor(directoryKey ?? undefined);
          return {
            disabledAgents: settings.get('task.disabledAgents'),
            modelOverrides: settings.get('task.agentModelOverrides'),
            prewalk: settings.get('task.agentPrewalk'),
            advisor: settings.get('task.agentAdvisor')
          };
        } catch {
          return null;
        }
      }
    });
    // Extension chrome table (spec 09 §5): string-payload widget/status
    // projection mirroring RpcExtensionUIRequest. Volatile events; the
    // snapshot GET is the reconnect authority (D2).
    this.chrome = createDomainChrome({
      publishFor: (directory, payload) =>
        this.ompBus.publish('omp.chrome.updated', payload, {
          directory,
          durable: false
        })
    });
    // URI bridge / session tree / agent-runs / jobs (spec 04). The factory
    // is synchronous and every engine dependency is a lazy closure, so it is
    // created here (not in async #boot) and mounted synchronously by
    // endpoints.js at route-registration time.
    this.uriDomain = createUriDomain({
      features: () => ompFeatures(),
      localOptionsFor: async (sessionId, directoryKey) => {
        const artifactsDir = await this.#artifactsDirFor(sessionId, directoryKey);
        return artifactsDir ? createLocalProtocolOptions(sessionId, directoryKey, artifactsDir) : null;
      },
      sessionTreeData: async (directory) => this.listSessions({ directory: directory ?? undefined }),
      entryTreeFor: async (sessionID, directory) => {
        const file = await this.#findSessionFile(sessionID, normalizeDirectoryKey(directory));
        if (!file) return null;
        const manager = await SessionManager.open(file.path);
        return { manager };
      },
      localFiles: (sessionID, directory) =>
        this.#listLocalFiles(sessionID, normalizeDirectoryKey(directory)),
      agentsSnapshot: () =>
        [...this.sessions.values()]
          .filter((hostSession) => hostSession.agentRegistry && hostSession.agentSession)
          .map((hostSession) => ({
            sessionID: hostSession.sessionId,
            directory: hostSession.directory,
            registry: hostSession.agentRegistry
          })),
      publish: (type, payload, scope) => this.ompBus.publish(type, payload, scope),
      liveSessionIds: () => [...this.sessions.keys()],
      agentRunTranscript: (sessionID, agentId, directory) => this.getAgentRunTranscript({ sessionID, agentId, directory }),
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
        console.warn('[omp-host] agent sidecar migration failed:', errorText(error));
      });
      // Per-directory keyed Settings store (spec 06 §5.1, master R6). The
      // boot instance doubles as the global-write executor; sessions inject
      // their directory's instance via options.settings (sdk.ts:1273-1275).
      if (!this.settingsStore) {
        try {
          this.settingsStore = await createSettingsStore({
            cwd: process.cwd(),
            agentDir: this.registry.agentDir
          });
        } catch (error) {
          // Degrade to no-injection (pre-R6 behavior) instead of bricking
          // every session; the settings endpoints surface the error.
          console.warn('[omp-host] settings store unavailable:', errorText(error));
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

  async #setDialogUiContext(hostSession: HostSession, directory: string, sessionId: string, hasUI: boolean) {
    const uiContext = hasUI
      ? this.dialogs.uiContextFor(directory, sessionId, {
          chrome: this.chrome.bridgeHandlersFor(directory, sessionId)
        })
      : undefined;
    if (hasUI && !hostSession.extensionUiInitialized) {
      if (!hostSession.extensionUiPromise && hostSession.agentSession) {
        hostSession.extensionUiPromise = initializeExtensions(hostSession.agentSession, {
          uiContext: asExtensionUiContext(uiContext),
          mode: 'json',
          reportSendError: (action, error) => {
            console.warn(`[omp-host] ${action} failed:`, errorText(error));
          },
          reportRuntimeError: (error) => {
            console.warn('[omp-host] extension runtime error:', error?.error ?? error);
          },
          onShutdown: () => {}
        })
          .then(() => {
            hostSession.extensionUiInitialized = true;
          })
          .finally(() => {
            hostSession.extensionUiPromise = null;
          });
      }
      await hostSession.extensionUiPromise;
    }
    // SAFETY: DialogBridge is the deliberate web degradation of ExtensionUIContext
    // (asExtensionUiContext seam); the SDK consumes the same subset and treats
    // an undefined context as "no UI" when the lease is absent.
    this.#applyToolUiContext(hostSession.sdkResult, asExtensionUiContext(uiContext), hasUI);
  }

  /**
   * Lease attach/detach → SDK tool UI context (R13: lease is hasUI
   * authority). A UI lease IS a session access: a client viewing the session
   * implies the engine should hold it live, so an attach that races ahead of
   * lazy materialization pulls the session in instead of dropping the
   * extension UI initialization on the floor.
   */
  #applyToolUiContext(sdkResult: Pick<CreateAgentSessionResult, 'setToolUIContext'> | undefined, uiContext: ReturnType<typeof asExtensionUiContext>, hasUI: boolean): void {
    // SAFETY: web degradation seam — undefined means "no UI bridge" and
    // pairs with hasUI=false; the SDK member accepts it at runtime.
    (sdkResult?.setToolUIContext as ((uiContext: ExtensionUIContext | undefined, hasUI: boolean) => void) | undefined)?.(uiContext, hasUI);
  }

  async #attachDialogUi(directory: string, sessionId: string) {
    const hostSession = this.sessions.get(sessionId) ?? (await this.#materialize(sessionId, directory));
    if (!hostSession) return;
    await this.#setDialogUiContext(hostSession, directory, sessionId, true);
  }

  #detachDialogUi(directory: string, sessionId: string) {
    const hostSession = this.sessions.get(sessionId);
    if (!hostSession) return;
    try {
      this.#applyToolUiContext(hostSession.sdkResult, undefined, false);
    } catch {
      // Session may already be disposed.
    }
  }

  /**
   * Plan mode ↔ xd://propose bridge (spec 02 §5.5): entering plan attaches
   * the review bridge; any other mode clears the handler.
   */
  #syncPlanProposalHandler(hostSession: HostSession, mode: string) {
    const session = hostSession?.agentSession;
    if (!session?.setPlanProposalHandler) return;
    if (mode === 'plan') {
      const bridge = this.modesDomain.bridgeFor(hostSession.sessionId, hostSession.directory);
      // SAFETY: AgentSession satisfies PlanProposalSession (preparePlanForReview)
      // structurally; the mode domain narrows to the single member it calls.
      // The mode domain consumes only preparePlanForReview (PlanProposalSession);
      // delegate through an adapter so the SDK AgentSession keeps its own type.
      const planSession = {
        preparePlanForReview: async (title: string): Promise<PreparePlanReviewResult> => {
          // SAFETY: AgentToolResult<PlanApprovalDetails> IS the
          // PreparePlanReviewResult shape by design (02 §5.5): { content, details }.
          return (await session.preparePlanForReview(title)) as PreparePlanReviewResult;
        }
      };
      // SAFETY: PlanReviewToolResult is the AgentToolResult shape by design
      // (02 §5.5); the SDK handler and the mode hook return the same wire form.
      const hook = bridge.hookFor(planSession);
      // SAFETY: hook's PlanReviewToolResult is the handler's AgentToolResult arm.
      session.setPlanProposalHandler(hook as Parameters<NonNullable<typeof session.setPlanProposalHandler>>[0]);
      hostSession.planHandlerAttached = true;
    } else if (hostSession.planHandlerAttached) {
      session.setPlanProposalHandler(null);
      hostSession.planHandlerAttached = false;
    }
  }

  #personasConfigPath() {
    return path.join(this.registry.registryRoot, 'ompchamber-personas.json');
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
    fs.writeFileSync(this.#personasConfigPath(), JSON.stringify({ personas: [...this.personas.values()] }, null, 2));
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
      const entry = getConfigDirs('agents', { project: false }).find((dir) => dir?.source === '.omp' && typeof dir?.path === 'string');
      if (entry) return entry.path;
    } catch {
      // Derived fallback below.
    }
    return path.join(os.homedir(), '.omp', 'agent', 'agents');
  }

  /**
   * One-time sidecar → omp migration (02 §6.2): each legacy
   * `ompchamber-agents.json` record becomes a user-scope worker `.md`
   * (frontmatter description/tools, body prompt) plus a mirrored persona so
   * existing `meta.agent` sessions keep resolving. Runs before the request
   * surface opens; any failure keeps the sidecar for an idempotent retry.
   */
  async #migrateAgentsSidecar() {
    const sidecarPath = path.join(this.registry.registryRoot, 'ompchamber-agents.json');
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
            description: typeof record.description === 'string' && record.description.trim() ? record.description : record.name,
            systemPrompt: typeof record.prompt === 'string' ? record.prompt : '',
            ...(Array.isArray(record.tools) && record.tools.length > 0 ? { tools: record.tools } : {})
          }),
          'utf8'
        );
      },
      personaExists: (name) => this.personas.has(name),
      mirrorPersona: (record) => {
        this.personas.set(record.name, {
          name: record.name,
          ...(record.description ? { description: record.description } : {}),
          ...(typeof record.prompt === 'string' && record.prompt ? { systemPrompt: record.prompt } : {}),
          ...(Array.isArray(record.tools) && record.tools.length > 0 ? { tools: record.tools } : {})
        });
      },
      markDone: () => {
        try {
          fs.renameSync(sidecarPath, `${sidecarPath}.migrated-${Date.now()}`);
          this.savePersonas();
          done = true;
        } catch (error) {
          console.warn('[omp-host] sidecar migration markDone failed:', errorText(error));
        }
      },
      log: (message, error) => console.warn('[omp-host] agent sidecar migration:', message, error ?? '')
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

  #disposeSession(session: HostSession | undefined) {
    if (!session || !session.agentSession) return;
    session.unsubscribe?.();
    session.unsubscribeAgentRegistry?.();
    session.unsubscribe = undefined;
    session.unsubscribeAgentRegistry = undefined;
    // Domain release: modes tracker + pending dialogs for this session (R11).
    this.modesDomain?.release?.(session.sessionId, session.directory);
    this.dialogs?.registry?.abortForSession?.(
      {
        directory: session.directory,
        sessionId: session.sessionId
      },
      'session disposed'
    );
    const agentSession = session.agentSession;
    session.agentSession = null;
    void agentSession.dispose().catch(() => {});
  }
  #sessionDirFor(cwd: string) {
    return SessionManager.getDefaultSessionDir(cwd, this.registry.agentDir);
  }

  #projectId(directoryKey: string) {
    const hash = crypto.createHash('sha256').update(directoryKey).digest('hex');
    return `prj_${hash.slice(0, 20)}`;
  }
  /**
   * Per-session artifacts directory for local:// root pinning (TUI parity,
   * spec 04 §5.2.3): live sessions answer from their own SessionManager;
   * cold sessions derive it from the transcript path (sessionFile minus
   * '.jsonl'). Never the project-level session directory — sessions in one
   * directory keep private local:// roots, and cross-session carry-over is
   * the SDK's explicit copy semantics (fork/plan-approve), not shared state.
   * Null when the session is unknown to this directory.
   */
  async #artifactsDirFor(sessionId: string, directoryKey: string) {
    const manager = this.sessions.get(sessionId)?.agentSession?.sessionManager;
    const liveDir = manager?.getArtifactsDir?.();
    if (typeof liveDir === 'string' && liveDir) return liveDir;
    const file = await this.#findSessionFile(sessionId, directoryKey);
    return file ? artifactsDirForSessionFile(file.path) : null;
  }

  /** Bounded walk depth for #listLocalFiles — local:// roots are shallow
   *  (plans, handoff notes, scratch); anything deeper is a runaway, not data. */
  static #LOCAL_WALK_MAX_DEPTH = 8;

  /**
   * Read-only file rows for one session's local:// root (artifacts browse,
   * spec 04). Returns null when the session is unknown to the directory;
   * an absent root is authoritative empty. Pure stat walk — no content
   * leaves this method; refs are '/'-joined relatives, never absolute paths.
   */
  async #listLocalFiles(sessionId: string, directoryKey: string) {
    const artifactsDir = await this.#artifactsDirFor(sessionId, directoryKey);
    if (!artifactsDir) return null;
    const options = createLocalProtocolOptions(sessionId, directoryKey, artifactsDir);
    const root = resolveLocalUrlToPath('local://', options);
    const files: Array<{ ref: string; size?: number; modifiedAt?: number }> = [];
    let truncated = false;
    const walk = async (relative: string, depth: number) => {
      let entries;
      try {
        entries = await fs.promises.readdir(relative ? path.join(root, relative) : root, {
          withFileTypes: true,
        });
      } catch (error) {
        if (errorCode(error) === 'ENOENT') return; // no local root yet — authoritative empty
        throw error;
      }
      for (const entry of entries) {
        const childRef = relative ? `${relative}/${entry.name}` : entry.name;
        if (entry.isDirectory()) {
          if (depth >= OmpHostEngine.#LOCAL_WALK_MAX_DEPTH) {
            truncated = true;
            continue;
          }
          await walk(childRef, depth + 1);
        } else if (entry.isFile()) {
          if (files.length >= ARTIFACTS_MAX_FILES_PER_SESSION) {
            truncated = true;
            return;
          }
          const stat = await fs.promises.stat(path.join(root, childRef)).catch((): null => null);
          files.push({
            ref: childRef,
            size: stat?.size ?? 0,
            modifiedAt: stat?.mtimeMs ?? 0,
          });
        }
      }
    };
    await walk('', 0);
    return { files, truncated };
  }

  /**
   * Wire Session record for an omp SessionInfo + registry metadata.
   */
  #wireSession(info: SessionListInfo, directoryKey: string, meta: SessionMeta | undefined, live?: HostSession | undefined): WireSessionRecord {
    // The live session's actual model wins over the sidecar projection — a
    // roles-resolved session (no registry selector) still reports the model
    // it is really running (spec 01 §5.5 badge seeding).
    const selector = meta?.model
      ? splitModelSelector(meta.model)
      : live?.agentSession?.model
        ? {
            providerID: live.agentSession.model.provider,
            modelID: live.agentSession.model.id
          }
        : null;
    return {
      id: info.id,
      slug: info.id,
      projectID: this.#projectId(directoryKey),
      directory: normalizeDirectoryKey(info.cwd || directoryKey),
      parentID: meta?.parentID,
      // Fork lineage rides a dedicated field: wire `parentID` means subagent
      // parentage and the shared UI makes parentID sessions read-only
      // ("subagent sessions cannot be prompted"). A user fork must stay a
      // normal promptable session.
      forkParentID: meta?.forkParentID,
      title: meta?.title ?? info.title ?? 'Untitled',
      ...(personaKeyFor(meta?.persona ?? meta?.agent) !== 'standard' ? { agent: wireAgentFor(personaKeyFor(meta?.persona ?? meta?.agent)) } : {}),
      ...(selector ? { model: { id: selector.modelID, providerID: selector.providerID } } : {}),
      ...(meta?.metadata ? { metadata: meta.metadata } : {}),
      time: {
        created: info.created instanceof Date ? info.created.getTime() : Date.parse(info.created) || Date.now(),
        updated: info.modified instanceof Date ? info.modified.getTime() : Date.parse(info.modified) || Date.now(),
        ...(meta?.timeArchived ? { archived: meta.timeArchived } : {})
      }
    };
  }

  async listSessions({ directory }: { directory?: string }) {
    await this.#boot();
    const directoryKey = normalizeDirectoryKey(directory);
    const cwd = directory ?? '';
    const infos = await SessionManager.list(cwd, this.#sessionDirFor(cwd), undefined);
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
            cwd: directory ?? '',
            title: meta.title,
            created: new Date(meta.timeCreated ?? Date.now()),
            modified: new Date(meta.timeUpdated ?? Date.now())
          },
          directoryKey,
          meta
        )
      );
    }
    return out;
  }

  async listAllSessions({ archived }: { archived?: boolean } = {}) {
    await this.#boot();
    const infos = await SessionManager.listAll();
    const byDirectory = new Map();
    for (const info of infos) {
      const directoryKey = normalizeDirectoryKey(info.cwd);
      const meta = (this.registry.get(directoryKey, info.id) ?? undefined);
      if (archived === false && meta?.timeArchived) continue;
      const list = byDirectory.get(directoryKey) ?? [];
      list.push(this.#wireSession(info, directoryKey, meta));
      byDirectory.set(directoryKey, list);
    }
    return byDirectory;
  }

  async createSession({ directory, title, parentID, agent, model }: { directory?: string; title?: string; parentID?: string; agent?: string; model?: { providerID?: string; modelID?: string } }) {
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
      ...(model ? { model: `${model.providerID ?? ''}/${model.modelID ?? ''}` || undefined } : {})
    });
    await manager.close();
    const session = this.#wireSession(
      {
        id: sessionId,
        cwd,
        title,
        created: new Date(now),
        modified: new Date(now)
      },
      cwd,
      (this.registry.get(cwd, sessionId) ?? undefined)
    );
    this.bus.emit('session.created', { sessionID: sessionId, info: session }, cwd);
    return session;
  }

  async getSession({ sessionID, directory }: { sessionID: string; directory?: string }) {
    await this.#boot();
    const directoryKey = normalizeDirectoryKey(directory);
    const live = this.sessions.get(sessionID);
    if (live) return this.#wireSessionFromLive(live);
    const file = await this.#findSessionFile(sessionID, directoryKey);
    if (!file) return null;
    const manager = await SessionManager.open(file.path);
    try {
      const info = this.#infoFromManager(manager, file.path, directoryKey);
      return this.#wireSession(info, directoryKey, (this.registry.get(directoryKey, sessionID) ?? undefined));
    } finally {
      await manager.close();
    }
  }

  #infoFromManager(manager: SessionManagerLike, filePath: string, directoryKey: string) {
    const header = manager.getHeader();
    const entries = manager.getEntries() ?? [];
    const last = entries[entries.length - 1];
    const created = header?.timestamp ? Date.parse(header.timestamp) : Date.now();
    const modified = last?.timestamp ? Date.parse(last.timestamp) : created;
    return {
      id: manager.getSessionId(),
      cwd: manager.getCwd() || directoryKey,
      title: manager.getSessionName(),
      created: new Date(created),
      modified: new Date(Number.isFinite(modified) ? modified : created)
    };
  }

  async #findSessionFile(sessionID: string, directoryKey: string) {
    const dir = this.#sessionDirFor(directoryKey);
    const infos = await SessionManager.list(directoryKey, dir);
    const hit = infos.find((info) => info.id === sessionID);
    if (hit) return { path: hit.path, dir };
    return null;
  }

  async updateSession({ sessionID, directory, title, metadata, timeArchived }: { sessionID: string; directory?: string; title?: string; metadata?: Record<string, SessionMetadataValue>; timeArchived?: number }) {
    await this.#boot();
    const live = this.sessions.get(sessionID);
    // A live session owns its registry entry under its own directory, and
    // getSession answers from the live record first regardless of the
    // requested directory. Writing the patch under a differing requested
    // directory would return and broadcast an update that was never applied,
    // and strand the patch as a phantom registry entry that listings under
    // the owning directory never read.
    const directoryKey = live ? normalizeDirectoryKey(live.directory) : normalizeDirectoryKey(directory);
    if (!live) {
      // Idle sessions are on-disk records owned by exactly one directory:
      // transcript and registry entry both live there. An update addressed to
      // a directory that owns neither is mis-addressed — writing it would
      // fabricate a phantom registry entry and answer with a session no
      // listing (keyed by the transcript's own cwd) can ever observe, so the
      // caller sees success while nothing takes effect. Refuse; registry-only
      // bookkeeping (transcript pruned externally) stays updatable.
      const hadRegistryEntry = (this.registry.get(directoryKey, sessionID) ?? undefined) != null;
      if (!hadRegistryEntry && !(await this.#findSessionFile(sessionID, directoryKey))) {
        return null;
      }
    }
    const patch: Partial<SessionMeta> = { timeUpdated: Date.now() };
    if (typeof title === 'string') patch.title = title;
    if (metadata !== undefined) patch.metadata = metadata;
    if (timeArchived !== undefined) patch.timeArchived = timeArchived || undefined;
    const meta = this.registry.update(directoryKey, sessionID, patch);
    if (live && typeof title === 'string') {
      await live.agentSession?.setSessionName(title, 'user').catch(() => {});
    }
    const session = await this.getSession({
      sessionID,
      directory: directoryKey
    });
    if (session) {
      this.bus.emit('session.updated', { sessionID, info: session }, directoryKey);
    }
    return (
      session ??
      this.#wireSession(
        {
          id: sessionID,
          cwd: directoryKey,
          created: new Date(),
          modified: new Date()
        },
        directoryKey,
        meta
      )
    );
  }

  async deleteSession({ sessionID, directory }: { sessionID: string; directory?: string }) {
    await this.#boot();
    const directoryKey = normalizeDirectoryKey(directory);
    const file = await this.#findSessionFile(sessionID, directoryKey);
    const live = this.sessions.get(sessionID);
    const info = live ? this.#wireSessionFromLive(live) : await this.getSession({ sessionID, directory: directoryKey });
    this.#disposeSession(live);
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
  #wireSessionFromLive(live: HostSession) {
    const meta = this.registry.get(live.directory, live.sessionId);
    const agentSession = live.agentSession;
    const now = Date.now();
    return this.#wireSession(
      {
        id: live.sessionId,
        cwd: live.directory,
        title: agentSession?.sessionManager.getSessionName() ?? meta?.title,
        created: new Date(meta?.timeCreated ?? now),
        modified: new Date(meta?.timeUpdated ?? now)
      },
      live.directory,
      meta ?? undefined,
      live
    );
  }

  /** Cold message projection from the persisted transcript. */
  async getMessages({ sessionID, directory }: { sessionID: string; directory?: string }) {
    return this.#projectedMessages(sessionID, directory);
  }

  /**
   * Paged cold projection for the message-history route: applies the
   * limit/before window over the full projection and reports the
   * next-older cursor (see paginateProjectedMessages).
   */
  async getMessagesPage({ sessionID, directory, limit, before }: { sessionID: string; directory?: string; limit?: number; before?: string }) {
    const projected = await this.#projectedMessages(sessionID, directory);
    if (!projected) return null;
    return paginateProjectedMessages(projected, { limit, before });
  }
  async #projectedMessages(sessionID: string, directory: string | null | undefined) {
    await this.#boot();
    const directoryKey = normalizeDirectoryKey(directory);
    const wireIdFor = this.#wireIdResolver(directoryKey, sessionID);
    const meta = (this.registry.get(directoryKey, sessionID) ?? undefined);
    const live = this.sessions.get(sessionID);
    const liveSession = live?.agentSession ?? null;
    const liveCount = liveSession?.messages?.length ?? -1;

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
        const entries = manager.getEntries() ?? [];
        // Exact per-message snapshots: fold the transcript's model_change /
        // thinking_level_change log so every user message carries the state
        // it was sent with (SDK user messages persist neither).
        const turnStateFor = buildTurnStateStamper(entries, { wireIdFor });
        // Timeline dividers for the same turn-state entries: model and mode
        // switches render as slim dividers at their point in the log.
        const mergeDividers = (projected: ProjectedMessage[]) => this.#mergeTurnEventDividers(projected, entries, sessionID);
        if (liveCount >= 0 && liveCount >= fileMessages.length) {
          return mergeDividers(
            projectConversation(liveSession?.messages ?? [], {
              sessionID,
              directory: directoryKey,
              agent: wireAgentFor(personaKeyFor(meta?.persona ?? meta?.agent)),
              wireIdFor,
              turnStateFor
            })
          );
        }
        if (fileMessages.length > 0 || liveCount < 0) {
          return mergeDividers(
            projectConversation(fileMessages, {
              sessionID,
              directory: directoryKey,
              agent: wireAgentFor(personaKeyFor(meta?.persona ?? meta?.agent)),
              wireIdFor,
              turnStateFor
            })
          );
        }
      } finally {
        await manager.close().catch(() => {});
      }
    }
    if (liveCount >= 0) {
      return projectConversation(liveSession?.messages ?? [], {
        sessionID,
        directory: directoryKey,
        agent: wireAgentFor(personaKeyFor(meta?.persona ?? meta?.agent)),
        wireIdFor
      });
    }
    return null;
  }

  /**
   * Insert turn-event dividers (model/mode switches) into a projected
   * conversation at their transcript position: before the first message
   * created at or after the entry's timestamp, or at the end. Entries the
   * divider projection rejects (init bookkeeping without a role tag) are
   * skipped, keeping deterministic ids stable across re-projections.
   */
  #mergeTurnEventDividers(projected: ProjectedMessage[], entries: readonly SessionEntry[], sessionID: string) {
    const dividers = [];
    for (const entry of entries) {
      const wire = projectTurnEventDivider(entry, { sessionID });
      if (wire) dividers.push(wire);
    }
    if (dividers.length === 0) return projected;
    const out = [...projected];
    for (const wire of dividers) {
      const at = out.findIndex((item) => (item.info.time?.created ?? 0) >= (wire.info.time?.created ?? 0));
      out.splice(at === -1 ? out.length : at, 0, wire);
    }
    return out;
  }

  /**
   * The thinking level a turn actually runs with: the session's explicit
   * pick when set, else the model's configured default (inherit), else
   * unknown (models without a thinking surface).
   */
  #effectiveThinkingLevel(session: AgentSession) {
    if (session.thinkingLevel !== undefined && session.thinkingLevel !== null) {
      return session.thinkingLevel;
    }
    const model = session.model;
    if (!model?.provider || !model?.id) return undefined;
    const entry = this.availableModels().find((candidate) => candidate.provider === model.provider && candidate.id === model.id);
    const defaultLevel = entry?.thinking?.defaultLevel;
    return typeof defaultLevel === 'string' && defaultLevel.length > 0 ? defaultLevel : undefined;
  }

  #wireIdResolver(directoryKey: string, sessionID: string) {
    if (this.wireIdOverrides.size === 0) return undefined;
    const prefix = `${directoryKey}\u0000${sessionID}\u0000`;
    return (message: WireIdMessageInput | null | undefined) => {
      if (message?.role !== 'user' && message?.role !== 'assistant') return undefined;
      return this.wireIdOverrides.get(prefix + deterministicWireId(message));
    };
  }

  /**
   * Keep a finished assistant turn's cold-projection id aligned with the id
   * the streaming projector already emitted. Live streaming derives the wire
   * id at message_start (empty content, start timestamp); the persisted
   * message finalizes both, so a re-fetch would otherwise project a second,
   * different id for the same message and the UI would render it twice.
   */
  #bridgeAssistantWireId(hostSession: HostSession, finalMessage: AssistantMessageInput) {
    const liveId = hostSession.projector?.current?.id;
    if (!liveId) return;
    const seed = textOfContent(finalMessage.content) || (finalMessage.content?.[0]?.name ?? '');
    const coldId = wireMessageId('assistant', finalMessage.timestamp, seed);
    if (coldId === liveId) return;
    this.wireIdOverrides.set(`${hostSession.directory}\u0000${hostSession.sessionId}\u0000${coldId}`, liveId);
  }

  /**
   * Wire join key for a retry update (P4, field-loss plan). The SDK's
   * persistenceKey addresses the persisted assistant entry
   * ('assistant:<ts>:<provider>:<model>:<responseId>:<stopReason>'); the UI
   * joins omp.retry.ended notes by projected WIRE id (the TUI joins the same
   * update onto its component by persistenceKey — entryId is persistence
   * layer only). Resolve the timestamp segment to the live assistant
   * message, derive its wire id (cold form, bridged to the live streaming id
   * via wireIdOverrides), then fall back to the most recent settled
   * assistant wire id (the TUI's FIFO analog). The raw key is the last
   * resort so the payload stays joinable-shaped even when nothing matches.
   */
  #retryWireIdFor(hostSession: HostSession, update: { entryId?: string; persistenceKey?: string }): string {
    const key = update.persistenceKey ?? update.entryId ?? '';
    const timestamp = Number.parseInt(key.split(':')[1] ?? '', 10);
    const messages = hostSession.agentSession?.messages ?? [];
    const isAssistant = (m: AgentSession['messages'][number]): m is AgentSession['messages'][number] & { role: 'assistant' } =>
      m?.role === 'assistant';
    const match = Number.isFinite(timestamp)
      ? [...messages].filter(isAssistant).reverse().find((m) => m.timestamp === timestamp)
      : undefined;
    if (match) {
      const seed = textOfContent(match.content)
        || (Array.isArray(match.content) && match.content[0]?.type === 'toolCall'
          ? (match.content[0].name ?? '')
          : '');
      const coldId = wireMessageId('assistant', match.timestamp, seed);
      return this.wireIdOverrides.get(
        `${hostSession.directory}\u0000${hostSession.sessionId}\u0000${coldId}`,
      ) ?? coldId;
    }
    return hostSession.lastAssistantWireId ?? key;
  }

  #resolveModel(selector: { providerID?: string; modelID?: string } | undefined) {
    if (!selector) return undefined;
    const available = this.#sdkModels();
    const wanted = `${selector.providerID}/${selector.modelID}`;
    return available.find((model) => `${model.provider}/${model.id}` === wanted) ?? available.find((model) => model.id === selector.modelID);
  }

  async #materialize(sessionId: string, directoryKey: string) {
    // Concurrent materialization dedup: a UI-lease attach racing the first
    // prompt (leases.acquire is fire-and-forget) used to build two
    // AgentSessions for one id — the loser never entered `sessions`, leaked
    // its event subscription, and extension UI initialized twice. Callers
    // now share one in-flight promise per session.
    const flightKey = `${directoryKey}\u0000${sessionId}`;
    const inFlight = this.#materializeInFlight.get(flightKey);
    if (inFlight) return inFlight;
    // SAFETY: #materializeNow resolves non-null once boot succeeded; the
    // map type admits null only for symmetry with #materialize callers.
    const run = this.#materializeNow(sessionId, directoryKey).finally(() => {
      this.#materializeInFlight.delete(flightKey);
    });
    // SAFETY: #materializeNow resolves a live HostSession once boot has
    // succeeded; the map type only admits null for external symmetry.
    this.#materializeInFlight.set(flightKey, run as Promise<HostSession>);
    return run;
  }

  async #materializeNow(sessionId: string, directoryKey: string) {
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
    const model = this.#resolveModel(meta?.model ? splitModelSelector(meta.model) : undefined);
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
      authStorage: this.authStorage ?? undefined,
      modelRegistry: this.modelRegistry ?? undefined,
      // Per-directory keyed Settings injection (spec 06 §5.1, master R6):
      // the session consumes this directory's global+project layering.
      // Absent store (degraded boot) falls back to the SDK singleton.
      ...(this.settingsStore ? { settings: await this.settingsStore.settingsFor(directoryKey) } : {}),
      // One registry per session: the SDK's global registry admits a single
      // "Main" agent per process generation, and omp-host embeds several
      // concurrent top-level sessions. The instance is retained on the
      // host session for the agent-runs aggregator (spec 04 §5.5).
      agentRegistry,
      // R13: hasUI authority is the per-session UI lease, never the
      // capability. No lease at creation → fail-closed for approval tools.
      hasUI: this.dialogs.hasUISnapshotFor(directoryKey, sessionId).hasUI,
      // R7/R8: local:// resolution stays session-pinned to THIS session's
      // artifacts dir (TUI parity, spec 04 §5.2.3); zero global mutation.
      localProtocolOptions: createLocalProtocolOptions(sessionId, directoryKey, () =>
        manager.getArtifactsDir(),
      ),
      ...(model ? { model } : {}),
      // Persona overlay (02 §5.1 D-B2): constructor-time systemPrompt and
      // toolset come from the persona resource; the deleted build/plan
      // agent pair and the planYolo mapping never reach createAgentSession
      // (plan mode is a session mode driven by the mode endpoints, §5.8).
      ...(persona?.systemPrompt ? { systemPrompt: persona.systemPrompt } : {}),
      ...(Array.isArray(persona?.tools) && persona.tools.length > 0 ? { toolNames: persona.tools } : {})
    });
    const hostSession: HostSession = existing ?? {
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
      // Re-read the map row: `existing` was narrowed by the early return above.
      syncedEntryKeys: this.sessions.get(sessionId)?.syncedEntryKeys ?? new Set(),
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
      appliedPlugins: null
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
    // Registry events drive the agent-runs aggregator (spec 04 §5.5): without
    // this wiring the snapshot stays at revision 0 forever — rows never appear
    // and omp.agents.updated never publishes, so every UI consumer (work-status
    // rows, header badge, transcript row resolution) sees an empty world even
    // while subagents run. Coalescing lives inside the aggregator (notify
    // schedules one flush per directory); refresh() is the rebuild step.
    hostSession.unsubscribeAgentRegistry = agentRegistry.onChange(() => {
      this.uriDomain?.aggregator.refresh();
    });
    this.uriDomain?.aggregator.refresh();
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
        timeUpdated: Date.now()
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
  async #snapshotAppliedPlugins(directoryKey: string) {
    try {
      const { discoverExtensionPaths } = await import('@oh-my-pi/pi-coding-agent/extensibility/extensions');
      const { getEnabledPlugins } = await import('@oh-my-pi/pi-coding-agent/extensibility/plugins');
      const directory = directoryKey ?? process.cwd();
      const [extensionPaths, plugins] = await Promise.all([discoverExtensionPaths([], directory), getEnabledPlugins(directory)]);
      return {
        appliedAt: Date.now(),
        extensionPaths: extensionPaths.map((item) => path.resolve(item)),
        pluginNames: plugins.map((plugin) => plugin.name)
      };
    } catch (error) {
      console.warn('[omp-host] applied-plugins snapshot failed:', errorText(error));
      return null;
    }
  }

  /** Live per-session plugin application snapshots (plugins.v1 projection). */
  appliedPluginsSnapshots(): Array<{ sessionId: string; directory: string } & AppliedPluginsSnapshot> {
    return [...this.sessions.values()]
      .filter((hostSession): hostSession is HostSession & { appliedPlugins: AppliedPluginsSnapshot } =>
        Boolean(hostSession.agentSession && hostSession.appliedPlugins))
      .map((hostSession) => ({
        sessionId: hostSession.sessionId,
        directory: hostSession.directory,
        ...hostSession.appliedPlugins
      }));
  }

  /**
   * Hot-reload plugin state for live sessions in a directory (plugins.v1):
   * mirrors omp's `/reload-plugins` — invalidate the process-global discovery
   * caches, republish task/agent definitions, and refresh skills + slash
   * commands on every live session of that directory. TS extension module
   * bindings stay frozen (sessions rebind at next materialization).
   */
  async reloadAppliedPlugins(directory: string | null, sessionId: string | null = null) {
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
      console.warn('[omp-host] reload cache invalidation failed:', errorText(error));
    }
    try {
      const { refreshAgentDiscovery } = await import('@oh-my-pi/pi-coding-agent/task');
      await refreshAgentDiscovery(directoryKey);
    } catch (error) {
      console.warn('[omp-host] reload agent discovery refresh failed:', errorText(error));
    }
    let sessionsRefreshed = 0;
    for (const hostSession of this.sessions.values()) {
      if (hostSession.directory !== directoryKey || !hostSession.agentSession) continue;
      if (sessionId && hostSession.sessionId !== sessionId) continue;
      try {
        await hostSession.agentSession.refreshSkills?.();
        sessionsRefreshed += 1;
      } catch (error) {
        console.warn('[omp-host] reload skills refresh failed:', hostSession.sessionId, errorText(error));
      }
    }
    return { sessionsRefreshed };
  }
  /**
   * omp-native publish helper (spec 05 §5.2.1 envelope; master D6-R1 single
   * channel). Payload never carries directory/sessionID.
   */
  #ompPublish<P extends object>(hostSession: HostSession, type: string, payload: P | null | undefined, { durable }: { durable?: boolean } = {}) {
    return this.ompBus.publish(type, payload, {
      directory: hostSession.directory,
      sessionID: hostSession.sessionId,
      durable: Boolean(durable)
    });
  }

  /**
   * Project + emit one live custom/hook message on both tracks (spec 05
   * §5.1 row 9). Returns the projected wire message id. `display:false`
   * messages emit only the omp event (UI won't build a card; cold projection
   * drops them too — double guard, 05 §5.8.2 T3).
   */
  #emitCustomLive(hostSession: HostSession, message: CustomMessage | HookMessage) {
    const { sessionId, directory } = hostSession;
    const projected = projectCustomMessage(message, {
      sessionID: sessionId,
      agent: wireAgentFor(hostSession.currentPersona),
      parentID: hostSession.lastUserWireId || undefined
    });
    const text = textOfContent(message.content);
    if (message.display !== false) {
      this.bus.emit('message.updated', { sessionID: sessionId, info: projected.info }, directory);
      for (const part of projected.parts) {
        this.bus.emit('message.part.updated', { sessionID: sessionId, part, time: Date.now() }, directory);
      }
    }
    this.#ompPublish(
      hostSession,
      'omp.custom.appended',
      {
        message: {
          wireMessageID: projected.info.id,
          customType: message.customType ?? '',
          attribution: message.attribution,
          timestamp: message.timestamp,
          text,
          ...(message.details !== undefined ? { details: message.details } : {}),
          display: message.display !== false
        }
      },
      { durable: true }
    );
    hostSession.syncedEntryKeys?.add(`${message.role}:${message.customType ?? ''}:${message.timestamp}`);
    return projected.info.id;
  }

  /**
   * Tail-sync: project transcript roles that have no dedicated SDK event
   * (custom injected out-of-band, compaction/branch dividers) so they appear
   * live without a refetch (spec 05 §5.5). Idempotent per (role,type,ts).
   * @returns {{ projected: Array<{wireId: string, role: string}>, lastCompactionId: string | null }}
   */
  #tailSyncTranscript(hostSession: HostSession) {
    const session = hostSession.agentSession;
    const out: TailSyncTail = { projected: [], lastCompactionId: null };
    if (!session?.messages) return out;
    const messages = session.messages;
    const pending = [];
    for (let i = messages.length - 1; i >= 0; i -= 1) {
      const message = messages[i];
      if (!message || typeof message !== 'object') continue;
      const role = message.role;
      if (role !== 'custom' && role !== 'hookMessage' && role !== 'compactionSummary' && role !== 'branchSummary' && role !== 'developer') continue;
      // SAFETY: only custom/hook messages carry customType; the read is a
      // presence probe keyed into syncedEntryKeys.
      const customTyped = message as { customType?: string; timestamp?: number };
      const key = `${role}:${customTyped.customType ?? ''}:${message.timestamp}`;
      if (hostSession.syncedEntryKeys?.has(key)) break;
      pending.push(message);
    }
    pending.reverse();
    for (const message of pending) {
      // SAFETY: same presence-probe read as the scan pass above.
      const customTyped = message as { customType?: string; timestamp?: number };
      const key = `${message.role}:${customTyped.customType ?? ''}:${message.timestamp}`;
      hostSession.syncedEntryKeys?.add(key);
      if (message.role === 'developer') {
        if (!textOfContent(message.content).trim()) continue;
        const projected = projectDeveloperMessage(message, {
          sessionID: hostSession.sessionId,
          agent: wireAgentFor(hostSession.currentPersona),
          parentID: hostSession.lastUserWireId || undefined
        });
        this.bus.emit('message.updated', { sessionID: hostSession.sessionId, info: projected.info }, hostSession.directory);
        for (const part of projected.parts) {
          this.bus.emit('message.part.updated', { sessionID: hostSession.sessionId, part, time: Date.now() }, hostSession.directory);
        }
        out.projected.push({ wireId: projected.info.id, role: message.role });
        if (message.attribution === 'user') {
          hostSession.lastUserWireId = projected.info.id;
        }
      } else if (message.role === 'compactionSummary' || message.role === 'branchSummary') {
        const projected = projectDividerMessage(message, {
          sessionID: hostSession.sessionId,
          agent: wireAgentFor(hostSession.currentPersona),
          parentID: hostSession.lastUserWireId || undefined
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
  #handleEngineEvent(hostSession: HostSession, event: AgentSessionEvent) {
    const { sessionId, directory } = hostSession;
    const session = hostSession.agentSession;
    if (!session) return;
    switch (event.type) {
      case 'message_start': {
        if (event.message?.role === 'user') {
          const pending = hostSession.pendingUserWireId;
          hostSession.pendingUserWireId = null;
          if (pending) {
            const canonicalId = wireMessageId('user', event.message.timestamp, textOfContent(event.message.content));
            this.wireIdOverrides.set(`${directory}\u0000${sessionId}\u0000${canonicalId}`, pending);
          }
          return;
        }
        if (event.message?.role === 'developer') {
          // Synthetic prompt (prompt(synthetic:true) yields a developer-role
          // message, agent-session.ts:5597): project immediately and occupy
          // the user turn slot so the following assistant message anchors to
          // it. Mark synced so the tail-sync pass never re-emits it.
          const projected = projectDeveloperMessage(event.message, {
            sessionID: sessionId,
            agent: wireAgentFor(hostSession.currentPersona),
            parentID: hostSession.lastUserWireId || undefined
          });
          this.bus.emit('message.updated', { sessionID: sessionId, info: projected.info }, directory);
          for (const part of projected.parts) {
            this.bus.emit('message.part.updated', { sessionID: sessionId, part, time: Date.now() }, directory);
          }
          hostSession.syncedEntryKeys?.add(`developer::${event.message.timestamp}`);
          if (event.message.attribution === 'user') {
            hostSession.lastUserWireId = projected.info.id;
          }
          return;
        }
        if (event.message?.role !== 'assistant') return;
        hostSession.projector = new StreamProjector({
          sessionID: sessionId,
          directory,
          agent: wireAgentFor(hostSession.currentPersona),
          emit: (type, properties, dir) => this.bus.emit(type, properties, dir)
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
        const finished = hostSession.projector.finishAssistant(event.message, hostSession.turnToolResults ?? new Map());
        this.#bridgeAssistantWireId(hostSession, event.message);
        if (finished?.id) {
          hostSession.lastAssistantWireId = finished.id;
          const usage = event.message.usage ?? {};
          this.#ompPublish(
            hostSession,
            'omp.usage.turn',
            {
              messageID: finished.id,
              usage,
              ...(event.message.ttft !== undefined ? { ttftMs: event.message.ttft } : {}),
              ...(event.message.duration !== undefined ? { durationMs: event.message.duration } : {}),
              timestamp: event.message.timestamp ?? Date.now()
            },
            { durable: true }
          );
        }
        return;
      }
      case 'tool_execution_start': {
        hostSession.projector?.toolStarted(event.toolCallId, event.toolName, event.args, {
          ...(event.intent ? { title: event.intent } : {})
        });
        return;
      }
      case 'tool_execution_update': {
        // Partial results (05 §5.6): running-state append; never terminal —
        // tool_execution_end owns completion. The task tool's partial details
        // carry the per-subagent AgentProgress snapshot the transcript renders
        // live; other tools stay text/asyncState-only until a consumer needs
        // their partial details on the wire.
        const partial = typeof event.partialResult === 'string' ? undefined : event.partialResult;
        hostSession.projector?.toolPartial(event.toolCallId, {
          text: typeof event.partialResult === 'string' ? event.partialResult : (event.partialResult?.text ?? event.partialResult?.output),
          asyncState: event.partialResult?.details?.async?.state,
          ...(event.toolName === 'task' && partial?.details !== undefined ? { details: partial.details } : {}),
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
          error: event.isError ? text || 'Tool error' : undefined,
          ...(details ? { metadata: { details } } : {})
        });
        const results = hostSession.turnToolResults ?? new Map();
        results.set(event.toolCallId, {
          content,
          ...(details ? { details } : {}),
          isError: Boolean(event.isError),
          timestamp: Date.now()
        });
        hostSession.turnToolResults = results;
        // TUI parity (event-controller.ts:1656-1660): the todo tool result's
        // details.phases is the authoritative full list. todo_reminder's
        // payload carries incomplete items only (todo-tracker.ts:269), so
        // without this mapping the todo panel never sees todo writes and a
        // reminder drops completed items from it.
        // SAFETY: boundary cast — the todo tool's details is the SDK's
        // { phases: TodoPhase[] } marker (tools/todo.ts result details);
        // isTodoPhase re-narrows every element before the projection reads it.
        const todoPhases = (details as { phases?: unknown } | undefined)?.phases;
        if (
          event.toolName === 'todo'
          && !event.isError
          && Array.isArray(todoPhases)
          && todoPhases.every(isTodoPhase)
        ) {
          const todos = todoPhases.flatMap((phase) => phase.tasks.map((task) => ({
            content: task.content,
            status: task.status,
            priority: 'medium',
            // ch10 wire 重合面: the SDK task carries the blocker note; the
            // reminder projection is transient (notice.raised), so this
            // mapping is the only carrier that puts it on the wire.
            ...(typeof task.blocker === 'string' && task.blocker ? { blocker: task.blocker } : {}),
          })));
          this.bus.emit('todo.updated', { sessionID: sessionId, todos }, directory);
        }
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
            session.getLastAssistantMessage() ?? {
              content: [],
              timestamp: Date.now(),
              usage: {},
              model: ''
            },
            hostSession.turnToolResults ?? new Map()
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
        // SAFETY: SDK todo rows are the wire todo shape (todo tool contract).
        const todos = ((event.todos ?? []) as Array<{ content?: string; status?: string; blocker?: string; priority?: string }>).map((todo) => ({
          content: todo.content ?? '',
          status: todo.status ?? 'pending',
          priority: todo.priority ?? 'medium',
          ...(typeof todo.blocker === 'string' && todo.blocker ? { blocker: todo.blocker } : {}),
        }));
        // Transient reminder surface only (TUI TodoReminderComponent parity:
        // event-controller.ts presents a reminder, never rewrites the todo
        // panel). The event payload lists incomplete items only
        // (todo-tracker.ts:269), so emitting wire todo.updated here would
        // replace the panel's authoritative full list from the todo tool
        // result mapping (tool_execution_end) and drop completed items.
        this.#ompPublish(
          hostSession,
          'omp.notice.raised',
          {
            level: 'info',
            message: `Unfinished todos (${event.attempt ?? 1}/${event.maxAttempts ?? 1}): ${todos
              .map((todo: { content?: string }) => todo.content)
              .filter(Boolean)
              .join('; ')}`
          },
          { durable: false }
        );
        return;
      }
      case 'todo_auto_clear': {
        this.bus.emit('todo.updated', { sessionID: sessionId, todos: [] }, directory);
        return;
      }
      case 'notice': {
        if (event.level === 'error') console.error('[omp-host]', event.message);
        this.#ompPublish(
          hostSession,
          'omp.notice.raised',
          {
            level: event.level,
            message: event.message,
            ...(event.source ? { source: event.source } : {})
          },
          { durable: false }
        );
        return;
      }
      case 'auto_compaction_start': {
        this.#ompPublish(
          hostSession,
          'omp.compaction.started',
          {
            reason: event.reason,
            action: event.action
          },
          { durable: false }
        );
        return;
      }
      case 'auto_compaction_end': {
        const sync = this.#tailSyncTranscript(hostSession);
        this.#ompPublish(
          hostSession,
          'omp.compaction.ended',
          {
            action: event.action,
            aborted: Boolean(event.aborted),
            willRetry: Boolean(event.willRetry),
            ...(event.skipped !== undefined ? { skipped: event.skipped } : {}),
            ...(event.errorMessage ? { errorMessage: event.errorMessage } : {}),
            ...(event.result?.tokensBefore !== undefined ? { tokensBefore: event.result.tokensBefore } : {}),
            ...(sync.lastCompactionId ? { wireMessageID: sync.lastCompactionId } : {})
          },
          { durable: false }
        );
        return;
      }
      case 'auto_retry_start': {
        // P1 (05 §5.3.2): status + superseded overlay only. Zero wire
        // mutation — message.part.removed stays P2-gated (master R14).
        this.bus.emit(
          'session.status',
          {
            sessionID: sessionId,
            status: {
              type: 'retry',
              attempt: event.attempt,
              message: event.errorMessage,
              next: Date.now() + event.delayMs
            }
          },
          directory
        );
        this.#ompPublish(
          hostSession,
          'omp.retry.started',
          {
            attempt: event.attempt,
            maxAttempts: event.maxAttempts,
            delayMs: event.delayMs,
            errorMessage: event.errorMessage,
            ...(hostSession.lastAssistantWireId ? { supersededMessageID: hostSession.lastAssistantWireId } : {})
          },
          { durable: false }
        );
        return;
      }
      case 'auto_retry_end': {
        this.bus.emit('session.status', { sessionID: sessionId, status: { type: 'busy' } }, directory);
        this.#ompPublish(hostSession, 'omp.retry.ended', {
          success: Boolean(event.success),
          attempt: event.attempt,
          ...(event.finalError ? { finalError: event.finalError } : {}),
          // SAFETY: SDK retry-error updates carry the persisted entry ids.
          retryErrors: ((event.retryErrors ?? []) as Array<{ entryId?: string; persistenceKey?: string; note?: string; retryRecovery?: unknown }>).map((update) => ({
            messageID: this.#retryWireIdFor(hostSession, update),
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
        this.#ompPublish(
          hostSession,
          'omp.fallback.applied',
          {
            from: event.from,
            to: event.to,
            role: event.role
          },
          { durable: true }
        );
        return;
      }
      case 'retry_fallback_succeeded': {
        // Success happened on the fallback model; no registry writeback.
        this.#ompPublish(
          hostSession,
          'omp.fallback.succeeded',
          {
            model: event.model,
            role: event.role
          },
          { durable: true }
        );
        return;
      }
      case 'model_changed': {
        const selector = modelSelector(session.model);
        this.registry.update(directory, sessionId, {
          ...(selector ? { model: selector } : {})
        });
        const info = this.#wireSessionFromLive(hostSession);
        this.bus.emit('session.updated', { sessionID: sessionId, info }, directory);
        this.#ompPublish(hostSession, 'omp.model.changed', {
          // Model omitted when unset: the upstream event is payload-less and
          // the TUI re-reads session.model (invalidate + refetch semantics);
          // a JSON null would fail the UI schema and drop the whole frame.
          ...(session.model
            ? { model: { provider: session.model.provider, id: session.model.id } }
            : {}),
          ...(session.thinkingLevel !== undefined ? { thinkingLevel: session.thinkingLevel } : {}),
        }, { durable: true });
        return;
      }
      case 'ttsr_triggered': {
        this.#ompPublish(
          hostSession,
          'omp.ttsr.triggered',
          {
            // SAFETY: ttsr rules are name-keyed config rows.
            rules: ((event.rules ?? []) as Array<{ name?: string }>).map((rule) => ({ name: rule.name }))
          },
          { durable: false }
        );
        return;
      }
      case 'irc_message': {
        this.#emitCustomLive(hostSession, event.message);
        return;
      }
      case 'thinking_level_changed': {
        // thinkingLevel omitted on clear (SDK contract: ThinkingLevel |
        // undefined; the TUI falls back to Off/inherited) — never JSON null.
        this.#ompPublish(hostSession, 'omp.thinking.changed', {
          ...(event.thinkingLevel !== undefined ? { thinkingLevel: event.thinkingLevel } : {}),
          ...(event.configured !== undefined ? { configured: event.configured } : {}),
          ...(event.resolved !== undefined ? { resolved: event.resolved } : {}),
        }, { durable: true });
        return;
      }
      case 'goal_updated': {
        this.modesDomain?.trackerFor(sessionId, directory)?.applyGoalUpdate?.(event.goal, event.state);
        this.#ompPublish(
          hostSession,
          'omp.goal.updated',
          {
            goal: event.goal ?? null,
            ...(event.state !== undefined ? { state: event.state } : {})
          },
          { durable: true }
        );
        return;
      }
      default: {
        // Defense-in-depth only (05 §5.1): the manifest + CI guard own the
        // real coverage check. Never silently swallow an unknown member.
        // SAFETY: exhaustive switch leaves `never`; the probe only reads .type.
        const unknownEvent = event as { type?: string } | null | undefined;
        console.error(`[omp-host] unhandled AgentSessionEvent type: ${unknownEvent?.type}`);
        this.unknownEventCounts = this.unknownEventCounts ?? new Map();
        this.unknownEventCounts.set(unknownEvent?.type ?? 'unknown', (this.unknownEventCounts.get(unknownEvent?.type ?? 'unknown') ?? 0) + 1);
        return;
      }
    }
  }

  async getSessionStatuses({ directory }: { directory?: string }) {
    await this.#boot();
    const AWAITING_ASYNC_TIMEOUT_MS = 10 * 60 * 1000;
    const now = Date.now();
    const statuses: Record<string, { type: 'busy' } | { type: 'idle' }> = {};
    for (const [id, live] of this.sessions) {
      if (live.directory !== normalizeDirectoryKey(directory)) continue;
      const stale = live.awaitingAsyncSince !== null && now - live.awaitingAsyncSince > AWAITING_ASYNC_TIMEOUT_MS;
      if (stale) live.awaitingAsyncSince = null;
      statuses[id] = live.agentSession?.isStreaming || live.awaitingAsyncSince !== null ? { type: 'busy' } : { type: 'idle' };
    }
    return statuses;
  }

  /** Structured customType inventory for the omp transcript read (05 §5.2.1). */
  async getCustomMessages({ sessionID, directory }: { sessionID: string; directory?: string }) {
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
        ...(message.details !== undefined ? { details: message.details } : {})
      });
    }
    return out;
  }

  /** Per-turn telemetry (05 §5.9): usage/ttft/duration per assistant message. */
  async getTelemetry({ sessionID, directory }: { sessionID: string; directory?: string }) {
    const context = await this.#transcriptContext(sessionID, directory);
    if (!context) return null;
    const directoryKey = normalizeDirectoryKey(directory);
    const wireIdFor = this.#wireIdResolver(directoryKey, sessionID);
    const out = [];
    for (const message of context.messages ?? []) {
      if (!message || message.role !== 'assistant') continue;
      const seed = textOfContent(message.content)
        || (Array.isArray(message.content) && message.content[0]?.type === 'toolCall'
          ? (message.content[0].name ?? '')
          : '');
      const baseId = wireMessageId('assistant', message.timestamp, seed);
      const overridden = wireIdFor?.(message);
      const usage: UsageInput = message.usage ?? {};
      out.push({
        messageID: overridden ?? baseId,
        timestamp: message.timestamp,
        input: usage.input ?? 0,
        output: usage.output ?? 0,
        cacheRead: usage.cacheRead ?? 0,
        cacheWrite: usage.cacheWrite ?? 0,
        ...(usage.reasoningTokens !== undefined ? { reasoningTokens: usage.reasoningTokens } : {}),
        totalTokens:
          (usage.input ?? 0) + (usage.output ?? 0) + (usage.cacheRead ?? 0) + (usage.cacheWrite ?? 0),
        ...(message.ttft !== undefined ? { ttftMs: message.ttft } : {}),
        ...(message.duration !== undefined ? { durationMs: message.duration } : {})
      });
    }
    return out;
  }

  /**
   * Structured session entries (05 §5.2.1): compaction dividers, branch
   * summaries, model/mode changes, ttsr injections, retry recovery notes.
   */
  async getEntries({ sessionID, directory, kinds }: { sessionID: string; directory?: string; kinds?: string[] }) {
    await this.#boot();
    const directoryKey = normalizeDirectoryKey(directory);
    const file = await this.#findSessionFile(sessionID, directoryKey);
    if (!file) return null;
    const wanted = new Set(
      String(kinds ?? '')
        .split(',')
        .map((kind) => kind.trim())
        .filter(Boolean)
    );
    const out = [];
    const manager = await SessionManager.open(file.path);
    try {
      for (const entry of manager.getEntries() ?? []) {
        const kind = entry.type === 'compaction' ? 'compaction' : entry.type === 'branch_summary' ? 'branch_summary' : entry.type === 'model_change' ? 'model_change' : entry.type === 'mode_change' ? 'mode_change' : entry.type === 'ttsr_injection' ? 'ttsr_injection' : null;
        if (!kind || (wanted.size > 0 && !wanted.has(kind))) continue;
        out.push({
          kind,
          id: entry.id,
          timestamp: Date.parse(entry.timestamp ?? '') || undefined,
          ...(entry.type === 'compaction'
            ? {
                summary: entry.summary,
                tokensBefore: entry.tokensBefore,
                ...(entry.warning ? { warning: entry.warning } : {})
              }
            : {}),
          ...(entry.type === 'branch_summary' ? { fromId: entry.fromId, summary: entry.summary } : {}),
          ...(entry.type === 'model_change' ? { model: entry.model, ...(entry.role ? { role: entry.role } : {}) } : {}),
          ...(entry.type === 'mode_change' ? { mode: entry.mode, ...(entry.data ? { data: entry.data } : {}) } : {}),
          ...(entry.type === 'ttsr_injection' ? { rules: entry.injectedRules } : {}),
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
        const seed = textOfContent(message.content)
          || (Array.isArray(message.content) && message.content[0]?.type === 'toolCall'
            ? (message.content[0].name ?? '')
            : '');
        const baseId = wireMessageId('assistant', message.timestamp, seed);
        out.push({
          kind: 'retry_recovery',
          messageID: wireIdFor?.(message) ?? baseId,
          timestamp: message.timestamp,
          retryRecovery: message.retryRecovery
        });
      }
    }
    return out;
  }

  /**
   * Chapter-14 read: one subagent run's transcript from its registry ref.
   * Live and parked refs keep sessionFile; historical disk rows have no ref
   * here and answer null (the route maps that to 404 `no-transcript`).
   */
  async getAgentRunTranscript({ sessionID, agentId, directory }: { sessionID: string; agentId: string; directory?: string }) {
    await this.#boot();
    const hostSession = this.sessions.get(sessionID);
    const runRef = hostSession?.agentRegistry.get(agentId);
    const sessionFile = runRef?.sessionFile;
    if (!hostSession || !runRef || !sessionFile) return null;
    const directoryKey = normalizeDirectoryKey(directory ?? hostSession.directory);
    const manager = await SessionManager.open(sessionFile);
    try {
      const context = manager.buildSessionContext({ transcript: true });
      const messages = projectConversation(context.messages ?? [], {
        sessionID: manager.getSessionId(),
        directory: directoryKey
      });
      return {
        sessionID,
        agentId,
        displayName: runRef.displayName ?? agentId,
        status: runRef.status,
        messages,
        ...(runRef.history?.outputPath ? { outputPath: runRef.history.outputPath } : {}),
      };
    } finally {
      await manager.close().catch(() => {});
    }
  }

  async #transcriptContext(sessionID: string, directory: string | null | undefined) {
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

  async prompt({
    sessionID,
    directory,
    text,
    model,
    agent,
    images,
    delivery,
    messageID,
  }: {
    sessionID: string;
    directory: string;
    text: string;
    model?: { providerID?: string; modelID?: string };
    agent?: string;
    images?: Array<{ data?: string; mimeType?: string }>;
    delivery?: string;
    messageID?: string;
  }): Promise<ProjectedMessage | null> {
    await this.#boot();
    const directoryKey = normalizeDirectoryKey(directory);
    const hostSession = await this.#materialize(sessionID, directoryKey);
    if (!hostSession) return null;
    const session = hostSession.agentSession;
    if (!session) return null;
    if (hostSession.extensionUiPromise) await hostSession.extensionUiPromise;
    hostSession.lastTouched = Date.now();

    // Model switching: resolve and apply when the requested selector differs.
    if (model && (model.providerID || model.modelID)) {
      const target = this.#resolveModel(model);
      if (target && modelSelector(target) !== modelSelector(session.model)) {
        await session.setModel(target).catch((error) => {
          console.error('[omp-host] model switch failed:', errorText(error));
        });
        this.registry.update(directoryKey, sessionID, {
          model: modelSelector(target)
        });
      }
    }

    const meta = (this.registry.get(directoryKey, sessionID) ?? undefined);
    // Persona switch (02 §5.1 D-B3, R2-M3): explicit session-level switch —
    // the wire `agent` parameter and registry meta normalize through
    // personaKeyFor, so the deleted build/plan values and unset all mean
    // "standard" and never trigger a rebuild. A switch to a persona that no
    // longer exists is rejected before any state changes: the session keeps
    // its current persona and the message is not dispatched.
    const nextPersona = personaKeyFor(agent ?? meta?.persona ?? meta?.agent);
    if (nextPersona !== 'standard' && !this.personas.has(nextPersona)) {
      throw new ModeDomainError(404, {
        error: 'persona-not-found',
        name: nextPersona
      });
    }
    if (nextPersona !== hostSession.currentPersona) {
      // The persona shapes the session's system prompt and toolset at
      // construction, so rebuild the AgentSession over the same transcript.
      this.#disposeSession(hostSession);
      this.registry.update(directoryKey, sessionID, {
        persona: nextPersona === 'standard' ? undefined : nextPersona,
        agent: undefined
      });
      const rebuilt = await this.#materialize(sessionID, directoryKey);
      if (!rebuilt) return null;
      return this.prompt({
        sessionID,
        directory: directoryKey,
        text,
        model,
        agent: nextPersona,
        images,
        delivery,
        messageID
      });
    }

    const content = [];
    if (typeof text === 'string' && text.length > 0) content.push({ type: 'text', text });
    for (const image of images ?? []) {
      content.push({
        type: 'image',
        data: image.data,
        mimeType: image.mimeType || 'image/png'
      });
    }
    const wire = projectUserMessage(
      {
        role: 'user',
        content: content.length === 1 && content[0].type === 'text' ? content[0].text : content,
        timestamp: Date.now()
      },
      {
        sessionID,
        agent: wireAgentFor(nextPersona),
        model: session.model,
        // Exact send-time snapshot: the effective level the turn runs with
        // (explicit pick, else the model's default) rides model.variant.
        thinkingLevel: this.#effectiveThinkingLevel(session),
        ...(typeof messageID === 'string' && messageID ? { wireId: messageID } : {})
      }
    );
    hostSession.pendingUserWireId = typeof messageID === 'string' && messageID ? messageID : null;
    hostSession.lastUserWireId = wire.info.id;
    this.bus.emit('message.updated', { sessionID, info: wire.info }, directoryKey);
    for (const part of wire.parts) {
      this.bus.emit('message.part.updated', { sessionID, part, time: Date.now() }, directoryKey);
    }

    if (!meta?.timeCreated) {
      this.registry.update(directoryKey, sessionID, {
        timeCreated: wire.info.time.created
      });
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

    const textOnly = content.length === 1 && content[0].type === 'text' ? (content[0].text ?? '') : (text ?? '');
    // SAFETY: filtered blocks are image parts; base64+mime is the wire form.
    const imageContents = content.filter((block) => block.type === 'image') as Array<{ type: 'image'; data: string; mimeType: string }>;
    // Dispatch mirrors the TUI input loop: every submission carries a
    // streaming behavior so a live turn never rejects the prompt. steer
    // injects into the running turn (the TUI's Enter-while-streaming);
    // when idle, and routing through prompt() rather than steer() keeps
    // "/" extension commands working mid-turn (steer() rejects them).
    const streamingBehavior = delivery === 'queue' ? 'followUp' : 'steer';
    // TUI/RPC parity (rpc-mode.ts tryRunRpcSkillCommand): a slash command
    // naming a skill runs as a skill-prompt custom message so the transcript
    // carries the invocation card; plain prompt() executes the command with
    // no card at all.
    if (imageContents.length === 0 && await this.#tryRunSkillCommand(hostSession, textOnly, streamingBehavior)) {
      return wire;
    }
    await session.prompt(textOnly, {
      images: imageContents,
      streamingBehavior
    });
    return wire;
  }

  /**
   * Mirror of rpc-mode's tryRunRpcSkillCommand: when the text is a slash
   * invocation of a known skill and skill commands are enabled, send the
   * skill-prompt custom message (display card, user attribution) instead of a
   * plain prompt. Returns false when the text is not a skill command so the
   * normal dispatch proceeds.
   */
  async #tryRunSkillCommand(hostSession: HostSession, text: string, streamingBehavior: "steer" | "followUp") {
    const session = hostSession.agentSession;
    if (!session?.skillsSettings?.enableSkillCommands) return false;
    const parsed = parseSkillInvocation(text);
    if (!parsed) return false;
    const skill = (session.skills ?? []).find((candidate) => candidate?.name === parsed.name);
    if (!skill) return false;
    const built = await buildSkillPromptMessage(skill, parsed.args, 'user');
    await session.promptCustomMessage(
      {
        customType: SKILL_PROMPT_MESSAGE_TYPE,
        content: built.message,
        display: true,
        details: built.details,
        attribution: 'user'
      },
      { streamingBehavior }
    );
    return true;
  }

  /**
   * Session-scoped model switch without sending a turn (spec 01 GAP-02/
   * GAP-04: prompts omit the model; changing it is an explicit setModel).
   * Same resolution + registry bookkeeping as the prompt-time switch.
   * GAP-06: when the target model matches the session's current model, this
   * degrades to a thinking-level-only change (`setThinkingLevel`) — the
   * in-session thinking slot applies through the same endpoint.
   */
  async setSessionModel({ sessionID, directory, model, thinkingLevel }: { sessionID: string; directory?: string; model?: { providerID?: string; modelID?: string }; thinkingLevel?: string }) {
    await this.#boot();
    const directoryKey = normalizeDirectoryKey(directory);
    if (!model || !(model.providerID || model.modelID)) {
      return { ok: false, error: 'model is required' };
    }
    const hostSession = await this.#materialize(sessionID, directoryKey);
    if (!hostSession) return { ok: false, error: 'session not found' };
    const session = hostSession.agentSession;
    if (!session) return { ok: false, error: 'session not found' };
    hostSession.lastTouched = Date.now();
    const target = this.#resolveModel(model);
    if (!target) return { ok: false, error: 'unknown model' };
    if (modelSelector(target) !== modelSelector(session.model)) {
      await session.setModel(target).catch((error) => {
        console.error('[omp-host] model switch failed:', errorText(error));
      });
      this.registry.update(directoryKey, sessionID, {
        model: modelSelector(target)
      });
    }
    if (thinkingLevel !== undefined && typeof session.setThinkingLevel === 'function') {
      // SDK contract: setThinkingLevel returns void (agent-session.d.ts:736)
      // — the change is observed through the thinking_level_changed event,
      // never a return value. 'inherit' is OMPChamber's wire sentinel for
      // clearing the explicit level; the SDK clears via undefined.
      try {
        // SAFETY: wire thinking levels are the SDK ThinkingLevel vocabulary
        // ('low'|'medium'|'high'); 'inherit' is the OMP clear sentinel.
        session.setThinkingLevel(thinkingLevel === 'inherit' ? undefined : (thinkingLevel as Parameters<NonNullable<AgentSession['setThinkingLevel']>>[0]));
      } catch (error) {
        console.error('[omp-host] thinking level switch failed:', errorText(error));
      }
    }
    return {
      ok: true,
      model: modelSelector(session.model) ?? modelSelector(target)
    };
  }

  async abort({ sessionID, directory }: { sessionID: string; directory?: string }) {
    await this.#boot();
    const live = this.sessions.get(sessionID);
    if (!live?.agentSession) return false;
    // AgentSession.abort() delivers the cancellation signal synchronously,
    // then awaits the full turn teardown (post-prompt drain + agent idle).
    // pi caps that drain on its dispose paths but not on abort, so a single
    // signal-blind tool call or never-settling post-prompt task parks the
    // await forever — this route then never answered, the stop request hung,
    // and the session stayed busy until a server restart. Bound the wait; a
    // healthy teardown settles well under a second.
    let timeoutTimer: ReturnType<typeof setTimeout> | undefined;
    const settled = await Promise.race([
      live.agentSession.abort({ reason: 'User aborted' }).then(
        () => true,
        (error) => {
          // The cancellation signal was still delivered; a rejected teardown
          // step must not break the stop contract — but leave a trace.
          console.warn('[omp-host] abort teardown rejected:', errorText(error));
          return true;
        }
      ),
      new Promise((resolve) => {
        timeoutTimer = setTimeout(resolve, this.abortTeardownTimeoutMs);
      }).then(() => false)
    ]);
    clearTimeout(timeoutTimer);
    if (settled) {
      // A settled abort with nothing streaming means the busy state was the
      // engine-level awaiting-async limbo: the turn ended with isTerminal
      // false (async delivery was supposed to resume it) and the resume
      // never came, so pi is idle while the session stays busy — Stop looked
      // dead while a new steer "magically" healed it (agent_start clears
      // awaitingAsyncSince). Stop must be authoritative instead: drop the
      // limbo and settle clients, mirroring the terminal agent_end path. A
      // genuine async resume starts with agent_start, which re-raises busy.
      // Optional chaining: a concurrent delete/dispose may have nulled
      // agentSession while the race was pending.
      if (!live.agentSession?.isStreaming && live.awaitingAsyncSince !== null) {
        live.awaitingAsyncSince = null;
        this.bus.emit('session.idle', { sessionID }, live.directory);
      }
      return true;
    }
    // The teardown is stuck and the session is bricked with it (pi leaves
    // #abortInProgress set and ignores further input). Force-dispose: pi's
    // dispose caps its own drains, the next prompt() rebuilds a live session
    // from the persisted transcript, and the emitted session.idle unsticks
    // every client immediately (module invariant: events carry the session's
    // own directory).
    console.warn(
      `[omp-host] abort teardown did not settle within ${this.abortTeardownTimeoutMs}ms; force-disposing session ${sessionID}`
    );
    this.#disposeSession(live);
    this.sessions.delete(sessionID);
    this.bus.emit('session.idle', { sessionID }, live.directory);
    return true;
  }

  async summarize({ sessionID, directory }: { sessionID: string; directory?: string }) {
    await this.#boot();
    const directoryKey = normalizeDirectoryKey(directory);
    const hostSession = await this.#materialize(sessionID, directoryKey);
    if (!hostSession?.agentSession) return false;
    await hostSession.agentSession.compact();
    return true;
  }

  async fork({ sessionID, directory, messageID }: { sessionID: string; directory: string; messageID?: string }) {
    await this.#boot();
    const directoryKey = normalizeDirectoryKey(directory);
    const file = await this.#findSessionFile(sessionID, directoryKey);
    if (!file) return null;
    const forked = await SessionManager.forkFrom(file.path, directoryKey, this.#sessionDirFor(directoryKey));
    const forkId = forked.getSessionId();
    // Wire contract: an optional messageID bounds the fork. TUI /branch
    // semantics — the selected user message and everything after it leave
    // the active path (the caller restores its text into the composer);
    // without one the fork keeps the whole transcript (TUI /fork semantics).
    if (messageID) {
      const entryId = resolveWireIdToEntryId(forked.getEntries?.() ?? [], messageID, {
        wireIdFor: this.#wireIdResolver(directoryKey, sessionID)
      });
      // Compat: native entry ids pass through unchanged (the same fallback
      // revert uses) before giving up and forking at the leaf.
      const boundary = forked.getEntry?.(entryId ?? messageID);
      if (!boundary) {
        console.warn(`[omp-host] fork boundary ${messageID} not found; forking at the leaf`);
      } else {
        // branch()/resetLeaf() move the leaf in memory only — the loader
        // rebuilds the active path from the last physical entry — so an
        // invisible marker entry appended at the new leaf makes the rewind
        // durable. Empty custom entries project to nothing (dropped by the
        // projection's empty-content rule), so the fork's transcript starts
        // clean at the boundary.
        const parentId = boundary.parentId ?? null;
        if (parentId) forked.branch(parentId);
        else forked.resetLeaf();
        forked.appendCustomEntry('ompchamber.forkBoundary', {
          from: sessionID,
          at: messageID
        });
      }
    }
    const now = Date.now();
    const meta = (this.registry.get(directoryKey, sessionID) ?? undefined);
    this.registry.update(directoryKey, forkId, {
      // Fork lineage for the session-tree projection (§5.4). NOT wire
      // `parentID` — that field is subagent parentage, and the shared UI
      // flips sessions carrying it into a read-only subagent composer.
      forkParentID: sessionID,
      title: meta?.title ? `${meta.title} (fork)` : 'Forked session',
      timeCreated: now,
      timeUpdated: now,
      ...(meta?.persona ? { persona: meta.persona } : {}),
      ...(meta?.agent ? { agent: meta.agent } : {}),
      ...(meta?.model ? { model: meta.model } : {})
    });
    await forked.close();
    const session = this.#wireSession(
      {
        id: forkId,
        cwd: directoryKey,
        created: new Date(now),
        modified: new Date(now)
      },
      directoryKey,
      (this.registry.get(directoryKey, forkId) ?? undefined)
    );
    this.bus.emit('session.created', { sessionID: forkId, info: session }, directoryKey);
    return session;
  }

  /**
   * Revert: move the transcript's active branch so `messageID` becomes the
   * last retained message. Records the previous leaf for unrevert.
   */
  async revert({ sessionID, directory, messageID }: { sessionID: string; directory?: string; messageID: string }) {
    await this.#boot();
    const directoryKey = normalizeDirectoryKey(directory);
    const hostSession = await this.#materialize(sessionID, directoryKey);
    if (!hostSession?.agentSession) return null;
    const manager = hostSession.agentSession.sessionManager;
    // The UI sends the wire message id it read from GET messages; branch()
    // wants the engine entry id. Resolve through the same projection the UI
    // saw (native ids pass through unchanged for compat).
    const entryId = resolveWireIdToEntryId(manager.getEntries?.() ?? [], messageID, {
      wireIdFor: this.#wireIdResolver(directoryKey, sessionID)
    });
    manager.branch(entryId ?? messageID);
    const previousLeaf = manager.getLeafId() ?? messageID;
    this.registry.update(directoryKey, sessionID, {
      revert: { messageID, previousLeaf },
      timeUpdated: Date.now()
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
  liveCommandsFor(directory: string | null) {
    const directoryKey = normalizeDirectoryKey(directory);
    for (const hostSession of this.sessions.values()) {
      if (hostSession.directory !== directoryKey) continue;
      const session = hostSession.agentSession;
      if (!session?.extensionRunner) continue;
      try {
        const commands = getSessionSlashCommands(session) ?? [];
        return Promise.resolve(
          commands.map((command) => ({
            name: command.name,
            ...(typeof command.description === 'string' && command.description ? { description: command.description } : {}),
            source: command.source ?? 'extension'
          }))
        );
      } catch {
        return Promise.resolve([]);
      }
    }
    return Promise.resolve([]);
  }

  async unrevert({ sessionID, directory }: { sessionID: string; directory?: string }) {
    await this.#boot();
    const directoryKey = normalizeDirectoryKey(directory);
    const hostSession = await this.#materialize(sessionID, directoryKey);
    if (!hostSession) return null;
    const meta = (this.registry.get(directoryKey, sessionID) ?? undefined);
    const previousLeaf = meta?.revert?.previousLeaf;
    if (!hostSession.agentSession) return null;
    const manager = hostSession.agentSession.sessionManager;
    if (previousLeaf) {
      manager.branch(previousLeaf);
    } else {
      manager.resetLeaf();
    }
    this.registry.update(directoryKey, sessionID, {
      revert: undefined,
      timeUpdated: Date.now()
    });
    const session = this.#wireSessionFromLive(hostSession);
    this.bus.emit('session.updated', { sessionID, info: session }, directoryKey);
    return session;
  }

  async getTodos({ sessionID, directory }: { sessionID: string; directory?: string }) {
    await this.#boot();
    const directoryKey = normalizeDirectoryKey(directory);
    const hostSession = this.sessions.get(sessionID);
    if (!hostSession?.agentSession) return [];
    const phases = hostSession.agentSession.getTodoPhases();
    const latest = phases[phases.length - 1];
    // SDK TodoPhase carries `tasks`; `items`/`todos` were pre-18 field names
    // that made this read return [] unconditionally. `priority` is not a
    // TodoItem field in the SDK but legacy transcripts may still carry it.
    const todos: Array<{ content: string; status: string; priority?: string }> = latest?.tasks ?? [];
    return todos.map((todo) => ({
      content: todo.content ?? '',
      status: todo.status ?? 'pending',
      priority: todo.priority ?? 'medium'
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

  /** SDK model rows (registry-backed); the typed read view for projections. */
  availableModels(): RegistryModel[] {
    // SAFETY: SDK Model is a structural superset of RegistryModel
    // (nullable size fields are admitted on the read view).
    return this.#sdkModels() as RegistryModel[];
  }

  #sdkModels() {
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

  projectIdFor(directoryKey: string) {
    return this.#projectId(normalizeDirectoryKey(directoryKey));
  }

  /**
   * Move a session to another project directory: relocate the transcript via
   * omp's SessionManager.moveTo and migrate the sidecar metadata.
   */
  async moveSession({ sessionID, destination }: { sessionID: string; destination: string }) {
    await this.#boot();
    const toKey = normalizeDirectoryKey(destination);
    const live = this.sessions.get(sessionID);
    const fromKey = live?.directory ?? (await this.#locateDirectory(sessionID));
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

  async #locateDirectory(sessionID: string) {
    for (const [id, live] of this.sessions) {
      if (id === sessionID) return live.directory;
    }
    const byDirectory = await this.listAllSessions({});
    for (const [directory, list] of byDirectory) {
      if (list.some((session: { id: string }) => session.id === sessionID)) return directory;
    }
    return null;
  }
}
