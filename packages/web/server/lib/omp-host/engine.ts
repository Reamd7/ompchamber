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
import { LiveSessionRegistry, SessionBusyError, sessionKey, type LiveRecord } from './live-registry.ts';
import { withColdManager } from './cold-reader.ts';
import { readSessionEventRows, readSessionScalars, readTranscriptMessagePage } from './cold-transcript-page.ts';
import { classifyExternalChange, fileSignature, tailEntryIdOf, type FileSignature } from './dual-write.ts';
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
// Idle-session sweep period (plan D2: 60s; the count gate is gone — quantity
// is not a memory bound, idle lifetime is).
const IDLE_SWEEP_INTERVAL_MS = 60_000;
// Drain budget for a single eviction's agentSession.dispose (SDK default is
// 5s; allow headroom for flush) and the global shutdown deadline for all
// disposals combined (plan §3.4).
const EVICT_DRAIN_TIMEOUT_MS = 8_000;
const SHUTDOWN_DISPOSE_DEADLINE_MS = 10_000;

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
  /** Registry key (`directory\0sessionID`) — lifecycle lookups go through it. */
  key: string;
  sessionId: string;
  directory: string;
  agentSession: SdkAgentSession | null;
  sdkResult: Pick<CreateAgentSessionResult, 'setToolUIContext'>;

  currentPersona: string;
  projector: StreamProjector | null;
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
  /** onSessionNameChanged unsubscribe (plan §3.3.2 — never leak the callback). */
  nameUnsubscribe?: () => void;
  /** Transcript identity at materialize; dual-write detection (plan §8). */
  fileSignature: FileSignature | null;
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
  #live: LiveSessionRegistry<HostSession>;
  /** True once shutdown started: new writers are rejected (plan §3.4). */
  #closing = false;
  /** Test seams: monotonic TTL clock + agent factory (defaults: real SDK). */
  #now: () => number;
  #createAgentSessionImpl: typeof createAgentSession;
  /** Bound on unknown-event diagnostic keys (plan §6: no unbounded key set). */
  static #UNKNOWN_EVENT_KEYS_MAX = 64;

  authStorage: AuthStorage | null;
  modelRegistry: ModelRegistry | null;
  registry: SessionMetaRegistry;
  bus: WireEventBus;
  /** omp-native event channel (spec 05 §5.2, master D6-R1 single authority). */
  ompBus: OmpEventBus;
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
  /** Single-eviction SDK drain budget (test injectable, plan §3.4). */
  evictDrainTimeoutMs: number;
  /** Global shutdown deadline across all disposals (test injectable). */
  shutdownDisposeDeadlineMs: number;

  constructor({
    agentDir,
    abortTeardownTimeoutMs,
    evictDrainTimeoutMs,
    shutdownDisposeDeadlineMs,
    now,
    createAgentSession: createAgentSessionImpl,
  }: {
    agentDir?: string;
    abortTeardownTimeoutMs?: number;
    evictDrainTimeoutMs?: number;
    shutdownDisposeDeadlineMs?: number;
    /** Monotonic clock for idle TTL (plan §4.2; test injectable). */
    now?: () => number;
    /** Agent factory seam for lifecycle tests (default: SDK createAgentSession). */
    createAgentSession?: typeof createAgentSession;
  } = {}) {
    this.authStorage = null;
    this.modelRegistry = null;
    this.registry = new SessionMetaRegistry({ agentDir });
    this.bus = new WireEventBus();
    /** How long abort() waits for the agent teardown before force-disposing (test injectable). */
    this.abortTeardownTimeoutMs = abortTeardownTimeoutMs ?? ABORT_TEARDOWN_TIMEOUT_MS;
    this.evictDrainTimeoutMs = evictDrainTimeoutMs ?? EVICT_DRAIN_TIMEOUT_MS;
    this.shutdownDisposeDeadlineMs = shutdownDisposeDeadlineMs ?? SHUTDOWN_DISPOSE_DEADLINE_MS;
    this.#now = now ?? (() => performance.now());
    this.#createAgentSessionImpl = createAgentSessionImpl ?? createAgentSession;
    this.#live = new LiveSessionRegistry<HostSession>({ now: this.#now });
    /** omp-native event channel (spec 05 §5.2, master D6-R1 single authority). */
    this.ompBus = new OmpEventBus();
    /**
     * Canonical-read wire id -> client messageID echoed at prompt time.
     * (See the field comment; the map itself is data-proportional by design —
     * plan D5 tracks its long-session growth as a known risk.)
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
        const hostSession = this.#liveHostAnywhere(directoryKey, sessionId);
        const manager = hostSession?.agentSession?.sessionManager;
        if (!manager?.appendModeChange) return undefined;
        const entryId = manager.appendModeChange(mode, data);
        if (hostSession) this.#syncPlanProposalHandler(hostSession, mode);
        return entryId;
      },
      sessionContextFor: (sessionId, directoryKey) => {
        const hostSession = this.#liveHostAnywhere(directoryKey, sessionId);
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
      // Cold entry tree (plan §7.1): build the snapshot inside withColdManager
      // so the cold manager is closed and its entries mirror released on
      // every path — the old contract handed a raw manager to the URI domain
      // and never closed it.
      entryTreeFor: async (sessionID, directory) => {
        const directoryKey = normalizeDirectoryKey(directory);
        const file = await this.#findSessionFile(sessionID, directoryKey);
        if (!file) return null;
        const tree = await withColdManager(file.path, (manager) =>
          buildEntryTreeSnapshot({ sessionID, directory: directoryKey, manager }),
        );
        return { tree };
      },
      localFiles: (sessionID, directory) =>
        this.#listLocalFiles(sessionID, normalizeDirectoryKey(directory)),
      agentsSnapshot: () =>
        this.#live
          .snapshot()
          .filter((record) => record.state === 'live' && record.payload)
          .map((record) => ({
            sessionID: record.sessionId,
            directory: record.directory,
            registry: record.payload!.agentRegistry
          })),
      publish: (type, payload, scope) => this.ompBus.publish(type, payload, scope),
      liveSessionIds: () =>
        this.#live.snapshot().filter((record) => record.state === 'live' || record.state === 'materializing').map((record) => record.sessionId)
    });
    this.bootError = null;
    this.bootPromise = null;
    this.sweeper = setInterval(() => this.#sweepIdleSessions(), IDLE_SWEEP_INTERVAL_MS);
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
    const hostSession = this.#liveHostAnywhere(directory, sessionId) ?? (await this.#materialize(sessionId, directory));
    if (!hostSession) return;
    await this.#setDialogUiContext(hostSession, directory, sessionId, true);
  }

  #detachDialogUi(directory: string, sessionId: string) {
    const hostSession = this.#liveHostAnywhere(directory, sessionId);
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

  /**
   * Idle reaper (plan §4): every live record idle beyond the TTL is a
   * candidate — there is no live-count gate (quantity is not a memory
   * bound). Candidate selection happens outside the gate, but the evict
   * decision re-reads state, TTL, inFlight, leases, pending dialogs and
   * every SDK activity signal INSIDE the per-key gate, with no unprotected
   * await between the checks and beginDispose.
   */
  #sweepIdleSessions() {
    const now = this.#now();
    for (const record of this.#live.snapshot()) {
      if (record.state !== 'live') continue;
      if (now - record.lastUsedAt < IDLE_SESSION_TTL_MS) continue;
      void this.#live
        .withOperation(record.key, async () => {
          const current = this.#live.byKey(record.key);
          if (!current || current !== record || current.state !== 'live') return;
          if (this.#now() - current.lastUsedAt < IDLE_SESSION_TTL_MS) return;
          if (current.inFlight > 0) return;
          if (this.#recordIsActive(current)) return;
          this.#evictRecord(current, 'idle-ttl');
        })
        .catch((error) => {
          console.warn('[omp-host] idle sweep error:', errorText(error));
        });
    }
    // Periodic transport hygiene (plan §6): tokens expire even without mints.
    this.uriDomain?.tokens?.sweep?.();
  }

  /** Interval target; public so the lifecycle tests drive it deterministically. */
  sweepIdleSessionsNow() {
    this.#sweepIdleSessions();
  }

  /** Full activity guard set (plan §4.1) read from the live SDK session. */
  #recordIsActive(record: LiveRecord<HostSession>): boolean {
    const hostSession = record.payload;
    if (!hostSession) return false;
    if (hostSession.awaitingAsyncSince !== null) return true;
    const session = hostSession.agentSession;
    if (!session) return false;
    if (
      session.isStreaming ||
      session.isAborting ||
      session.isRetrying ||
      session.isCompacting ||
      session.isGeneratingHandoff ||
      session.isBashRunning ||
      session.isEvalRunning ||
      session.hasPendingBashMessages ||
      session.hasPendingPythonMessages ||
      session.hasPostPromptWork ||
      session.queuedMessageCount > 0
    ) {
      return true;
    }
    try {
      if (session.hasPendingAsyncWork()) return true;
    } catch {
      // Getter must not veto eviction by throwing.
    }
    if (this.dialogs.registry.pendingCount({ directory: record.directory, sessionId: record.sessionId }) > 0) {
      return true;
    }
    if (this.dialogs.hasUISnapshotFor(record.directory, record.sessionId).holders > 0) {
      return true;
    }
    return false;
  }

  /**
   * Tear the host side off a record: subscriptions, domain handles, UI.
   * Every release settles independently (plan §3.3 step 6): a throwing
   * unsubscribe must not skip the remaining teardown or — worse — abort
   * #evictRecord before the disposal promise exists, which would strand an
   * evicting record that can never finish.
   */
  #releaseHostHandles(hostSession: HostSession): void {
    const failures: string[] = [];
    const attempt = (release: () => void) => {
      try {
        release();
      } catch (error) {
        failures.push(errorText(error));
      }
    };
    attempt(() => {
      hostSession.unsubscribe?.();
      hostSession.unsubscribe = undefined;
    });
    attempt(() => {
      hostSession.nameUnsubscribe?.();
      hostSession.nameUnsubscribe = undefined;
    });
    hostSession.extensionUiPromise = null;
    hostSession.extensionUiInitialized = false;
    const { sessionId, directory } = hostSession;
    attempt(() => this.modesDomain?.release?.(sessionId, directory));
    attempt(() => this.dialogs?.releaseSession?.(directory, sessionId, 'session disposed'));
    attempt(() => this.uriDomain?.descriptors?.releaseForSession?.(directory, sessionId));
    attempt(() => this.uriDomain?.aggregator?.releaseForSession?.(directory, sessionId));
    // wireIdOverrides cleanup (plan D5): necessary, not sufficient — the map
    // still grows with live long sessions by design until phase 5 lands a
    // stable wire id.
    const prefix = `${directory}\u0000${sessionId}\u0000`;
    for (const key of this.wireIdOverrides.keys()) {
      if (key.startsWith(prefix)) this.wireIdOverrides.delete(key);
    }
    if (failures.length > 0) {
      console.warn(`[omp-host] partial host-handle release failure for ${sessionId}:`, failures.join('; '));
    }
  }

  async #artifactsDirFor(sessionId: string, directoryKey: string) {
    const manager = this.#liveHostAnywhere(directoryKey, sessionId)?.agentSession?.sessionManager;
    const liveDir = manager?.getArtifactsDir?.();
    if (typeof liveDir === 'string' && liveDir) return liveDir;
    const file = await this.#findSessionFile(sessionId, directoryKey);
    return file ? artifactsDirForSessionFile(file.path) : null;
  }

  /** Release per-directory host state when the directory goes quiet (plan §6). */
  #maybeReleaseDirectoryState(directory: string): void {
    if (this.#live.liveDirectories().has(normalizeDirectoryKey(directory))) return;
    this.chrome?.releaseDirectory?.(directory);
    // Sidecar meta map: disk stays authoritative; the next access reloads.
    this.registry.release(directory);
  }

  /**
   * Evict one live record (plan §3.4). MUST run inside the record's
   * operation gate. Order: beginDispose (sync, before the first await) →
   * host teardown → session.idle → the single saved dispose promise.
   *
   * The host-side transition is SYNCHRONOUS: by the time this returns, the
   * record is `evicting` with its unsubscribe/domain handles torn off and
   * the SDK disposal running detached. Callers decide whether — and how
   * long — to await the returned promise; awaiting it inside a gate body
   * would let a never-settling disposal occupy the gate forever.
   */
  #evictRecord(
    record: LiveRecord<HostSession>,
    reason: string,
    { emitIdle = true }: { emitIdle?: boolean } = {},
  ): Promise<'disposed' | 'failed'> {
    if (!this.#live.beginEvict(record)) {
      return record.disposePromise ?? Promise.resolve('disposed');
    }
    const hostSession = record.payload;
    const agentSession = hostSession?.agentSession ?? null;
    record.payload = null;
    try {
      // Step 1: reject new SDK work before the host's first await.
      agentSession?.beginDispose?.();
    } catch {
      // beginDispose must not block teardown when it throws.
    }
    if (hostSession) {
      // Step 2: host handles come off synchronously; late events after this
      // point can only land on an already-evicting record.
      this.#releaseHostHandles(hostSession);
      hostSession.agentSession = null;
      this.#maybeReleaseDirectoryState(hostSession.directory);
    }
    if (emitIdle) {
      // Step 3: clients learn the session left live state without waiting
      // for a possibly slow SDK drain.
      this.bus.emit('session.idle', { sessionID: record.sessionId }, record.directory);
    }
    if (!agentSession) {
      this.#live.finishEvict(record, true);
      return Promise.resolve('disposed');
    }
    // Step 4: one saved, detached dispose promise; its settle (success or
    // failure) is the only thing that can move the record out of `evicting`.
    // Failed disposal → observable quarantine tombstone; the SDK object
    // stays referenced by this promise chain so the same file cannot gain a
    // second writer (plan §3.4). errorText bounds the rejection to a
    // message string before it reaches domain state.
    const disposePromise = (async (): Promise<'disposed' | 'failed'> => {
      try {
        await agentSession.dispose({ drainTimeoutMs: this.evictDrainTimeoutMs });
        this.#live.finishEvict(record, true);
        return 'disposed';
      } catch (rejection) {
        console.warn(`[omp-host] session ${record.sessionId} disposal failed (${reason}):`, errorText(rejection));
        this.#live.finishEvict(record, false, errorText(rejection));
        return 'failed';
      }
    })();
    record.disposePromise = disposePromise;
    return disposePromise;
  }

  /** Bounded wait for a record's disposal (never parks on a hung drain). */
  #awaitDisposalBounded(record: LiveRecord<HostSession>): Promise<'disposed' | 'failed' | 'timeout'> {
    const disposal = record.disposePromise ?? Promise.resolve('disposed' as const);
    // SAFETY: the race always resolves with one of the three literal arms.
    return Promise.race([
      disposal,
      new Promise((resolve) => setTimeout(() => resolve('timeout' as const), this.evictDrainTimeoutMs * 2)),
    ]) as Promise<'disposed' | 'failed' | 'timeout'>;
  }

  /**
   * Directory-keyed live lookup (plan §3.2): the requested directory must
   * own the record. Cross-directory same-id records never satisfy each other.
   */
  #liveHost(directory: string | null | undefined, sessionId: string): HostSession | null {
    if (!directory) return null;
    return this.#live.getLive(directory, sessionId)?.payload ?? null;
  }

  /**
   * Live lookup when the caller's directory may differ from the owning one
   * (getSession/updateSession semantics: a live session owns its registry
   * entry and answers first). The exact-key hit is preferred; the id-only
   * fallback returns the unique live record and refuses the ambiguous
   * two-directories case instead of guessing (plan §3.2).
   */
  #liveHostAnywhere(directory: string | null | undefined, sessionId: string): HostSession | null {
    const keyed = this.#liveHost(directory, sessionId);
    if (keyed) return keyed;
    const byId = this.#live.bySessionId(sessionId);
    return byId && byId.state === 'live' ? byId.payload : null;
  }

  #sessionDirFor(cwd: string) {
    return SessionManager.getDefaultSessionDir(cwd, this.registry.agentDir);
  }

  #projectId(directoryKey: string) {
    const hash = crypto.createHash('sha256').update(directoryKey).digest('hex');
    return `prj_${hash.slice(0, 20)}`;
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
      out.push(this.#wireSession(info, directoryKey, metas.get(info.id), this.#liveHost(directoryKey, info.id) ?? undefined));
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
    const live = this.#liveHostAnywhere(directoryKey, sessionID);
    if (live) return this.#wireSessionFromLive(live);
    const file = await this.#findSessionFile(sessionID, directoryKey);
    if (!file) return null;
    // Header scalars stream without a manager (plan §7.2); null → the
    // manager arm below keeps the invalid-header rewrite/mint behavior.
    const scalars = await readSessionScalars(file.path);
    if (scalars) {
      const created = scalars.createdIso ? Date.parse(scalars.createdIso) : Date.now();
      const modified = scalars.modifiedIso ? Date.parse(scalars.modifiedIso) : created;
      const info = {
        id: scalars.id,
        // SessionManager.open's fallback for a missing/deleted recorded cwd is
        // the launch project dir; the host process never setProjectDir's, so
        // process.cwd() is the same value.
        cwd: scalars.cwd ?? process.cwd(),
        title: scalars.title,
        created: new Date(created),
        modified: new Date(Number.isFinite(modified) ? modified : created)
      };
      return this.#wireSession(info, directoryKey, (this.registry.get(directoryKey, sessionID) ?? undefined));
    }
    return withColdManager(file.path, (manager) => {
      const info = this.#infoFromManager(manager, file.path, directoryKey);
      return this.#wireSession(info, directoryKey, (this.registry.get(directoryKey, sessionID) ?? undefined));
    });
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
    const live = this.#liveHostAnywhere(directory, sessionID);
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
    const live = this.#liveHostAnywhere(directoryKey, sessionID);
    const fromKey = live ? normalizeDirectoryKey(live.directory) : directoryKey;
    const key = sessionKey(fromKey, sessionID);
    // Response snapshot before teardown (a live manager still answers the
    // title); the mutation itself runs entirely inside the gate below.
    const info = live ? this.#wireSessionFromLive(live) : await this.getSession({ sessionID, directory: fromKey });
    // Serialize the WHOLE delete under the owning key's gate — evict,
    // bounded disposal wait, registry row, transcript removal (plan §3.4).
    // Await INSIDE the gate: a re-materialize squeezing between disposal
    // and rmSync would resurrect a second writer on the file being deleted.
    // A disposal that fails or outlives the bounded wait refuses the delete
    // with a retryable busy error — unlinking a transcript a live writer
    // still holds is the POSIX orphan-write / Windows locked-file hazard.
    await this.#live.withOperation(key, async () => {
      const record = this.#live.get(fromKey, sessionID);
      if (record && record.state === 'live') {
        this.#evictRecord(record, 'delete', { emitIdle: false });
      }
      const blocking = this.#live.byKey(key);
      if (blocking) {
        const outcome = await this.#awaitDisposalBounded(blocking);
        if (outcome !== 'disposed') {
          throw new SessionBusyError(
            outcome === 'failed' ? 'session-failed' : 'session-evicting',
            `session ${sessionID} is not deletable: disposal ${outcome === 'timeout' ? 'did not settle' : `failed (${blocking.failure?.reason ?? 'unknown'})`}`,
            sessionID,
          );
        }
      }
      const file = await this.#findSessionFile(sessionID, fromKey);
      this.uriDomain?.descriptors?.releaseForSession?.(fromKey, sessionID);
      this.uriDomain?.aggregator?.releaseForSession?.(fromKey, sessionID);
      this.registry.remove(fromKey, sessionID);
      if (file) {
        // Removal failures propagate: answering "deleted" while a writer
        // still holds the transcript is the lie this gate exists to kill.
        fs.rmSync(file.path, { force: true });
      }
    });
    this.#maybeReleaseDirectoryState(fromKey);
    this.bus.emit('session.deleted', { sessionID }, fromKey);
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
    await this.#boot();
    const directoryKey = normalizeDirectoryKey(directory);
    const wireIdFor = this.#wireIdResolver(directoryKey, sessionID);
    const meta = (this.registry.get(directoryKey, sessionID) ?? undefined);
    const live = this.#liveHostAnywhere(directoryKey, sessionID);
    const liveCount = live?.agentSession?.messages?.length ?? -1;
    const file = await this.#findSessionFile(sessionID, directoryKey);
    const externalChange = live
      ? classifyExternalChange(live.fileSignature, file?.path ?? '')
      : 'unchanged';
    if (file) {
      // Windowed cold read (plan §7.2): metadata pass + bounded content pass
      // produce the requested page without materializing the transcript;
      // labeled fallbacks keep the full-materialization arm below.
      const agent = wireAgentFor(personaKeyFor(meta?.persona ?? meta?.agent));
      const streamed = await readTranscriptMessagePage(file.path, {
        sessionID,
        directory: directoryKey,
        agent,
        wireIdFor,
        limit,
        before,
      });
      if (streamed) {
        const liveArmWins = externalChange !== 'dirty' && liveCount >= 0 && liveCount >= streamed.fileMessageCount;
        if (liveArmWins) {
          return paginateProjectedMessages(
            this.#mergeTurnEventDividers(
              projectConversation(live?.agentSession?.messages ?? [], {
                sessionID,
                directory: directoryKey,
                agent,
                wireIdFor
              }),
              streamed.dividerEntries,
              sessionID
            ),
            { limit, before }
          );
        }
        if (streamed.fileMessageCount > 0 || liveCount < 0) return streamed.page;
        return null;
      }
    }
    const projected = await this.#projectedMessages(sessionID, directory);
    if (!projected) return null;
    return paginateProjectedMessages(projected, { limit, before });
  }
  async #projectedMessages(sessionID: string, directory: string | null | undefined) {
    await this.#boot();
    const directoryKey = normalizeDirectoryKey(directory);
    const wireIdFor = this.#wireIdResolver(directoryKey, sessionID);
    const meta = (this.registry.get(directoryKey, sessionID) ?? undefined);
    const live = this.#liveHostAnywhere(directoryKey, sessionID);
    const liveSession = live?.agentSession ?? null;
    const liveCount = liveSession?.messages?.length ?? -1;

    // File transcript (transcript: true) is the display truth: it keeps the
    // full history including pre-compaction user turns and divider entries.
    // A live session's runtime context is post-compaction (users folded into
    // the summary), which used to blank the UI — so read the live list only
    // when it is at least as complete as the file.
    const file = await this.#findSessionFile(sessionID, directoryKey);
    // Dual-write classification (plan §8): an external rewrite that shrank
    // or mutated the transcript must not be masked by a longer, now-stale
    // live mirror — dirty means the file arm is the display truth outright.
    const externalChange = live
      ? classifyExternalChange(live.fileSignature, file?.path ?? '')
      : 'unchanged';
    if (file) {
      // Labeled fallback (plan §7.1): the file arm needs entries for
      // turn-state stampers and dividers, so it full-materializes through a
      // cold manager that withColdManager closes and releases in finally.
      return withColdManager(file.path, (manager) => {
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
        // Dirty external rewrite: the file is the truth even when the stale
        // live mirror is longer (plan §8.1 step 4).
        const liveArmWins = externalChange !== 'dirty' && liveCount >= 0 && liveCount >= fileMessages.length;
        if (liveArmWins) {
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
        // SAFETY: both consume arms returned ProjectedMessage[]; the null
        // arm matches the outer no-file/no-live contract.
        return null as ProjectedMessage[] | null;
      });
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

  /**
   * Materialize (or join) the live session for one key (plan §3.2/§3.3).
   * Runs inside the per-key operation gate: concurrent callers serialize,
   * the second one joins the freshly committed record. An evicting record
   * is awaited once (bounded by the drain budget) and retried exactly once;
   * a quarantined (failed) or shutting-down key rejects with a retryable
   * SessionBusyError instead of silently building a second writer.
   */
  async #materialize(sessionId: string, directoryKey: string): Promise<HostSession | null> {
    const key = sessionKey(directoryKey, sessionId);
    return this.#live.withOperation(key, () => this.#materializeGated(sessionId, directoryKey));
  }

  async #materializeGated(sessionId: string, directoryKey: string): Promise<HostSession | null> {
    await this.#boot();
    for (let attempt = 0; ; attempt += 1) {
      const liveRecord = this.#live.getLive(directoryKey, sessionId);
      if (liveRecord) {
        // A materialize is a user-visible live operation: refresh the TTL.
        this.#live.touch(liveRecord);
        return liveRecord.payload;
      }
      const record = this.#live.get(directoryKey, sessionId);
      if (record?.state === 'evicting') {
        if (attempt >= 1) {
          throw new SessionBusyError('session-evicting', `session ${sessionId} is evicting`, sessionId);
        }
        // Wait for the single disposal promise — bounded: a disposal that
        // never settles must park this caller at a retryable busy error,
        // not forever (plan §3.4 timeout rule).
        await Promise.race([
          record.disposePromise ?? Promise.resolve('disposed' as const),
          new Promise((resolve) => setTimeout(resolve, this.evictDrainTimeoutMs * 2)),
        ]).catch(() => {});
        continue;
      }
      if (this.#closing) {
        throw new SessionBusyError('host-shutting-down', 'host is shutting down', sessionId);
      }
      break;
    }
    const record = this.#live.beginMaterialize(directoryKey, sessionId);
    try {
      const hostSession = await this.#materializeNow(sessionId, directoryKey);
      if (!hostSession) {
        // Session file absent — cold miss, not a setup failure.
        this.#live.failMaterialize(record);
        return null;
      }
      this.#live.commitMaterialize(record, hostSession);
      return hostSession;
    } catch (error) {
      // #materializeNow already released its resources in its finally
      // block (plan §3.3); drop the dedup row and rethrow.
      this.#live.failMaterialize(record);
      throw error;
    }
  }


  /**
   * Build one live session. Runs inside the key's gate with a
   * `materializing` record open. Every step after the first await is
   * guarded by a try/finally that releases each installed resource
   * exactly once (plan §3.3): event unsubscribe, name unsubscribe,
   * agentSession.dispose, temporary-manager close, and the domain handles.
   */
  async #materializeNow(sessionId: string, directoryKey: string): Promise<HostSession | null> {
    const file = await this.#findSessionFile(sessionId, directoryKey);
    if (!file) return null;
    const manager = await SessionManager.open(file.path, this.#sessionDirFor(directoryKey));
    let agentCreated = false;
    let hostSession: HostSession | null = null;
    try {
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
      const { session, setToolUIContext } = await this.#createAgentSessionImpl({
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
      agentCreated = true;
      hostSession = {
        key: sessionKey(directoryKey, sessionId),
        sessionId,
        directory: directoryKey,
        agentSession: null,
        currentPersona: personaKeyFor(meta?.persona ?? meta?.agent),
        projector: null,
        pendingUserWireId: null,
        lastUserWireId: null,
        syncedEntryKeys: new Set(),
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
        // Dual-write identity at materialize (plan §8): later external
        // writes are classified against this.
        fileSignature: fileSignature(file.path, tailEntryIdOf(file.path))
      };
      hostSession.appliedPlugins = await this.#snapshotAppliedPlugins(directoryKey);
      hostSession.agentSession = session;
      hostSession.unsubscribe = session.subscribe((event) => {
        try {
          this.#handleEngineEvent(hostSession!, event);
        } catch (error) {
          console.error('[omp-host] event projection error:', error);
        }
      });
      // Modes tracker for this session (cold-recovery + mode_change appends).
      this.modesDomain?.trackerFor(sessionId, directoryKey);
      // Apply and await the lease context before publishing the record so a
      // lease that raced materialization still gets its extension UI.
      if (this.dialogs.hasUISnapshotFor(directoryKey, sessionId).hasUI) {
        await this.#setDialogUiContext(hostSession, directoryKey, sessionId, true);
      }
      hostSession.nameUnsubscribe = session.sessionManager?.onSessionNameChanged?.(() => {
        const info = this.#wireSessionFromLive(hostSession!);
        this.registry.update(directoryKey, sessionId, {
          title: info.title,
          timeUpdated: Date.now()
        });
        this.bus.emit('session.updated', { sessionID: sessionId, info }, directoryKey);
      });
      return hostSession;
    } catch (error) {
      // Failure cleanup (plan §3.3): every installed resource is released
      // independently and idempotently; the original error propagates.
      const cleanup = hostSession;
      await Promise.allSettled([
        (async () => {
          cleanup?.unsubscribe?.();
          if (cleanup) cleanup.unsubscribe = undefined;
        })(),
        (async () => {
          cleanup?.nameUnsubscribe?.();
          if (cleanup) cleanup.nameUnsubscribe = undefined;
        })(),
        (async () => {
          if (cleanup) this.#releaseHostHandles(cleanup);
        })(),
        (async () => {
          const agent = cleanup?.agentSession;
          if (cleanup) cleanup.agentSession = null;
          if (agentCreated && agent) await agent.dispose({ drainTimeoutMs: this.evictDrainTimeoutMs });
        })(),
        (async () => {
          if (!agentCreated) await manager.close().catch(() => {});
        })(),
      ]);
      throw error;
    }
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
    return this.#live
      .snapshot()
      .filter((record) => record.state === 'live' && record.payload)
      .map((record) => record.payload!)
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
    for (const hostSession of this.#live.snapshot().map((record) => record.payload).filter((payload): payload is HostSession => payload !== null)) {
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
    // Late-event guard (plan §3.4): after eviction begins, events may still
    // arrive from in-flight SDK emission; they must land on a still-live
    // record only — an evicting/failed record starts no new work.
    const record = this.#live.byKey(hostSession.key);
    if (!record || record.state !== 'live' || record.payload !== hostSession) return;
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
        // tool_execution_end owns completion.
        hostSession.projector?.toolPartial(event.toolCallId, {
          text: typeof event.partialResult === 'string' ? event.partialResult : (event.partialResult?.text ?? event.partialResult?.output),
          asyncState: event.partialResult?.details?.async?.state
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
        // Transcript roles without dedicated SDK events (dividers, custom
        // notes) tail-sync here, before the busy/idle decision.
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
        // Bounded diagnostic keys (plan §6): unknown types fold into a fixed
        // `other` bucket once the key set is full; total drops are counted.
        const type = unknownEvent?.type ?? 'unknown';
        if (this.unknownEventCounts.size >= OmpHostEngine.#UNKNOWN_EVENT_KEYS_MAX && !this.unknownEventCounts.has(type)) {
          this.unknownEventCounts.set('other', (this.unknownEventCounts.get('other') ?? 0) + 1);
        } else {
          this.unknownEventCounts.set(type, (this.unknownEventCounts.get(type) ?? 0) + 1);
        }
        return;
      }
    }
  }

  async getSessionStatuses({ directory }: { directory?: string }) {
    await this.#boot();
    const AWAITING_ASYNC_TIMEOUT_MS = 10 * 60 * 1000;
    const now = Date.now();
    const statuses: Record<string, { type: 'busy' } | { type: 'idle' }> = {};
    for (const record of this.#live.snapshot()) {
      const live = record.payload;
      if (!live || record.state !== 'live') continue;
      const id = record.sessionId;
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
    // Structured rows + retry-recovery stream from one scan (plan §7.2);
    // null → manager arm below (legacy versions, blob refs).
    const streamedRows = await readSessionEventRows(
      file.path,
      wanted,
      this.#wireIdResolver(directoryKey, sessionID)
    );
    if (streamedRows) return streamedRows;
    const out = await withColdManager(file.path, async (manager) => {
      const rows: unknown[] = [];
      for (const entry of manager.getEntries() ?? []) {
        const kind = entry.type === 'compaction' ? 'compaction' : entry.type === 'branch_summary' ? 'branch_summary' : entry.type === 'model_change' ? 'model_change' : entry.type === 'mode_change' ? 'mode_change' : entry.type === 'ttsr_injection' ? 'ttsr_injection' : null;
        if (!kind || (wanted.size > 0 && !wanted.has(kind))) continue;
        rows.push({
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
      return rows;
    });
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

  async #transcriptContext(sessionID: string, directory: string | null | undefined) {
    await this.#boot();
    const directoryKey = normalizeDirectoryKey(directory);
    const file = await this.#findSessionFile(sessionID, directoryKey);
    if (!file) return null;
    return withColdManager(file.path, (manager) => manager.buildSessionContext({ transcript: true }));
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
    // Dual-write gate (plan §8.2): when the transcript changed externally in
    // a way the live mirror cannot absorb (dirty rewrite) and the session is
    // inactive, rebuild the writer from disk — bounded to one bounded retry,
    // never during streaming/retry/compaction/async work, and always through
    // the key's gate so no second writer can appear.
    await this.#reloadIfDirty(sessionID, directoryKey);
    const hostSession = await this.#materialize(sessionID, directoryKey);
    if (!hostSession) return null;
    const session = hostSession.agentSession;
    if (!session) return null;
    if (hostSession.extensionUiPromise) await hostSession.extensionUiPromise;
    const promptRecord = this.#live.byKey(hostSession.key);
    if (promptRecord && promptRecord.state === 'live') {
      // A prompt is a user-visible write: refresh the idle TTL. The turn's
      // streaming lifetime is guarded by the SDK activity getters; the
      // in-flight slot below covers only the synchronous dispatch window.
      this.#live.touch(promptRecord);
    }

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
      // construction, so rebuild the AgentSession over the same transcript —
      // through the gate: the old session must finish disposing before the
      // replacement materializes (plan §3.4, no double writer).
      const personaRecord = this.#live.byKey(hostSession.key);
      await this.#live.withOperation(sessionKey(directoryKey, sessionID), async () => {
        const record = this.#live.byKey(hostSession.key);
        if (record && record.state === 'live' && record.payload === hostSession) {
          this.#evictRecord(record, 'persona-rebuild', { emitIdle: false });
        }
      });
      if (personaRecord) await this.#awaitDisposalBounded(personaRecord);
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
    const dispatchRecord = this.#live.byKey(hostSession.key);
    if (dispatchRecord && dispatchRecord.state === 'live') this.#live.beginUse(dispatchRecord);
    try {
      await session.prompt(textOnly, {
        images: imageContents,
        streamingBehavior
      });
    } finally {
      // Terminal activity (streaming, async work) is read from the SDK
      // getters by the sweeper; the dispatch slot must never outlive the
      // dispatch itself — a lost agent_end must not pin the session forever.
      if (dispatchRecord && dispatchRecord.state === 'live') this.#live.endUse(dispatchRecord);
    }
    return wire;
  }

  /**
   * Bounded dirty-reload (plan §8.2): classify the transcript against the
   * live record's materialize-time signature; on `dirty` with every activity
   * guard quiet, evict inside the key's gate and let the caller's
   * #materialize rebuild from disk. Reloads are never attempted while the
   * session streams/retries/compacts/holds async work — those turns keep the
   * steer/queue semantics instead, and "absolute freshness" stays best-effort
   * (plan D6).
   */
  async #reloadIfDirty(sessionID: string, directoryKey: string): Promise<void> {
    const record = this.#live.get(directoryKey, sessionID);
    if (!record || record.state !== 'live' || !record.payload) return;
    if (record.inFlight > 0 || this.#recordIsActive(record)) return;
    const file = await this.#findSessionFile(sessionID, directoryKey);
    if (!file) return;
    if (classifyExternalChange(record.payload.fileSignature, file.path) !== 'dirty') return;
    console.warn(`[omp-host] transcript for ${sessionID} changed externally; rebuilding live writer from disk`);
    await this.#live.withOperation(sessionKey(directoryKey, sessionID), async () => {
      const current = this.#live.byKey(sessionKey(directoryKey, sessionID));
      if (!current || current.state !== 'live' || !current.payload) return;
      // Recheck inside the gate: activity may have arrived since selection.
      if (current.inFlight > 0 || this.#recordIsActive(current)) return;
      if (classifyExternalChange(current.payload.fileSignature, file.path) !== 'dirty') return;
      this.#evictRecord(current, 'dual-write-reload');
    });
    const after = this.#live.get(directoryKey, sessionID);
    if (after && after.state === 'evicting') await this.#awaitDisposalBounded(after);
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
    const modelRecord = this.#live.byKey(hostSession.key);
    if (modelRecord && modelRecord.state === 'live') this.#live.touch(modelRecord);
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
    const live = this.#liveHostAnywhere(directory, sessionID);
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
    // Clients learn the live state ended before any drain; then the bounded
    // disposal starts inside the key's gate and is awaited outside it.
    this.bus.emit('session.idle', { sessionID }, live.directory);
    const record = this.#live.get(live.directory, sessionID);
    if (record && record.state === 'live') {
      await this.#live
        .withOperation(record.key, async () => {
          this.#evictRecord(record, 'abort-force-dispose', { emitIdle: false });
        })
        .catch(() => {});
      await this.#awaitDisposalBounded(record);
    }
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
    for (const hostSession of this.#live.snapshot().map((record) => record.payload).filter((payload): payload is HostSession => payload !== null)) {
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
    const hostSession = this.#liveHostAnywhere(directoryKey, sessionID);
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

  /**
   * Counters-only stream diagnostics (docs/plan.md §9.1, phase 0 slice).
   * Returns sizes and estimates — never transcripts, payloads, paths, or
   * credentials. All "bytes" fields are the buses' declared serialized
   * estimates (UTF-16 units), not JS-heap measures; `process.*Bytes` are
   * Node's own memoryUsage counters. The data-proportional counts are
   * labeled as such (plan §6): they grow with live data by design and are
   * not bounded caches.
   *
   * §9.1 also asks for OS handle and child-process counts. Under Bun,
   * `process._getActiveHandles()`/`getActiveResourcesInfo()` are stubs that
   * always return `[]` — reporting them would masquerade as authoritative
   * zeros, so they are deliberately absent. Child processes are covered by
   * the external sampler scripts/perf/process-tree-sample.mjs instead.
   */
  getStreamDiagnostics() {
    const memory = process.memoryUsage();
    return {
      wireBus: this.bus.stats(),
      ompBus: this.ompBus.stats(),
      dataProportional: {
        liveSessions: this.#live.stats(),
        wireIdOverrides: this.wireIdOverrides.size,
        personas: this.personas.size,
      },
      process: {
        heapUsedBytes: memory.heapUsed,
        externalBytes: memory.external,
        arrayBufferBytes: memory.arrayBuffers,
        rssBytes: memory.rss,
      },
    };
  }

  /**
   * Graceful shutdown (plan §3.4): close the intake, synchronously
   * beginDispose every live record (inside each key's gate), then wait for
   * all disposals under one global deadline. Records whose disposal has not
   * settled by the deadline stay quarantined in the registry — observably
   * failed, still blocking new writers — instead of being cleared and
   * masquerading as released. `sessions.clear()` before these steps was not
   * a shutdown.
   */
  async shutdown() {
    this.#closing = true;
    clearInterval(this.sweeper);
    const records = this.#live.snapshot();
    const deadline = new Promise((resolve) => setTimeout(resolve, this.shutdownDisposeDeadlineMs));
    await Promise.all(
      records.map((record) =>
        this.#live
          .withOperation(record.key, async () => {
            const current = this.#live.byKey(record.key);
            if (!current || current.state !== 'live') return;
            const disposal = this.#evictRecord(current, 'shutdown', { emitIdle: false });
            // The gate body itself is bounded by the global deadline: a
            // disposal that never settles must not park shutdown (the
            // record simply stays `evicting` — quarantined, observable).
            await Promise.race([disposal, deadline]);
          })
          .catch((error) => {
            console.warn('[omp-host] shutdown dispose failed:', errorText(error));
          }),
      ),
    );
    const quarantined = this.#live.stats();
    if (quarantined.evicting > 0 || quarantined.failed > 0) {
      console.warn(
        `[omp-host] shutdown: ${quarantined.evicting} disposal(s) still draining, ${quarantined.failed} quarantined — restart clears them`,
      );
    }
    try {
      await this.dialogs?.dispose?.('omp-host shutdown');
    } catch {
      // Settle-all is best-effort at shutdown.
    }
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
   * omp's SessionManager.moveTo and migrate the sidecar metadata. A live
   * session is evicted (awaited, inside the owning key's gate) first — the
   * old cold-path opened a second writable manager while a live writer held
   * the same file (plan §3.4). Ownership transfers cold; the next prompt in
   * the destination materializes fresh.
   */
  async moveSession({ sessionID, destination }: { sessionID: string; destination: string }) {
    await this.#boot();
    const toKey = normalizeDirectoryKey(destination);
    const live = this.#liveHostById(sessionID);
    const fromKey = live ? normalizeDirectoryKey(live.directory) : ((await this.#locateDirectory(sessionID)) ?? null);
    if (!fromKey) return null;
    const key = sessionKey(fromKey, sessionID);
    // Evict → bounded disposal wait → transcript relocation ALL inside the
    // owning key's gate: an interleaved materialize would open a second
    // writer on the file being moved, and a disposal that fails or times
    // out must refuse the move instead of relocating under a live writer
    // (plan §3.4). Ownership transfers cold; the next prompt in the
    // destination materializes fresh.
    await this.#live.withOperation(key, async () => {
      const record = this.#live.get(fromKey, sessionID);
      if (record && record.state === 'live') {
        this.#evictRecord(record, 'move', { emitIdle: true });
      }
      const blocking = this.#live.byKey(key);
      if (blocking) {
        const outcome = await this.#awaitDisposalBounded(blocking);
        if (outcome !== 'disposed') {
          throw new SessionBusyError(
            outcome === 'failed' ? 'session-failed' : 'session-evicting',
            `session ${sessionID} is not movable: disposal ${outcome === 'timeout' ? 'did not settle' : `failed (${blocking.failure?.reason ?? 'unknown'})`}`,
            sessionID,
          );
        }
      }
      const file = await this.#findSessionFile(sessionID, fromKey);
      if (file) {
        // The relocating manager is a cold read of the same file: close +
        // release in finally, same contract as every other cold open.
        await withColdManager(file.path, (manager) => manager.moveTo(toKey, this.#sessionDirFor(toKey)));
      }
      this.registry.move(fromKey, toKey, sessionID);
    });
    this.#maybeReleaseDirectoryState(fromKey);
    const session = await this.getSession({ sessionID, directory: toKey });
    if (session) this.bus.emit('session.updated', { sessionID, info: session }, toKey);
    return session;
  }

  async #locateDirectory(sessionID: string) {
    const record = this.#live.bySessionId(sessionID);
    if (record === undefined) return null; // same id live in two directories — refuse to guess
    if (record !== null) return record.directory;
    const byDirectory = await this.listAllSessions({});
    for (const [directory, list] of byDirectory) {
      if (list.some((session: { id: string }) => session.id === sessionID)) return directory;
    }
    return null;
  }

  /** Unique live payload by id (null-ambiguous on two-directory duplicates). */
  #liveHostById(sessionId: string): HostSession | null {
    const record = this.#live.bySessionId(sessionId);
    return record && record.state === 'live' ? record.payload : null;
  }

  /** Test/diagnostics view: live record access by id. */
  liveRecord(sessionId: string, directory?: string): LiveRecord<HostSession> | null {
    if (directory) return this.#live.get(directory, sessionId);
    const record = this.#live.bySessionId(sessionId);
    return record === undefined ? null : record;
  }
}
