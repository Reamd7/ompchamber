// Session modes & agents domain — spec docs/omp-parity/02-agents-and-modes.md
// (server side, Wave-1 self-contained module; the coordinator mounts it).
//
// Surfaces:
//   1. createModeTracker       — session mode state machine (02 §5.4) with
//                                 mode_change entry persistence and the
//                                 omp.mode.changed projection.
//   2. agent-definitions CRUD  — omp agent discovery chain as the read
//                                 authority; writes are .md files in the
//                                 user/project agents dirs; bundled is
//                                 read-only (02 §5.2, GAP-B03/B04).
//   3. personas CRUD + personaFor — independent persona resource and the
//                                 materialize-time systemPrompt overlay
//                                 (02 §5.2a, master D6-R12).
//   4. planReviewBridge        — xd://propose hook producing
//                                 omp.plan.review_requested and the GET /plan
//                                 review payload (02 §5.5).
//   5. createModesDomain + registerModesDomainRoutes — per-session
//                                 tracker/bridge ownership and route mounting.
//
// Engine integration points (coordinator wires; this module never imports
// engine.js — all SDK state reaches it through the injected callbacks):
//   - #materialize: persona overlay via personaFor(meta, personasStore) →
//     systemPrompt/toolNames; status 'standard' = no overlay (02 §5.1 D-B2).
//     (The old planYolo mapping is deleted — plan mode is a session mode
//     driven by the mode endpoints, 02 §5.8.)
//   - per hostSession: domain.trackerFor(id, dir) / domain.bridgeFor(id, dir);
//     on plan enter call session.setPlanProposalHandler(bridge.hookFor(session))
//     (SDK: agent-session.ts:1733-1735, mirrors TUI interactive-mode.ts:2739).
//   - #handleEngineEvent 'goal_updated': tracker.applyGoalUpdate(event.goal,
//     event.state) keeps the mode snapshot fresh; the omp.goal.updated publish
//     itself already lives in engine.js (Wave 0).
//   - prompt(): persona-switch rebuild condition (02 §5.1 D-B3).

import fs from 'node:fs';
import path from 'node:path';
import { parse as parseYaml, stringify as stringifyYaml } from 'yaml';
import { BUILTIN_TOOLS } from '@oh-my-pi/pi-coding-agent';
import { normalizeDirectoryKey } from './registry.ts';
import { errorText, errorCode, featureUnavailable, ompFeatures } from './omp-parity.ts';

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/** Mode projection value (02 §5.4). Prewalk is an orthogonal status bit (5.7). */
export type ModeValue = 'none' | 'plan' | 'plan_paused' | 'goal' | 'goal_paused' | 'vibe' | 'loop';

/**
 * Mode projection set (02 §5.4) — the runtime mirror of {@link ModeValue}.
 * Prewalk is an orthogonal status bit (5.7). Typed `readonly string[]` so
 * `.includes()` accepts the unvalidated route input.
 */
export const MODE_VALUES: readonly string[] = Object.freeze([
  'none', 'plan', 'plan_paused', 'goal', 'goal_paused', 'vibe', 'loop',
]);

/** TUI default plan file (interactive-mode.ts:2307-2309 #getPlanFilePath). */
export const DEFAULT_PLAN_FILE_PATH = 'local://PLAN.md';

/** Plan review choices (02 §5.5; TUI plan-review-overlay options 3979-3982). */
export const PLAN_REVIEW_CHOICES: readonly string[] = Object.freeze([
  'approve-execute', 'approve-compact', 'approve-keep', 'refine',
]);

/** ConfiguredThinkingLevel selectors (SDK thinking.ts:56-65,138-141). */
const THINKING_LEVELS = new Set([
  'auto', 'inherit', 'off', 'minimal', 'low', 'medium', 'high', 'xhigh', 'max',
]);

const AGENT_NAME_PATTERN = /^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$/;


const json = <T>(data: T, init?: ResponseInit): Response => Response.json(data, init);
const badRequest = (message: string) => json({ error: 'bad-request', message }, { status: 400 });

/**
 * Parse a JSON request body (corrupt input → {}). The expected wire shape
 * rides the call-site type parameter; every field is runtime-validated by
 * the caller, so the parse-time assertion is the only leap of faith.
 */
const readJsonBody = async <T extends object>(request: Request): Promise<T> => {
  try {
    // SAFETY: the parsed body is only read through T's fields, each of which
    // the caller runtime-validates before use (see the contract above).
    return (await request.json()) as T;
  } catch {
    // SAFETY: a corrupt body degrades to {} so the callers' per-field checks
    // observe "absent" instead of a parse error mid-route.
    return {} as T;
  }
};

/**
 * Domain error body: an `error` code string plus the context fields the
 * throwing site echoes (names, conflicts, invalid-input rejects).
 */
export interface ModeErrorBody {
  error?: string;
  message?: string;
  /** mode-conflict / invalid-transition context (02 §5.4). */
  conflict?: string;
  mode?: string;
  /** agent/persona name the error is about (02 §5.2/§5.2a). */
  name?: string;
  /** rejected scope value (02 §5.2). */
  scope?: string;
  /** unknown-tools reject list (02 §5.2). */
  tools?: string[];
  /** rejected thinkingLevel: the raw wire value (02 §5.2). */
  thinkingLevel?: unknown;
  /** rejected loop-enter values (02 §5.4). */
  count?: number;
  durationMs?: number;
  /** invalid review-choice context (02 §5.5). */
  choice?: string;
  choices?: string[];
}

/** Domain error carrying an HTTP status + body; route handlers map it 1:1. */
export class ModeDomainError extends Error {
  status: number;
  body: ModeErrorBody;

  constructor(status: number, body: ModeErrorBody) {
    super(body?.message ?? body?.error ?? 'mode-domain-error');
    this.status = status;
    this.body = body;
  }
}

const conflictFor = (mode: string): string => {
  if (mode === 'plan' || mode === 'plan_paused') return 'plan';
  if (mode === 'goal' || mode === 'goal_paused') return 'goal';
  return mode;
};

const modeConflict = (mode: string) =>
  new ModeDomainError(409, {
    error: 'mode-conflict',
    conflict: conflictFor(mode),
    message: `Exit ${conflictFor(mode)} mode first.`,
  });

// ---------------------------------------------------------------------------
// 1a. Route plumbing (host.ts dispatches handler(request, { params, url, headers }))
// ---------------------------------------------------------------------------

/** Per-request route context supplied by the host route table (host.ts). */
export interface ModesRouteContext {
  params?: { [name: string]: string | undefined };
  url?: URL;
  headers?: Headers;
}

/** omp-host route handler (Basic auth is enforced by host.js outside these). */
export type ModesRouteHandler = (
  request: Request,
  ctx?: ModesRouteContext,
) => Response | Promise<Response>;

/** `route(method, pattern, handler)` registration callback (host.ts / endpoints.ts). */
export type ModesRouteMount = (
  method: string,
  pattern: string,
  handler: ModesRouteHandler,
) => void;

// ---------------------------------------------------------------------------
// 1. Storage adapters (personas sidecar)
// ---------------------------------------------------------------------------

/** Persona record (spec 02 §5.2a): name + optional overlay fields. */
export interface OmpPersona {
  name: string;
  description?: string;
  systemPrompt?: string;
  tools?: string[];
}

/** Personas sidecar store (the jsonFileStore / mapBackedStore product). */
export interface PersonaStore {
  load(): OmpPersona[];
  save(records: OmpPersona[]): void;
}

/**
 * JSON sidecar file store (used by the personas store). Shape:
 * `{ [key]: records[] }`; missing/corrupt file → [].
 */
export function jsonFileStore(filePath: string, key = 'agents'): PersonaStore {
  return {
    load() {
      try {
        const parsed = JSON.parse(fs.readFileSync(filePath, 'utf8'));
        const records = parsed?.[key];
        return Array.isArray(records) ? records.filter((r) => r && typeof r.name === 'string') : [];
      } catch {
        return [];
      }
    },
    save(records) {
      fs.mkdirSync(path.dirname(filePath), { recursive: true });
      fs.writeFileSync(filePath, JSON.stringify({ [key]: records }, null, 2));
    },
  };
}

/** Adapter over the engine's live personas Map (coordinator wiring). */
export function mapBackedStore(map: Map<string, OmpPersona>, persist?: (records: OmpPersona[]) => void): PersonaStore {
  return {
    load() {
      return [...map.values()];
    },
    save(records) {
      map.clear();
      for (const record of records) map.set(record.name, record);
      persist?.(records);
    },
  };
}

// ---------------------------------------------------------------------------
// 2. Agent definitions (02 §5.2 — omp discovery chain + .md file storage)
// ---------------------------------------------------------------------------

const definitionScope = (body: AgentDefinitionWriteBody) => {
  const outer = typeof body?.scope === 'string' ? body.scope : undefined;
  const inner = typeof body?.definition?.scope === 'string' ? body.definition.scope : undefined;
  if (outer !== undefined && inner !== undefined && outer !== inner) {
    throw new ModeDomainError(400, { error: 'scope-mismatch', message: 'scope differs between body and definition' });
  }
  return outer ?? inner;
};

const validateName = (name: AgentDefinitionInput['name'], { label = 'name' }: { label?: string } = {}): string => {
  if (typeof name !== 'string' || !AGENT_NAME_PATTERN.test(name.trim())) {
    throw new ModeDomainError(400, { error: 'invalid-name', message: `${label} must match ${AGENT_NAME_PATTERN}` });
  }
  return name.trim();
};

const validateTools = (tools: AgentDefinitionInput['tools'], allowedTools: Set<string>): string[] => {
  if (tools === undefined || tools === null) return [];
  if (!Array.isArray(tools) || tools.some((tool) => typeof tool !== 'string')) {
    throw new ModeDomainError(400, { error: 'invalid-tools', message: 'tools must be an array of strings' });
  }
  const unknown = [...new Set(tools)].filter((tool) => !allowedTools.has(tool));
  if (unknown.length > 0) {
    throw new ModeDomainError(400, { error: 'unknown-tools', tools: unknown });
  }
  return [...new Set(tools)];
};

const parseCsvList = (value: AgentDefinitionInput['model'] | AgentDefinitionInput['spawns']) => (
  typeof value === 'string'
    ? value.split(',').map((entry) => entry.trim()).filter(Boolean)
    : value
);

/** boolean | non-empty model pattern; `null` clears the field (02 §5.2). */
const validateFlagOrPattern = (field: string, value: AgentDefinitionInput['prewalk']) => {
  if (value === null) return null;
  if (value === true || value === false) return value;
  if (typeof value === 'string' && value.trim()) return value.trim();
  throw new ModeDomainError(400, {
    error: `invalid-${field}`,
    message: `${field} must be a boolean or a model pattern string`,
  });
};

/**
 * Unvalidated /omp/agent-definitions wire input (02 §5.2/§5.3): every field
 * is runtime-checked below; optional fields accept `null` to clear.
 */
export interface AgentDefinitionInput {
  name?: unknown;
  description?: unknown;
  systemPrompt?: unknown;
  model?: unknown;
  thinkingLevel?: unknown;
  tools?: unknown;
  spawns?: unknown;
  prewalk?: unknown;
  advisor?: unknown;
  readSummarize?: unknown;
  scope?: unknown;
}

/** Validated definition patch; `null` clears an optional field. */
/** The seven task-override keys shared by patch and serialization. */
const AGENT_OVERRIDE_KEYS = ['model', 'thinkingLevel', 'tools', 'spawns', 'prewalk', 'advisor', 'readSummarize'] as const;
type AgentOverrideKey = (typeof AGENT_OVERRIDE_KEYS)[number];

export interface AgentDefinitionPatch {
  description?: string;
  systemPrompt?: string;
  model?: string[] | null;
  thinkingLevel?: string | null;
  tools?: string[] | null;
  spawns?: '*' | string[] | null;
  prewalk?: boolean | string | null;
  advisor?: boolean | string | null;
  readSummarize?: boolean | null;
}

/** POST/PUT /omp/agent-definitions body: a `{ scope?, renameTo?, definition? }`
 *  wrapper or a bare definition patch — both shapes runtime-validated. */
export interface AgentDefinitionWriteBody extends AgentDefinitionInput {
  renameTo?: unknown;
  definition?: AgentDefinitionInput;
}

/**
 * Validate a definition patch (02 §5.2/§5.3 — the omp AgentDefinition
 * frontmatter contract; the OpenCode mode/permission/temperature fields
 * have no omp counterpart and are not accepted). Optional fields accept
 * `null` to clear.
 */
const validateDefinitionPatch = (patch: AgentDefinitionInput, allowedTools: Set<string>): AgentDefinitionPatch => {
  const out: AgentDefinitionPatch = {};
  if (patch.description !== undefined) {
    if (typeof patch.description !== 'string' || !patch.description.trim()) {
      throw new ModeDomainError(400, {
        error: 'invalid-description',
        message: 'description must be a non-empty string (required by the omp agent frontmatter)',
      });
    }
    out.description = patch.description;
  }
  if (patch.systemPrompt !== undefined) {
    if (typeof patch.systemPrompt !== 'string' || !patch.systemPrompt.trim()) {
      throw new ModeDomainError(400, { error: 'invalid-prompt', message: 'systemPrompt must be a non-empty string' });
    }
    out.systemPrompt = patch.systemPrompt;
  }
  if (patch.model !== undefined) {
    if (patch.model === null) {
      out.model = null;
    } else {
      const model = parseCsvList(patch.model);
      if (!Array.isArray(model) || model.length === 0
        || model.some((pattern) => typeof pattern !== 'string' || !pattern.trim())) {
        throw new ModeDomainError(400, {
          error: 'invalid-model',
          message: 'model must be an array of model patterns, e.g. ["@smol", "anthropic/*:high"]',
        });
      }
      out.model = model.map((pattern) => pattern.trim());
    }
  }
  if (patch.thinkingLevel !== undefined) {
    if (patch.thinkingLevel === null) {
      out.thinkingLevel = null;
    } else if (typeof patch.thinkingLevel !== 'string' || !THINKING_LEVELS.has(patch.thinkingLevel)) {
      throw new ModeDomainError(400, {
        error: 'invalid-thinking-level',
        thinkingLevel: patch.thinkingLevel,
        message: `thinkingLevel must be one of ${[...THINKING_LEVELS].join(', ')}`,
      });
    } else {
      out.thinkingLevel = patch.thinkingLevel;
    }
  }
  if (patch.tools !== undefined) {
    out.tools = patch.tools === null ? null : validateTools(patch.tools, allowedTools);
  }
  if (patch.spawns !== undefined) {
    if (patch.spawns === null) {
      out.spawns = null;
    } else {
      const spawns = patch.spawns === '*' ? '*' : parseCsvList(patch.spawns);
      if (spawns !== '*' && (!Array.isArray(spawns) || spawns.length === 0
        || spawns.some((name) => typeof name !== 'string' || !name.trim()))) {
        throw new ModeDomainError(400, {
          error: 'invalid-spawns',
          message: 'spawns must be "*" or an array of agent names',
        });
      }
      out.spawns = spawns === '*' ? '*' : spawns.map((name) => name.trim());
    }
  }
  if (patch.prewalk !== undefined) out.prewalk = validateFlagOrPattern('prewalk', patch.prewalk);
  if (patch.advisor !== undefined) out.advisor = validateFlagOrPattern('advisor', patch.advisor);
  if (patch.readSummarize !== undefined) {
    if (patch.readSummarize === null) {
      out.readSummarize = null;
    } else if (typeof patch.readSummarize !== 'boolean') {
      throw new ModeDomainError(400, { error: 'invalid-read-summarize', message: 'readSummarize must be a boolean' });
    } else {
      out.readSummarize = patch.readSummarize;
    }
  }
  return out;
};

const gateProjectScope = (scope: string | undefined, settingsProjectScopes?: boolean): 'user' | 'project' => {
  if (scope === undefined || scope === null) return 'user';
  if (scope !== 'user' && scope !== 'project') {
    throw new ModeDomainError(400, { error: 'invalid-scope', scope, message: 'scope must be "user" or "project"' });
  }
  if (scope !== 'project') return scope;
  if (settingsProjectScopes) return 'project';
  throw new ModeDomainError(409, {
    error: 'project-scope-unavailable',
    message: 'Project-scoped agent definitions require settings.projectScopes.v1; use scope "user".',
  });
};

/**
 * Effective task.* override values for one directory's sessions (the keyed
 * Settings merged view — 02 §5.2 read projection). Injected by the engine;
 * null/throw degrades to override-free definitions.
 */
export interface TaskOverrideValues {
  disabledAgents?: unknown;
  modelOverrides?: unknown;
  prewalk?: unknown;
  advisor?: unknown;
}

/** Effective task.* overrides read for one directory (engine-injected). */
export type OverridesFor = (directory: string | null) => Promise<TaskOverrideValues | null>;

const recordEntryFor = (
  name: string,
  value: TaskOverrideValues['modelOverrides'] | TaskOverrideValues['prewalk'] | TaskOverrideValues['advisor'],
): string | undefined => {
  if (!value || typeof value !== 'object' || Array.isArray(value)) return undefined;
  // SAFETY: plain-object overrides hold string fields (YAML discovery output);
  // only a confirmed string field is returned.
  const fields = value as Record<string, string>;
  return typeof fields[name] === 'string' ? fields[name] : undefined;
}

/** Join the settings-level per-agent overrides onto definition records (02 §5.2). */
const withTaskOverrides = async (records: OmpAgentRecord[], overridesFor: OverridesFor | undefined, directory: string | null): Promise<OmpAgentRecord[]> => {
  if (typeof overridesFor !== 'function') return records;
  let values;
  try {
    values = await overridesFor(directory);
  } catch {
    return records;
  }
  if (!values) return records;
  const disabled = Array.isArray(values.disabledAgents)
    ? new Set(values.disabledAgents.filter((name) => typeof name === 'string'))
    : new Set();
  return records.map((record) => {
    const modelOverride = recordEntryFor(record.name, values.modelOverrides);
    const prewalkOverride = recordEntryFor(record.name, values.prewalk);
    const advisorOverride = recordEntryFor(record.name, values.advisor);
    return {
      ...record,
      disabled: disabled.has(record.name),
      ...(modelOverride !== undefined ? { modelOverride } : {}),
      ...(prewalkOverride !== undefined ? { prewalkOverride } : {}),
      ...(advisorOverride !== undefined ? { advisorOverride } : {}),
    };
  });
};

/** Discovered omp agent (structural subset of the SDK `discoverAgents` record consumed here). */
export interface DiscoveredAgent {
  name: string;
  description?: string;
  source: string;
  filePath?: string;
  systemPrompt?: string;
  model?: string[];
  thinkingLevel?: string;
  tools?: string[];
  spawns?: '*' | string[];
  prewalk?: boolean | string;
  advisor?: boolean | string;
  readSummarize?: boolean;
}

/** `discoverAgents(directory)` result shape consumed by the handlers. */
export interface AgentDiscoveryResult {
  agents: DiscoveredAgent[];
  projectAgentsDir?: string | null;
}

/** omp agent record — the /omp/agent-definitions read projection (02 §5.2). */
export interface OmpAgentRecord {
  name: string;
  description: string;
  source: string;
  systemPrompt: string;
  filePath?: string;
  model?: string[];
  thinkingLevel?: string;
  tools?: string[];
  spawns?: '*' | string[];
  prewalk?: boolean | string;
  advisor?: boolean | string;
  readSummarize?: boolean;
  /** Settings-joined fields (withTaskOverrides). */
  disabled?: boolean;
  modelOverride?: string;
  prewalkOverride?: string;
  advisorOverride?: string;
}

/** AgentDefinition (SDK task/types.ts:359-378) → OmpAgent record (02 §5.2). */
const definitionToRecord = (agent: DiscoveredAgent): OmpAgentRecord => ({
  name: agent.name,
  description: typeof agent.description === 'string' ? agent.description : '',
  source: agent.source,
  ...(agent.filePath ? { filePath: agent.filePath } : {}),
  systemPrompt: typeof agent.systemPrompt === 'string' ? agent.systemPrompt : '',
  ...(Array.isArray(agent.model) && agent.model.length > 0 ? { model: agent.model } : {}),
  ...(agent.thinkingLevel !== undefined && agent.thinkingLevel !== null
    ? { thinkingLevel: String(agent.thinkingLevel) }
    : {}),
  ...(Array.isArray(agent.tools) && agent.tools.length > 0 ? { tools: agent.tools } : {}),
  ...(agent.spawns !== undefined && agent.spawns !== null ? { spawns: agent.spawns } : {}),
  ...(agent.prewalk !== undefined && agent.prewalk !== null ? { prewalk: agent.prewalk } : {}),
  ...(agent.advisor !== undefined && agent.advisor !== null ? { advisor: agent.advisor } : {}),
  ...(agent.readSummarize !== undefined && agent.readSummarize !== null
    ? { readSummarize: agent.readSummarize }
    : {}),
});

/** Definition input to serializeAgentMarkdown: name + description are required
 * by the SDK frontmatter parser; the rest ride the omp frontmatter fields.
 * `rawFrontmatter` is the update path's lossless carrier: the full parsed
 * frontmatter record of the file being rewritten — unknown keys (incl. the
 * SDK's autoloadSkills/blocking/output) survive verbatim, while whitelist
 * keys follow the merged values (undefined clears the key). The SDK has no
 * round-trip serializer and its read parser mutates on load (yield
 * injection, spawns inference), so raw-record preservation is the only
 * lossless write (plan P9). */
export interface AgentDefinitionSerialization {
  name: string;
  description: string;
  systemPrompt?: string;
  model?: string[];
  thinkingLevel?: string;
  tools?: string[];
  spawns?: '*' | string[];
  prewalk?: boolean | string;
  advisor?: boolean | string;
  readSummarize?: boolean;
  rawFrontmatter?: AgentFrontmatterRecord;
}

/** Frontmatter keys the whitelist merge owns; raw passthrough covers the rest. */
const SERIALIZED_MERGED_KEYS = ['tools', 'model', 'thinkingLevel', 'spawns', 'prewalk', 'advisor', 'readSummarize'] as const;

/** Parsed agent-frontmatter value: user-authored YAML, arbitrary shapes. */
export type AgentFrontmatterValue =
  | string
  | number
  | boolean
  | null
  | AgentFrontmatterValue[]
  | { [key: string]: AgentFrontmatterValue };

/** The whole parsed frontmatter record of an agent .md file. */
export type AgentFrontmatterRecord = { [key: string]: AgentFrontmatterValue };
/**
 * Serialize a definition to the omp agent markdown shape: YAML frontmatter
 * (name + description are required by the SDK parser,
 * discovery/helpers.ts:256-260 parseAgentFields) with the body as the
 * systemPrompt. Round-trips through `discoverAgents` (first-wins dedup,
 * discovery.ts) — the re-discovered record, not this string, is the
 * authority returned to clients.
 */
export function serializeAgentMarkdown(definition: AgentDefinitionSerialization): string {
  // Raw path (update): start from the file's verbatim frontmatter record so
  // unknown keys survive; whitelist keys follow the merged values, where
  // undefined/null means the patch cleared the key.
  const frontmatter: AgentFrontmatterRecord = definition.rawFrontmatter
    ? { ...definition.rawFrontmatter }
    : { name: definition.name, description: definition.description };
  for (const key of SERIALIZED_MERGED_KEYS) {
    const value = definition[key];
    if (!definition.rawFrontmatter) {
      if (value === undefined || value === null) continue;
      if (key === 'tools' || key === 'model') {
        if (Array.isArray(value) && value.length > 0) frontmatter[key] = value;
        continue;
      }
      frontmatter[key] = value;
      continue;
    }
    if (value === undefined || value === null) delete frontmatter[key];
    else frontmatter[key] = value;
  }
  frontmatter.name = definition.name;
  frontmatter.description = definition.description;
  const body = typeof definition.systemPrompt === 'string' ? definition.systemPrompt : '';
  return `---\n${stringifyYaml(frontmatter)}---\n\n${body}\n`;
}


/**
 * File/discovery adapters injected by the engine (02 §5.2): reads come from
 * the omp discovery chain, writes land as .md files, mutations hot-reload.
 */
export interface AgentDefinitionAdapter {
  discover: (directory: string | null) => Promise<AgentDiscoveryResult>;
  writeFile: (filePath: string, content: string) => Promise<void>;
  deleteFile: (filePath: string) => Promise<boolean>;
  /** Lossless-update reader (P9): fetches an existing definition file so its
   * raw frontmatter survives a GUI edit. Optional — without it updates keep
   * the whitelist-only write. */
  readFile?: (filePath: string) => Promise<string>;
  /** Hot-reload hook (refreshAgentDiscovery in the engine process). */
  onDefinitionsChanged?: (directory: string | null) => void | Promise<void>;
  /** Reveal the definition file in the platform file manager. */
  revealFile?: (filePath: string) => Promise<void>;
  userAgentsDir: string;
  projectAgentsDirFor: (directory: string) => string;
}

/** createAgentDefinitionHandlers options — the adapter plus table knobs. */
export interface AgentDefinitionHandlersOptions {
  discover?: (directory: string | null) => Promise<AgentDiscoveryResult>;
  writeFile?: (filePath: string, content: string) => Promise<void>;
  deleteFile?: (filePath: string) => Promise<boolean>;
  readFile?: (filePath: string) => Promise<string>;
  userAgentsDir?: string;
  projectAgentsDirFor?: (directory: string) => string;
  onDefinitionsChanged?: (directory: string | null) => void | Promise<void>;
  revealFile?: (filePath: string) => Promise<void>;
  allowedTools?: Set<string> | Iterable<string>;
  settingsProjectScopes?: boolean;
  overridesFor?: OverridesFor;
}

/** /omp/agent-definitions CRUD handlers (route-handler shaped). */
export interface AgentDefinitionHandlers {
  list: ModesRouteHandler;
  get: ModesRouteHandler;
  create: ModesRouteHandler;
  update: ModesRouteHandler;
  remove: ModesRouteHandler;
  refresh: ModesRouteHandler;
  reveal: ModesRouteHandler;
}

/**
 * CRUD handlers for /omp/agent-definitions (02 §5.2, GAP-B03/B04): the omp
 * agent discovery chain (project `.omp/agents` > user `~/.omp/agent/agents`
 * > extension packages > bundled — SDK task/discovery.ts `discoverAgents`)
 * is the read authority; writes land as agent markdown files in the user or
 * project agents dir. Bundled and extension/plugin-owned definitions are
 * read-only — overriding rides omp's first-wins shadowing (a same-name user
 * definition), never mutation.
 *
 * @param {AgentDefinitionHandlersOptions} options discovery + file adapters,
 *        plus the tool allowlist / project-scope gate / settings overrides.
 */
/**
 * Parse the leading YAML frontmatter record of an agent .md file (update
 * path, plan P9). Returns null when there is no reader, no file path, no
 * frontmatter block, or the YAML fails to parse — callers degrade to the
 * whitelist-only write.
 */
const readExistingFrontmatter = async (
  filePath: string | undefined,
  readFile: ((filePath: string) => Promise<string>) | undefined,
): Promise<AgentFrontmatterRecord | null> => {
  // A missing reader or path degrades to the whitelist-only write.
  if (!filePath || !readFile) return null;
  try {
    const content = await readFile(filePath);
    const match = content.match(/^---\r?\n([\s\S]*?)\r?\n---(?:\r?\n|$)/);
    if (!match) return null;
    const parsed = parseYaml(match[1]);
    // SAFETY: parseYaml returns unknown; the guards above establish a plain
    // non-array object, which is exactly AgentFrontmatterRecord's shape.
    return parsed && typeof parsed === 'object' && !Array.isArray(parsed)
      ? { ...(parsed as AgentFrontmatterRecord) }
      : null;
  } catch {
    return null;
  }
};

export function createAgentDefinitionHandlers({
  discover,
  writeFile,
  deleteFile,
  readFile,
  userAgentsDir,
  projectAgentsDirFor,
  allowedTools,
  settingsProjectScopes = false,
  overridesFor,
  onDefinitionsChanged,
  revealFile,
}: AgentDefinitionHandlersOptions = {}): AgentDefinitionHandlers {
  if (typeof discover !== 'function' || typeof writeFile !== 'function' || typeof deleteFile !== 'function'
    || typeof userAgentsDir !== 'string' || typeof projectAgentsDirFor !== 'function') {
    throw new TypeError('agent-definitions handlers require discovery + file adapters');
  }
  const allow = allowedTools instanceof Set ? allowedTools : new Set(allowedTools ?? Object.keys(BUILTIN_TOOLS ?? {}));

  /**
   * Hot-reload hook (02 §5.2 refresh): the SDK memoizes the create-time
   * discovery per cwd (task/index.ts discoveryMemo) and every task tool
   * advertises that list to the model. After a definition file changes,
   * refreshAgentDiscovery must run in the engine process or live sessions
   * keep describing the stale agent set. Swallowed failures never fail the
   * mutation — dispatch-time discovery stays fresh regardless.
   */
  const definitionsChanged = async (directory: string | null): Promise<void> => {
    if (typeof onDefinitionsChanged !== 'function') return;
    try {
      await onDefinitionsChanged(directory);
    } catch (error) {
      console.warn('[omp-host] agent discovery refresh failed:', errorText(error));
    }
  };

  const discoverSafe = async (directory: string | null): Promise<AgentDiscoveryResult> => {
    try {
      return await discover(directory);
    } catch (error) {
      throw new ModeDomainError(503, {
        error: 'agent-discovery-failed',
        message: errorText(error),
      });
    }
  };
  const findAgent = async (directory: string | null, name: string | undefined): Promise<DiscoveredAgent | null> => {
    const { agents } = await discoverSafe(directory);
    return agents.find((agent) => agent?.name === name) ?? null;
  };
  const recordFor = async (directory: string | null, name: string | undefined): Promise<OmpAgentRecord> => {
    const agent = await findAgent(directory, name);
    if (!agent) {
      throw new ModeDomainError(500, {
        error: 'definition-not-parsed',
        name,
        message: 'the definition was written but did not parse back through discovery',
      });
    }
    return definitionToRecord(agent);
  };
  const isManaged = (agent: DiscoveredAgent | null, directory: string | null): boolean => {
    if (!agent?.filePath) return false;
    const resolved = path.resolve(agent.filePath);
    return resolved.startsWith(path.resolve(userAgentsDir) + path.sep)
      || resolved.startsWith(path.resolve(projectAgentsDirFor(directory ?? process.cwd())) + path.sep);
  };
  const assertWritable = (agent: DiscoveredAgent, directory: string | null): void => {
    if (agent.source === 'bundled') {
      throw new ModeDomainError(409, {
        error: 'bundled-read-only',
        name: agent.name,
        message: 'Bundled agents are read-only. Create a definition with the same name in the user or project scope to shadow it.',
      });
    }
    if (!isManaged(agent, directory)) {
      throw new ModeDomainError(409, {
        error: 'definition-not-managed',
        name: agent.name,
        message: 'This definition is owned by an extension or plugin directory. Edit it at its source.',
      });
    }
  };
  const scopeDir = (scope: 'user' | 'project', directory: string | null): string =>
    (scope === 'project' ? projectAgentsDirFor(directory ?? process.cwd()) : userAgentsDir);
  const mergeDefinition = (existing: Partial<AgentDefinitionSerialization> & { name: string }, patch: AgentDefinitionPatch): AgentDefinitionSerialization => {
    const merged: AgentDefinitionSerialization = {
      name: existing.name,
      description: patch.description ?? existing.description ?? '',
      systemPrompt: patch.systemPrompt !== undefined ? patch.systemPrompt : existing.systemPrompt,
    };
    // SAFETY: the merge walks the seven declared override keys shared by
    // AgentDefinitionPatch and the serialized record; presence wins and the
    // key-union write needs the never-widened assignment TS demands.
    for (const key of AGENT_OVERRIDE_KEYS) {
      const value = patch[key] !== undefined ? patch[key] : existing[key];
      if (value !== null && value !== undefined) {
        // SAFETY: key-union write; value comes from the same field of
        // patch/existing, so the target field type already admits it.
        merged[key] = value as never;
      }
    }
    return merged;
  };

  return {
    async list(request, ctx) {
      const directory = directoryParam(ctx);
      const { agents, projectAgentsDir } = await discoverSafe(directory);
      return json({
        agents: await withTaskOverrides(agents.map(definitionToRecord), overridesFor, directory),
        projectAgentsDir: projectAgentsDir ?? null,
      });
    },

    async get(request, ctx) {
      const directory = directoryParam(ctx);
      const agent = await findAgent(directory, ctx?.params?.name);
      if (!agent) return json({ error: 'not-found' }, { status: 404 });
      const [joined] = await withTaskOverrides([definitionToRecord(agent)], overridesFor, directory);
      return json(joined);
    },

    async create(request, ctx) {
      const directory = directoryParam(ctx);
      const body = await readJsonBody<AgentDefinitionWriteBody>(request);
      const definition = body?.definition ?? body;
      const name = validateName(definition?.name);
      if (await findAgent(directory, name)) {
        throw new ModeDomainError(409, { error: 'agent-definition-exists', name });
      }
      const scope = gateProjectScope(definitionScope(body), settingsProjectScopes);
      const patch = validateDefinitionPatch(definition, allow);
      if (patch.systemPrompt === undefined) {
        throw new ModeDomainError(400, { error: 'invalid-prompt', message: 'systemPrompt is required' });
      }
      if (patch.description === undefined) {
        throw new ModeDomainError(400, { error: 'invalid-description', message: 'description is required' });
      }
      await writeFile(
        path.join(scopeDir(scope, directory), `${name}.md`),
        serializeAgentMarkdown(mergeDefinition({ name }, patch)),
      );
      const [joined] = await withTaskOverrides([await recordFor(directory, name)], overridesFor, directory);
      await definitionsChanged(directory);
      return json(joined, { status: 201 });
    },

    async update(request, ctx) {
      const directory = directoryParam(ctx);
      const name = ctx?.params?.name;
      const existing = await findAgent(directory, name);
      if (!existing) throw new ModeDomainError(404, { error: 'not-found' });
      assertWritable(existing, directory);
      const body = await readJsonBody<AgentDefinitionWriteBody>(request);
      const renameTo = body?.renameTo !== undefined ? validateName(body.renameTo, { label: 'renameTo' }) : undefined;
      if (renameTo !== undefined && renameTo !== name && await findAgent(directory, renameTo)) {
        throw new ModeDomainError(409, { error: 'agent-definition-exists', name: renameTo });
      }
      const patch = validateDefinitionPatch(body?.definition ?? {}, allow);
      const currentScope = existing.source === 'project' ? 'project' : 'user';
      const nextScope = gateProjectScope(definitionScope(body) ?? currentScope, settingsProjectScopes);
      const targetName = renameTo ?? name;
      const nextPath = path.join(scopeDir(nextScope, directory), `${targetName}.md`);
      // P9 lossless update: re-serialize from the existing file's verbatim
      // frontmatter so unknown keys survive the GUI edit. A missing reader
      // or unreadable file degrades to the whitelist-only write.
      const rawFrontmatter = await readExistingFrontmatter(existing.filePath, readFile);
      await writeFile(nextPath, serializeAgentMarkdown({
        ...mergeDefinition(existing, patch),
        name: targetName ?? existing.name,
        ...(rawFrontmatter ? { rawFrontmatter } : {}),
      }));
      if (existing.filePath && path.resolve(nextPath) !== path.resolve(existing.filePath)) {
        await deleteFile(existing.filePath);
      }
      const [joined] = await withTaskOverrides(
        [await recordFor(directory, targetName)],
        overridesFor,
        directory,
      );
      await definitionsChanged(directory);
      return json(joined);
    },

    async remove(request, ctx) {
      const directory = directoryParam(ctx);
      const name = ctx?.params?.name;
      const existing = await findAgent(directory, name);
      if (!existing) throw new ModeDomainError(404, { error: 'not-found' });
      assertWritable(existing, directory);
      if (!existing.filePath) throw new ModeDomainError(400, { error: 'not-file-backed', name });
      await deleteFile(existing.filePath);
      await definitionsChanged(directory);
      return new Response(null, { status: 204 });
    },

    /** POST /omp/agent-definitions/refresh (02 §5.2): out-of-band file edits. */
    async refresh(request, ctx) {
      const directory = directoryParam(ctx);
      await definitionsChanged(directory);
      return new Response(null, { status: 204 });
    },

    /** POST /omp/agent-definitions/{name}/reveal — open the definition file's folder. */
    async reveal(request, ctx) {
      const directory = directoryParam(ctx);
      const existing = await findAgent(directory, ctx?.params?.name);
      if (!existing) throw new ModeDomainError(404, { error: 'not-found' });
      if (existing.source === 'bundled') {
        throw new ModeDomainError(409, {
          error: 'bundled-read-only',
          name: existing.name,
          message: 'Bundled agents have no definition file. Create a user or project copy to customize it.',
        });
      }
      if (typeof revealFile !== 'function' || !isManaged(existing, directory)) {
        throw new ModeDomainError(404, {
          error: 'definition-not-managed',
          name: existing.name,
          message: 'This definition has no editable file in the user or project agents directory.',
        });
      }
      try {
        if (!existing.filePath) throw new ModeDomainError(400, { error: 'not-file-backed', name: existing.name });
        await revealFile(path.resolve(existing.filePath));
      } catch (error) {
        console.warn('[omp-host] failed to reveal agent definition:', errorText(error));
        return json({ error: 'reveal-failed' }, { status: 500 });
      }
      return json({ ok: true });
    },
  };
}

/** Legacy `ompchamber-agents.json` record (02 §6.2 migration source). */
export interface SidecarAgentRecord {
  name: string;
  description?: string;
  prompt?: string;
  tools?: string[];
}

/**
 * migrateSidecarAgents adapters (the engine wires the sidecar file, the omp
 * discovery chain and the personas map). All-optional matches the `= {}`
 * default — a missing adapter degrades per the retry contract below.
 */
export interface SidecarMigrationOptions {
  loadRecords: () => SidecarAgentRecord[];
  agentExists: (name: string) => Promise<boolean>;
  writeAgent: (record: SidecarAgentRecord) => Promise<void>;
  personaExists: (name: string) => boolean;
  mirrorPersona: (record: SidecarAgentRecord) => void;
  markDone: () => void;
  /** Diagnostic logger; `cause` is the caught error from the failed adapter call. */
  log?: (message: string, cause?: unknown) => void;
}

/** Sidecar migration outcome; `failed` names the record that stopped the run. */
export interface SidecarMigrationResult {
  migrated: number;
  skipped: number;
  failed?: string;
}

/**
 * One-time sidecar → omp migration (02 §6.2): every
 * `ompchamber-agents.json` record becomes a user-scope worker `.md` plus a
 * mirrored `OmpPersona`, so legacy `meta.agent` sessions keep resolving
 * (D-B2: the worker file and the top-level persona are separate resources).
 * A name that already exists in discovery is skipped (first-wins). Any
 * failure keeps the sidecar for an idempotent retry on the next boot.
 *
 * @param {SidecarMigrationOptions} options sidecar + discovery adapters.
 * @returns {Promise<SidecarMigrationResult>}
 */
export async function migrateSidecarAgents({
  loadRecords,
  agentExists,
  writeAgent,
  personaExists,
  mirrorPersona,
  markDone,
  log = () => {},
}: SidecarMigrationOptions): Promise<SidecarMigrationResult> {
  let records: SidecarAgentRecord[] = [];
  try {
    records = loadRecords();
  } catch (error) {
    log('sidecar read failed; leaving migration pending', error);
    return { migrated: 0, skipped: 0 };
  }
  if (!Array.isArray(records)) records = [];
  let migrated = 0;
  let skipped = 0;
  for (const record of records) {
    if (!record || typeof record.name !== 'string' || !record.name.trim()) {
      skipped += 1;
      continue;
    }
    try {
      if (await agentExists(record.name)) {
        skipped += 1;
      } else {
        await writeAgent(record);
      }
      if (!personaExists(record.name)) mirrorPersona(record);
      migrated += 1;
    } catch (error) {
      log(`sidecar migration stopped at "${record.name}"; keeping the sidecar for retry`, error);
      return { migrated, skipped, failed: record.name };
    }
  }
  markDone();
  return { migrated, skipped };
}

// ---------------------------------------------------------------------------
// 3. Personas (02 §5.2a, master D6-R12)
// ---------------------------------------------------------------------------

/** Unvalidated /omp/personas wire input (a `{ persona? }` wrapper or bare). */
export interface PersonaInput {
  name?: unknown;
  description?: unknown;
  systemPrompt?: unknown;
  tools?: unknown;
}

/** POST/PUT /omp/personas body: a `{ persona? }` wrapper or a bare patch. */
export interface PersonaWriteBody extends PersonaInput {
  persona?: PersonaInput;
}

/** Validated persona patch. */
export interface PersonaPatch {
  description?: string;
  systemPrompt?: string;
  tools?: string[];
}

/** createPersonaHandlers options. */
export interface PersonaHandlersOptions {
  store?: PersonaStore;
  allowedTools?: Set<string> | Iterable<string>;
}

/** /omp/personas CRUD handlers (route-handler shaped). */
export interface PersonaHandlers {
  list: ModesRouteHandler;
  get: ModesRouteHandler;
  create: ModesRouteHandler;
  update: ModesRouteHandler;
  remove: ModesRouteHandler;
}

/**
 * CRUD handlers for /omp/personas. `OmpPersona = { name, description?,
 * systemPrompt?, tools? }` — no model/thinkingLevel (top-level model choice
 * belongs to model roles, spec 02 §5.2a).
 */
export function createPersonaHandlers({ store, allowedTools }: PersonaHandlersOptions = {}): PersonaHandlers {
  if (!store?.load || !store?.save) throw new TypeError('persona handlers require a store');
  const allow = allowedTools instanceof Set ? allowedTools : new Set(allowedTools ?? Object.keys(BUILTIN_TOOLS ?? {}));

  const find = (name: string | undefined) => store.load().find((persona) => persona.name === name) ?? null;

  const validatePersonaPatch = (patch: PersonaInput): PersonaPatch => {
    const out: PersonaPatch = {};
    if (patch.description !== undefined) {
      if (typeof patch.description !== 'string') throw new ModeDomainError(400, { error: 'invalid-description' });
      out.description = patch.description;
    }
    if (patch.systemPrompt !== undefined) {
      if (typeof patch.systemPrompt !== 'string') throw new ModeDomainError(400, { error: 'invalid-system-prompt' });
      out.systemPrompt = patch.systemPrompt;
    }
    if (patch.tools !== undefined) out.tools = validateTools(patch.tools, allow);
    return out;
  };

  return {
    async list() {
      return json({ personas: store.load() });
    },

    async get(request, ctx) {
      const persona = find(ctx?.params?.name);
      return persona ? json(persona) : json({ error: 'not-found' }, { status: 404 });
    },

    async create(request) {
      const body = await readJsonBody<PersonaWriteBody>(request);
      const input = body?.persona ?? body;
      const name = validateName(input?.name);
      if (find(name)) throw new ModeDomainError(409, { error: 'persona-exists', name });
      const patch = validatePersonaPatch(input ?? {});
      const persona = {
        name,
        ...(patch.description !== undefined ? { description: patch.description } : {}),
        ...(patch.systemPrompt !== undefined ? { systemPrompt: patch.systemPrompt } : {}),
        ...(patch.tools !== undefined ? { tools: patch.tools } : {}),
      };
      store.save([...store.load(), persona]);
      return json(persona, { status: 201 });
    },

    async update(request, ctx) {
      const name = ctx?.params?.name;
      const existing = find(name);
      if (!existing) throw new ModeDomainError(404, { error: 'not-found' });
      const body = await readJsonBody<PersonaWriteBody>(request);
      const input = body?.persona ?? body ?? {};
      const renameTo = input.name !== undefined && input.name !== name
        ? validateName(input.name)
        : undefined;
      if (renameTo !== undefined && find(renameTo)) {
        throw new ModeDomainError(409, { error: 'persona-exists', name: renameTo });
      }
      const patch = validatePersonaPatch(input);
      const persona = { ...existing, ...patch, name: renameTo ?? existing.name };
      store.save(store.load().map((entry) => (entry.name === name ? persona : entry)));
      return json(persona);
    },

    async remove(request, ctx) {
      const name = ctx?.params?.name;
      if (!find(name)) throw new ModeDomainError(404, { error: 'not-found' });
      store.save(store.load().filter((persona) => persona.name !== name));
      return new Response(null, { status: 204 });
    },
  };
}

/** Session meta persona selection (legacy `meta.agent` participates, 02 §6.1). */
export interface PersonaMeta {
  persona?: string;
  agent?: string;
}

/** personaFor outcome (02 §5.1 D-B2): 'standard' = no overlay. */
export interface PersonaOverlay {
  status: 'standard' | 'active' | 'missing';
  name?: string;
  persona: { name: string; systemPrompt?: string; tools?: string[] } | null;
}

/**
 * Resolve the persona overlay for a session meta at materialize time
 * (02 §5.1 D-B2). Legacy `meta.agent` values participate per the migration
 * contract (02 §6.1): 'build'/'plan'/unset → standard; any other name is
 * treated as a persona name.
 *
 * @param {PersonaMeta | null} meta
 * @param {Iterable<OmpPersona> | Map<string, OmpPersona>} personas
 * @returns {PersonaOverlay}
 */
export function personaFor(meta: PersonaMeta | null, personas: Iterable<OmpPersona> | Map<string, OmpPersona>): PersonaOverlay {
  const byName = new Map();
  if (personas instanceof Map) {
    for (const [key, value] of personas) byName.set(key, value);
  } else if (personas && typeof personas[Symbol.iterator] === 'function') {
    for (const persona of personas) {
      if (persona && typeof persona.name === 'string') byName.set(persona.name, persona);
    }
  }
  const requested = meta?.persona
    ?? (meta?.agent === 'build' || meta?.agent === 'plan' || meta?.agent === undefined
      ? undefined
      : meta.agent);
  if (requested === undefined) return { status: 'standard', persona: null };
  const persona = byName.get(requested);
  if (!persona) return { status: 'missing', name: requested, persona: null };
  return {
    status: 'active',
    persona: {
      name: persona.name,
      ...(persona.systemPrompt !== undefined ? { systemPrompt: persona.systemPrompt } : {}),
      ...(Array.isArray(persona.tools) ? { tools: persona.tools } : {}),
    },
  };
}

// ---------------------------------------------------------------------------
// 4. Mode tracker (02 §5.4)
// ---------------------------------------------------------------------------

/** Mode enter payload (POST /omp/sessions/{id}/mode action=enter; per-mode fields). */
export interface ModeEnterData {
  /** plan */
  planFilePath?: string;
  hasDraftContent?: boolean;
  /** vibe */
  previousTools?: string[];
  /** goal */
  objective?: string;
  /** loop */
  count?: number;
  durationMs?: number;
  prompt?: string;
}

/** POST /omp/sessions/{id}/mode body: `{ action?, mode? }` + the enter payload. */
export interface ModeActionBody extends ModeEnterData {
  action?: unknown;
  mode?: string;
}

/**
 * Loop projection (02 §5.4): host-driver state, never persisted. An alias
 * (not an interface) so it satisfies the SDK's `Record<string, unknown>`
 * appendModeChange boundary structurally when riding {@link ModeChangeData}.
 */
export type ModeLoopState = {
  state: 'running' | 'paused';
  remaining?: number;
  limit?: number;
  prompt?: string;
};

/**
 * Per-mode payload appended/published on a mode transition (02 §5.4): the
 * fixed projection shapes — plan `{planFilePath}`, goal `{objective}`, vibe
 * `{previousTools}`, loop = {@link ModeLoopState}, prewalk `{target}` /
 * `{active:false}`, cold-start recovery `{recovered:true}`.
 */
export type ModeChangeData =
  | { planFilePath: string }
  | { objective: string }
  | { previousTools: string[] }
  | ModeLoopState
  | { target?: string }
  | { active: boolean }
  | { recovered: boolean };

/** omp goal payload passthrough (`goal_updated` events; status drives goal_paused). */
export interface ModeGoal {
  id?: string;
  status?: unknown;
}

/** GET /omp/sessions/{id}/mode payload (02 §5.4). */
export interface ModeSnapshot {
  mode: ModeValue;
  persona?: string;
  plan?: {
    planFilePath: string;
    paused: boolean;
    hasDraftContent: boolean;
    review?: PlanReviewDetails;
  };
  goal?: ModeGoal & { state?: unknown };
  loop?: ModeLoopState;
  prewalk?: { target?: string };
}

/**
 * buildSessionContext() recovery payload (SDK session-context.ts:280-282):
 * `{ mode, modeData }` from the last mode_change entry on the path.
 */
export interface ModeSessionContext {
  mode?: unknown;
  modeData?: ModeSessionContextData | null;
  /** Untagged thenable discrimination — trackerFor accepts sync or thenable. */
  then?: undefined;
}

/** modeData fields consumed by cold-start recovery (values pre-validation). */
export interface ModeSessionContextData {
  planFilePath?: unknown;
  hasDraftContent?: unknown;
  previousTools?: unknown;
  goal?: unknown;
}

/**
 * omp bus payloads this domain publishes (events.ts treats them as opaque):
 * `omp.mode.changed { mode, data? }` (02 §5.4) and
 * `omp.plan.review_requested { details }` (02 §5.5).
 */
export type OmpEventPayload =
  | { mode: string; data?: ModeChangeData }
  | { details: PlanReviewDetails | null };

/** omp bus publish binding (per-session OmpEventBus.publish, events.ts). */
export type OmpEventPublish = (
  type: string,
  payload: OmpEventPayload,
  options?: { durable?: boolean },
) => void;

/**
 * sessionManager.appendModeChange binding (SDK session-manager.ts:2179);
 * `data` is the per-mode entry payload this tracker appends (02 §5.4).
 */
export type ModeAppendEntry = (mode: string, data?: ModeChangeData) => string | undefined;

/** createModeTracker options. */
export interface ModeTrackerOptions {
  publish?: OmpEventPublish;
  appendEntry?: ModeAppendEntry;
  now?: () => number;
}

/** Per-session mode tracker (02 §5.4 state machine + projection). */
export interface ModeTracker {
  readonly mode: ModeValue;
  enterMode(mode: string, data?: ModeEnterData): ModeSnapshot;
  exitMode(options?: { paused?: boolean; persist?: boolean }): ModeSnapshot;
  pauseMode(mode?: string): ModeSnapshot;
  resumeMode(mode?: string): ModeSnapshot;
  applyGoalUpdate<T>(goal: ModeGoal | null | undefined, goalState?: T): ModeSnapshot;
  setPrewalk(active: boolean, options?: { target?: string }): ModeSnapshot;
  setPlanDraft(hasDraftContent: boolean): ModeSnapshot;
  setReview(review: PlanReviewDetails | null | undefined): ModeSnapshot;
  setPersona(persona: string | null | undefined): ModeSnapshot;
  recoverFromSessionContext(sessionContext: ModeSessionContext | null | undefined): ModeSnapshot;
  snapshot(): ModeSnapshot;
}

/** Internal state shape behind createModeTracker. */
interface ModeTrackerState {
  mode: ModeValue;
  persona: string | undefined;
  previousTools: string[] | undefined;
  plan: { planFilePath: string; hasDraftContent: boolean } | null;
  goal: { goal: ModeGoal; state: unknown } | null;
  loop: ModeLoopState | null;
  prewalk: { target?: string } | null;
  review: PlanReviewDetails | null;
}


/**
 * Per-session mode state machine.
 *
 * Persistence mirrors the TUI exactly:
 * - plan enter → appendModeChange('plan', { planFilePath })   (interactive-mode.ts:2751)
 * - plan pause/exit → 'plan_paused' / 'none'                  (interactive-mode.ts:2916)
 * - plan resume (from paused) → 'plan' + { planFilePath }     (interactive-mode.ts:3937-3938)
 * - vibe enter → 'vibe' + { previousTools }                   (interactive-mode.ts:3524)
 * - vibe exit → 'none'                                        (vibe/runtime.ts:630)
 * - goal enter/pause: NOT appended here — the SDK's GoalRuntime persist
 *   callback owns goal mode_change entries                    (agent-session.ts:1420-1426)
 * - goal exit → 'none' (TUI parity, interactive-mode.ts:2966) — pass
 *   `{ persist: false }` when the SDK already persisted the drop.
 * - loop: never persisted (the TUI never writes loop entries either).
 *
 * @param {ModeTrackerOptions} [options]
 */
export function createModeTracker({ publish, appendEntry, now = Date.now }: ModeTrackerOptions = {}): ModeTracker {
  const state: ModeTrackerState = {
    mode: 'none',
    persona: undefined,
    previousTools: undefined,
    plan: null,     // { planFilePath, hasDraftContent }
    goal: null,     // { goal, state }
    loop: null,     // { state, remaining?, limit?, prompt? }
    prewalk: null,  // { target? }
    review: null,   // PlanApprovalDetails — fed via setReview (review bridge)
  };

  const emit = (mode: string, data?: ModeChangeData) => {
    publish?.('omp.mode.changed', { mode, ...(data !== undefined ? { data } : {}) }, { durable: true });
  };
  const append = (mode: string, data?: ModeChangeData) => appendEntry?.(mode, data);

  const assertEntering = (entering: string): 'idempotent' | 'resume' | 'enter' => {
    const current = state.mode;
    if (current === entering || (entering === 'goal' && current === 'goal_paused')) return 'idempotent';
    if (entering === 'plan' && current === 'plan_paused') return 'resume';
    if (current === 'none') return 'enter';
    if (entering === 'goal' && current === 'loop') return 'enter';
    if (entering === 'vibe' && current === 'loop') return 'enter';
    if (entering === 'loop' && (current === 'goal' || current === 'goal_paused' || current === 'vibe')) return 'enter';
    throw modeConflict(current);
  };

  const enterPlan = (data: ModeEnterData = {}) => {
    const planFilePath = typeof data.planFilePath === 'string' && data.planFilePath
      ? data.planFilePath
      : DEFAULT_PLAN_FILE_PATH;
    state.mode = 'plan';
    state.plan = { planFilePath, hasDraftContent: Boolean(data.hasDraftContent) };
    if (Array.isArray(data.previousTools)) state.previousTools = data.previousTools;
    append('plan', { planFilePath });
    emit('plan', { planFilePath });
  };

  const enterGoal = (data: ModeEnterData = {}) => {
    state.mode = 'goal';
    // Goal mode_change persistence is owned by the SDK GoalRuntime
    // (agent-session.ts:1420-1426) — appending here would duplicate entries.
    emit('goal', typeof data.objective === 'string' && data.objective ? { objective: data.objective } : undefined);
  };

  const enterVibe = (data: ModeEnterData = {}) => {
    if (!Array.isArray(data.previousTools)) {
      throw new ModeDomainError(400, {
        error: 'vibe-requires-previous-tools',
        message: 'vibe enter requires previousTools (the captured toolset to restore on exit)',
      });
    }
    state.mode = 'vibe';
    state.previousTools = data.previousTools;
    append('vibe', { previousTools: data.previousTools });
    emit('vibe', { previousTools: data.previousTools });
  };

  const enterLoop = (data: ModeEnterData = {}) => {
    const count = data.count === undefined ? undefined : Number(data.count);
    const durationMs = data.durationMs === undefined ? undefined : Number(data.durationMs);
    if (count !== undefined && (!Number.isInteger(count) || count < 1)) {
      throw new ModeDomainError(400, { error: 'invalid-loop-count', count: data.count });
    }
    if (durationMs !== undefined && (!Number.isInteger(durationMs) || durationMs < 1)) {
      throw new ModeDomainError(400, { error: 'invalid-loop-duration', durationMs: data.durationMs });
    }
    state.mode = 'loop';
    state.loop = {
      state: 'running',
      ...(count !== undefined ? { remaining: count } : {}),
      limit: count ?? durationMs,
      ...(typeof data.prompt === 'string' && data.prompt ? { prompt: data.prompt } : {}),
    };
    // Loop is host-driver state; the TUI never persists loop mode entries.
    emit('loop', { ...state.loop });
  };

  const tracker: ModeTracker = {
    /** Current projection value (02 §5.4 mode set). */
    get mode() {
      return state.mode;
    },

    /**
     * Enter a mode. `mode` ∈ plan | goal | vibe | loop. Same-mode re-enter is
     * an idempotent no-op; plan enter from plan_paused resumes.
     */
    enterMode(mode: string, data: ModeEnterData = {}) {
      if (!MODE_VALUES.includes(mode) || mode === 'none') {
        throw new ModeDomainError(400, { error: 'invalid-mode', mode });
      }
      const kind = assertEntering(mode);
      if (kind === 'idempotent') return tracker.snapshot();
      if (mode === 'plan') enterPlan(data);
      else if (mode === 'goal') enterGoal(data);
      else if (mode === 'vibe') enterVibe(data);
      else enterLoop(data);
      return tracker.snapshot();
    },

    /**
     * Exit the active mode to 'none'. `{ paused: true }` from plan records
     * plan_paused instead (TUI three-state toggle). `{ persist: false }`
     * skips the mode_change append when the SDK already persisted the exit.
     */
    exitMode({ paused = false, persist = true }: { paused?: boolean; persist?: boolean } = {}) {
      const from = state.mode;
      if (from === 'none') return tracker.snapshot();
      if (from === 'plan') {
        state.mode = paused ? 'plan_paused' : 'none';
        if (!paused) state.plan = null;
        if (persist) append(paused ? 'plan_paused' : 'none');
        emit(state.mode);
        return tracker.snapshot();
      }
      if (from === 'plan_paused') {
        state.mode = 'none';
        state.plan = null;
        if (persist) append('none');
        emit('none');
        return tracker.snapshot();
      }
      if (from === 'goal' || from === 'goal_paused') {
        state.mode = 'none';
        state.goal = null;
        if (persist) append('none');
        emit('none');
        return tracker.snapshot();
      }
      if (from === 'vibe') {
        state.mode = 'none';
        if (persist) append('none');
        emit('none');
        return tracker.snapshot();
      }
      // loop
      state.mode = 'none';
      state.loop = null;
      emit('none');
      return tracker.snapshot();
    },

    /** action=pause: plan → plan_paused, goal → goal_paused, loop → paused. */
    pauseMode(mode?: string) {
      const target = mode ?? conflictFor(state.mode);
      if (target === 'plan') {
        if (state.mode === 'plan_paused') {
          throw new ModeDomainError(400, { error: 'already-paused', mode: 'plan' });
        }
        if (state.mode !== 'plan') throw new ModeDomainError(400, { error: 'not-active', mode: 'plan' });
        return tracker.exitMode({ paused: true });
      }
      if (target === 'goal') {
        if (state.mode === 'goal_paused') {
          throw new ModeDomainError(400, { error: 'already-paused', mode: 'goal' });
        }
        if (state.mode !== 'goal') throw new ModeDomainError(400, { error: 'not-active', mode: 'goal' });
        state.mode = 'goal_paused';
        // SDK GoalRuntime owns goal pause entries (agent-session.ts:1420-1426).
        emit('goal_paused');
        return tracker.snapshot();
      }
      if (target === 'loop') {
        if (state.mode !== 'loop' || !state.loop) throw new ModeDomainError(400, { error: 'not-active', mode: 'loop' });
        state.loop = { ...state.loop, state: 'paused' };
        emit('loop', { ...state.loop });
        return tracker.snapshot();
      }
      throw new ModeDomainError(400, { error: 'invalid-mode', mode: target });
    },

    /** action=resume: plan_paused → plan, goal_paused → goal, loop paused → running. */
    resumeMode(mode?: string) {
      const target = mode ?? conflictFor(state.mode);
      if (target === 'plan') {
        if (state.mode === 'plan') throw new ModeDomainError(400, { error: 'not-paused', mode: 'plan' });
        if (state.mode !== 'plan_paused') throw new ModeDomainError(400, { error: 'not-active', mode: 'plan' });
        return tracker.enterMode('plan', { planFilePath: state.plan?.planFilePath });
      }
      if (target === 'goal') {
        if (state.mode === 'goal') throw new ModeDomainError(400, { error: 'not-paused', mode: 'goal' });
        if (state.mode !== 'goal_paused') throw new ModeDomainError(400, { error: 'not-active', mode: 'goal' });
        state.mode = 'goal';
        // SDK GoalRuntime owns goal resume entries.
        emit('goal');
        return tracker.snapshot();
      }
      if (target === 'loop') {
        if (state.mode !== 'loop' || !state.loop) throw new ModeDomainError(400, { error: 'not-active', mode: 'loop' });
        state.loop = { ...state.loop, state: 'running' };
        emit('loop', { ...state.loop });
        return tracker.snapshot();
      }
      throw new ModeDomainError(400, { error: 'invalid-mode', mode: target });
    },

    /**
     * Consume a goal_updated event (the omp.goal.updated publish lives in
     * engine.js, Wave 0). Derives goal_paused from goal.status while a goal
     * mode is active (the projection value set, 02 §5.4).
     */
    applyGoalUpdate<T>(goal: ModeGoal | null | undefined, goalState?: T) {
      state.goal = goal === null || goal === undefined ? null : { goal, state: goalState };
      if (state.mode === 'goal' || state.mode === 'goal_paused') {
        const next = goal?.status === 'paused' ? 'goal_paused' : 'goal';
        if (next !== state.mode) {
          state.mode = next;
          emit(next);
        }
      }
      return tracker.snapshot();
    },

    /**
     * Prewalk status bit (02 §5.7): orthogonal to the exclusive mode set.
     * Arming publishes `omp.mode.changed {mode:'prewalk', data:{target}}`
     * (spec projection); disarming publishes `data:{active:false}`.
     */
    setPrewalk(active: boolean, { target }: { target?: string } = {}) {
      if (active) {
        state.prewalk = target !== undefined && target !== null ? { target } : {};
        emit('prewalk', { ...(target !== undefined && target !== null ? { target } : {}) });
      } else {
        state.prewalk = null;
        emit('prewalk', { active: false });
      }
      return tracker.snapshot();
    },

    /** Plan draft content bit — fed by the plan-file write path (04 domain). */
    setPlanDraft(hasDraftContent: boolean) {
      if (state.plan) state.plan = { ...state.plan, hasDraftContent: Boolean(hasDraftContent) };
      return tracker.snapshot();
    },

    /** Review state — fed by the plan review bridge (onReview wiring). */
    setReview(review: PlanReviewDetails | null | undefined) {
      state.review = review && typeof review === 'object' ? review : null;
      return tracker.snapshot();
    },

    setPersona(persona: string | null | undefined) {
      state.persona = persona === undefined || persona === null ? undefined : String(persona);
      return tracker.snapshot();
    },

    /**
     * Cold-start recovery (02 §5.4): consume the SDK SessionManager's
     * buildSessionContext() result — `{ mode, modeData }` from the last
     * mode_change entry on the path (session-context.ts:280-282) — restore the
     * projection, and publish omp.mode.changed once. Never appends.
     */
    recoverFromSessionContext(sessionContext: ModeSessionContext | null | undefined) {
      const persisted = sessionContext?.mode;
      const data = sessionContext?.modeData;
      if (persisted === 'plan' || persisted === 'plan_paused') {
        state.mode = persisted;
        state.plan = {
          planFilePath: typeof data?.planFilePath === 'string' && data.planFilePath
            ? data.planFilePath
            : DEFAULT_PLAN_FILE_PATH,
          hasDraftContent: Boolean(data?.hasDraftContent),
        };
      } else if (persisted === 'goal' || persisted === 'goal_paused') {
        state.mode = persisted;
        if (data?.goal && typeof data.goal === 'object') state.goal = { goal: data.goal, state: undefined };
      } else if (persisted === 'vibe') {
        state.mode = 'vibe';
        if (Array.isArray(data?.previousTools)) state.previousTools = data.previousTools;
      } else {
        state.mode = 'none';
      }
      emit(state.mode, state.mode === 'none' ? undefined : { recovered: true });
      return tracker.snapshot();
    },

    /** GET /api/omp/sessions/{id}/mode payload (02 §5.4). */
    snapshot(): ModeSnapshot {
      const out: ModeSnapshot = { mode: state.mode };
      if (state.persona !== undefined) out.persona = state.persona;
      if (state.mode === 'plan' || state.mode === 'plan_paused') {
        out.plan = {
          planFilePath: state.plan?.planFilePath ?? DEFAULT_PLAN_FILE_PATH,
          paused: state.mode === 'plan_paused',
          hasDraftContent: Boolean(state.plan?.hasDraftContent),
          ...(state.review ? { review: state.review } : {}),
        };
      }
      if ((state.mode === 'goal' || state.mode === 'goal_paused') && state.goal) {
        out.goal = { ...state.goal.goal, ...(state.goal.state !== undefined ? { state: state.goal.state } : {}) };
      }
      if (state.mode === 'loop' && state.loop) out.loop = { ...state.loop };
      if (state.prewalk) out.prewalk = { ...state.prewalk };
      return out;
    },
  };

  return tracker;
}

// ---------------------------------------------------------------------------
// 5. Plan review bridge (02 §5.5)
// ---------------------------------------------------------------------------

/** Plan review details payload (SDK preparePlanForReview `details`; agent-session.ts:933-948). */
export interface PlanReviewDetails {
  planFilePath?: string;
  title?: string;
  planExists?: boolean;
}

/** preparePlanForReview result (content passes through opaquely). */
export interface PreparePlanReviewResult {
  content: unknown[];
  details: PlanReviewDetails;
}

/** AgentSession surface consumed by the bridge. */
export interface PlanProposalSession {
  preparePlanForReview: (title: string) => Promise<PreparePlanReviewResult>;
}

/** POST /omp/sessions/{id}/plan/review body (02 §5.5 step 5). */
export interface PlanReviewDecisionInput {
  choice?: string;
  feedback?: unknown;
  executionRole?: unknown;
  editedContent?: unknown;
}

/** Decision echo settled through the propose tool result (02 §5.5). */
export interface PlanReviewDecisionDetails {
  choice: string;
  planFilePath?: string;
  feedback?: unknown;
  executionRole?: unknown;
  editedContent?: unknown;
}

/** decide() outcome: refine keeps the turn in planning (`dispatched:false`). */
export interface PlanReviewDecisionResult {
  dispatched: boolean;
  decision: PlanReviewDecisionInput | null;
  reason?: string;
}

/** Tool result returned by the xd://propose hook (content passes through). */
export interface PlanReviewToolResult {
  content: unknown[];
  details?: PlanReviewDetails | PlanReviewDecisionDetails;
}

/** GET /omp/sessions/{id}/plan payload fragment (02 §5.5 step 7). */
export interface PlanReviewSnapshot {
  planFilePath: string;
  review?: PlanReviewDetails;
}

/** Pending propose held until a review decision (02 §5.5 step 4). */
interface PlanReviewPending {
  resolve: (result: PlanReviewToolResult) => void;
  details: PlanReviewDetails | null;
  requestedAt: number;
}

/** planReviewBridge internal state. */
interface PlanReviewBridgeState {
  prepareRef: ((title: string) => Promise<PreparePlanReviewResult>) | null;
  planFilePath: string | null;
  review: PlanReviewDetails | null;
  pending: PlanReviewPending | null;
  decision: PlanReviewDecisionInput | null;
  disposed: boolean;
}

/** planReviewBridge options. */
export interface PlanReviewBridgeOptions {
  publish?: OmpEventPublish;
  prepare?: (title: string) => Promise<PreparePlanReviewResult>;
  onReview?: (details: PlanReviewDetails | null) => void;
  now?: () => number;
}

/** Bridge for the `xd://propose` tool hook (02 §5.5 step 3). */
export interface PlanReviewBridge {
  hook: (title: string) => Promise<PlanReviewToolResult>;
  hookFor: (session: PlanProposalSession) => (title: string) => Promise<PlanReviewToolResult>;
  decide: (input?: PlanReviewDecisionInput) => PlanReviewDecisionResult;
  snapshot: () => PlanReviewSnapshot;
  clear: () => PlanReviewSnapshot;
  dispose: (reason?: string) => void;
}


const SUPERSEDED_RESULT: PlanReviewToolResult = {
  content: [{ type: 'text', text: 'Plan review superseded by a newer proposal.' }],
};

/**
 * Bridge for the `xd://propose` tool hook (02 §5.5 step 3).
 *
 * Verified SDK surface: `session.setPlanProposalHandler(handler)` installs the
 * handler `xd://propose` dispatches the written plan title to
 * (tools/resolve.ts:109-110, agent-session.ts:1726-1735); the TUI attaches
 * `title => session.preparePlanForReview(title)` (interactive-mode.ts:2739),
 * which validates the plan artifact and returns
 * `{ content:[{type:'text',text:'Plan ready for review.'}], details:
 * { planFilePath, title, planExists } }` (agent-session.ts:933-948).
 *
 * This bridge wraps that handler: it publishes
 * `omp.plan.review_requested {details}` (durable) and holds the tool result
 * pending until a review decision arrives (`decide`), mirroring the TUI
 * overlay's blocking semantics without the TUI's turn abort.
 *
 * @param {PlanReviewBridgeOptions} [options]
 */
export function planReviewBridge({ publish, prepare, onReview, now = Date.now }: PlanReviewBridgeOptions = {}): PlanReviewBridge {
  const state: PlanReviewBridgeState = {
    prepareRef: prepare ?? null,
    planFilePath: null,
    review: null,   // PlanApprovalDetails of the latest propose
    pending: null,  // { resolve, details, requestedAt }
    decision: null, // last decide() input
    disposed: false,
  };

  const settle = (result: PlanReviewToolResult) => {
    const pending = state.pending;
    state.pending = null;
    pending?.resolve(result);
  };

  const bridge: PlanReviewBridge = {
    /**
     * The xd://propose hook. Attach with
     * `session.setPlanProposalHandler(bridge.hook)`; `bridge.hookFor(session)`
     * returns it bound to that session's preparePlanForReview.
     */
    hook: async (title: string): Promise<PlanReviewToolResult> => {
      if (!state.prepareRef) {
        throw new ModeDomainError(400, {
          error: 'plan-review-not-bound',
          message: 'planReviewBridge has no prepare binding; use hookFor(session).',
        });
      }
      const result = await state.prepareRef(title);
      // dispose() may have run while prepare was in flight — never strand a
      // fresh pending promise on a torn-down bridge.
      if (state.disposed) {
        return { content: [{ type: 'text', text: 'Plan review aborted: bridge disposed.' }] };
      }
      const details = result?.details ?? null;
      settle(SUPERSEDED_RESULT);
      state.review = details;
      if (details?.planFilePath) state.planFilePath = details.planFilePath;
      onReview?.(details);
      publish?.('omp.plan.review_requested', { details }, { durable: true });
      return await new Promise((resolve) => {
        state.pending = { resolve, details, requestedAt: now() };
      });
    },

    /** Bind the hook to a live AgentSession's preparePlanForReview. */
    hookFor: (session: PlanProposalSession) => {
      if (typeof session?.preparePlanForReview !== 'function') {
        throw new TypeError('planReviewBridge.hookFor requires an AgentSession with preparePlanForReview');
      }
      state.prepareRef = (title) => session.preparePlanForReview(title);
      return bridge.hook;
    },

    /**
     * Settle the pending proposal with a review decision
     * (POST /omp/sessions/{id}/plan/review body, 02 §5.5 step 5).
     * Returns `{ dispatched, decision }` — refine keeps the turn in planning
     * (`dispatched:false`) and the engine re-prompts with `feedback`.
     */
    decide: (input?: PlanReviewDecisionInput): PlanReviewDecisionResult => {
      const choice = input?.choice;
      if (typeof choice !== 'string' || !PLAN_REVIEW_CHOICES.includes(choice)) {
        throw new ModeDomainError(400, { error: 'invalid-choice', choice, choices: [...PLAN_REVIEW_CHOICES] });
      }
      state.decision = { ...input };
      if (!state.pending) {
        return { dispatched: false, decision: state.decision, reason: 'no-pending-proposal' };
      }
      const details = state.pending.details;
      if (choice === 'refine') {
        settle({
          content: [{
            type: 'text',
            text: `Plan refinement requested. Update the plan file, then write ${details?.title ?? 'the plan title'} to xd://propose again when ready.`,
          }],
          details: { choice, ...(input?.feedback !== undefined ? { feedback: input.feedback } : {}) },
        });
        return { dispatched: false, decision: state.decision };
      }
      settle({
        content: [{ type: 'text', text: `Plan approved (${choice}).` }],
        details: {
          choice,
          planFilePath: details?.planFilePath,
          ...(input?.executionRole !== undefined ? { executionRole: input.executionRole } : {}),
          ...(input?.editedContent !== undefined ? { editedContent: input.editedContent } : {}),
        },
      });
      return { dispatched: true, decision: state.decision };
    },

    /** GET /omp/sessions/{id}/plan payload fragment (02 §5.5 step 7). */
    snapshot: (): PlanReviewSnapshot => {
      return {
        planFilePath: state.planFilePath ?? DEFAULT_PLAN_FILE_PATH,
        ...(state.review ? { review: state.review } : {}),
      };
    },

    /** Drop review state (plan exit). Any pending propose settles superseded. */
    clear: (): PlanReviewSnapshot => {
      settle(SUPERSEDED_RESULT);
      state.review = null;
      state.decision = null;
      onReview?.(null);
      return bridge.snapshot();
    },

    /** Session teardown: settle the pending propose with an abort notice. */
    dispose: (reason = 'session disposed'): void => {
      state.disposed = true;
      settle({
        content: [{ type: 'text', text: `Plan review aborted: ${reason}.` }],
      });
      state.pending = null;
    },
  };

  return bridge;
}

// ---------------------------------------------------------------------------
// 6. Modes domain + route mounting
// ---------------------------------------------------------------------------

const directoryParam = (ctx?: ModesRouteContext): string | null => {
  const fromQuery = ctx?.url?.searchParams?.get('directory');
  const fromHeader = ctx?.headers?.get?.('x-opencode-directory');
  const raw = fromQuery ?? (fromHeader ? decodeURIComponent(fromHeader) : null);
  return raw ? normalizeDirectoryKey(raw) : null;
};

// Domain errors map 1:1 to their HTTP status; anything else is a bug and
// rethrows so the host's route wrapper answers a 500 (host.js:84-92).
const toResponse = <E>(error: E): Response => {
  if (error instanceof ModeDomainError) return json(error.body, { status: error.status });
  throw error;
};

/** Per-session bindings injected by the engine (spec 02 §5.1/§5.4). */
export interface ModesDomainDeps {
  /** omp bus publish for that session (engine binds #ompPublish). */
  publishFor?: (sessionId: string, directory: string) => OmpEventPublish;
  /** `(mode, data)` appending through that session's sessionManager.appendModeChange. */
  appendFor?: (sessionId: string, directory: string) => ModeAppendEntry;
  /** buildSessionContext() (sync or thenable) for cold-start recovery. */
  sessionContextFor?: (
    sessionId: string,
    directory: string,
  ) => ModeSessionContext | PromiseLike<ModeSessionContext> | null | undefined;
  /** omp agent discovery chain + .md write adapters (02 §5.2). */
  agentDefinitions?: AgentDefinitionAdapter;
  personasStore?: PersonaStore;
  allowedTools?: Set<string> | Iterable<string>;
  settingsProjectScopes?: boolean;
  overridesFor?: OverridesFor;
}

/** Modes/plan/goal/personas/agent-definitions domain object (spec 02). */
export interface ModesDomain {
  trackerFor: (sessionId: string, directory: string | null) => ModeTracker;
  bridgeFor: (sessionId: string, directory: string | null) => PlanReviewBridge;
  release: (sessionId: string, directory: string | null) => void;
  agentDefinitions: AgentDefinitionHandlers | null;
  personas: PersonaHandlers | null;
  register: (route: ModesRouteMount, options?: ModesRouteRegistrationOptions) => void;
}

/** registerModesDomainRoutes options. */
export interface ModesRouteRegistrationOptions {
  features?: Record<string, boolean>;
}

/**
 * Domain object owning per-session mode trackers and plan review bridges.
 * All engine state reaches it through injected bindings:
 * - `publishFor(sessionId, directory)` → omp bus publish for that session
 *   (engine binds `#ompPublish`).
 * - `appendFor(sessionId, directory)` → `(mode, data)` appending through that
 *   session's `sessionManager.appendModeChange` (SDK session-manager.ts:2179).
 * - `sessionContextFor(sessionId, directory)` → `buildSessionContext()` result
 *   (sync or thenable) for cold-start recovery on first access.
 */
export function createModesDomain({
  publishFor,
  appendFor,
  sessionContextFor,
  agentDefinitions: agentDefinitionsOptions,
  personasStore,
  allowedTools,
  settingsProjectScopes = false,
  overridesFor,
}: ModesDomainDeps = {}): ModesDomain {
  const trackers = new Map<string, ModeTracker>();
  const bridges = new Map<string, PlanReviewBridge>();
  const keyOf = (sessionId: string, directory: string | null): string => `${normalizeDirectoryKey(directory)}${sessionId}`;

  const trackerFor = (sessionId: string, directory: string | null): ModeTracker => {
    const key = keyOf(sessionId, directory);
    const existing = trackers.get(key);
    if (existing) return existing;
    const directoryKey = normalizeDirectoryKey(directory);
    const tracker = createModeTracker({
      publish: publishFor?.(sessionId, directoryKey),
      appendEntry: appendFor?.(sessionId, directoryKey),
    });
    trackers.set(key, tracker);
    if (sessionContextFor) {
      const context = sessionContextFor(sessionId, directoryKey);
      const recover = (value: ModeSessionContext | PromiseLike<ModeSessionContext> | null | undefined) => {
        if (value && typeof value === 'object' && typeof value.then !== 'function') {
          tracker.recoverFromSessionContext(value);
        }
      };
      if (context && typeof context.then === 'function') context.then(recover, () => {});
      else recover(context);
    }
    return tracker;
  };

  const bridgeFor = (sessionId: string, directory: string | null): PlanReviewBridge => {
    const key = keyOf(sessionId, directory);
    const existing = bridges.get(key);
    if (existing) return existing;
    const directoryKey = normalizeDirectoryKey(directory);
    const tracker = trackerFor(sessionId, directory);
    const bridge = planReviewBridge({
      publish: publishFor?.(sessionId, directoryKey),
      onReview: (details) => tracker.setReview(details),
    });
    bridges.set(key, bridge);
    return bridge;
  };

  const release = (sessionId: string, directory: string | null): void => {
    const key = keyOf(sessionId, directory);
    bridges.get(key)?.dispose('session released');
    bridges.delete(key);
    trackers.delete(key);
  };

  const agentDefinitions = agentDefinitionsOptions
    ? createAgentDefinitionHandlers({
      ...agentDefinitionsOptions,
      allowedTools,
      settingsProjectScopes,
      overridesFor,
    })
    : null;
  const personas = personasStore
    ? createPersonaHandlers({ store: personasStore, allowedTools })
    : null;

  const domain: ModesDomain = {
    trackerFor,
    bridgeFor,
    release,
    agentDefinitions,
    personas,
    register(route: ModesRouteMount, options?: ModesRouteRegistrationOptions) {
      return registerModesDomainRoutes(route, domain, options);
    },
  };
  return domain;
}

/**
 * Mount the /omp routes owned by this domain (public paths are /api/omp/...;
 * the web proxy strips /api). Each group is gated by its capability key —
 * a `false`/missing key answers an explicit 501 so clients fail loudly
 * (master R2; omp-parity.js featureUnavailable).
 *
 * @param {ModesRouteMount} route
 * @param {ModesDomain} domain
 * @param {ModesRouteRegistrationOptions} [options]
 */
export function registerModesDomainRoutes(
  route: ModesRouteMount,
  domain: ModesDomain,
  { features = ompFeatures() }: ModesRouteRegistrationOptions = {},
): void {
  const gated = (featureKey: string, handler: ModesRouteHandler): ModesRouteHandler =>
    async (request, ctx) =>
      features?.[featureKey] === true ? handler(request, ctx) : featureUnavailable(featureKey);

  // ---- sessions/{id}/mode (modes.v1) ----
  route('GET', '/omp/sessions/{id}/mode', gated('modes.v1', async (request, ctx) => {
    const directory = directoryParam(ctx);
    if (!directory) return badRequest('directory is required');
    return json(domain.trackerFor(ctx?.params?.id ?? '', directory).snapshot());
  }));

  route('POST', '/omp/sessions/{id}/mode', gated('modes.v1', async (request, ctx) => {
    const directory = directoryParam(ctx);
    if (!directory) return badRequest('directory is required');
    const body = await readJsonBody<ModeActionBody>(request);
    const tracker = domain.trackerFor(ctx?.params?.id ?? '', directory);
    const action = typeof body?.action === 'string' && body.action
      ? body.action
      : body?.mode === 'none' ? 'exit' : 'enter';
    try {
      if (action === 'enter') tracker.enterMode(typeof body?.mode === 'string' ? body.mode : '', body ?? {});
      else if (action === 'exit') tracker.exitMode();
      else if (action === 'pause') tracker.pauseMode(body?.mode);
      else if (action === 'resume') tracker.resumeMode(body?.mode);
      else return badRequest(`invalid action "${action}"`);
    } catch (error) {
      return toResponse(error);
    }
    return json(tracker.snapshot());
  }));

  // ---- sessions/{id}/plan (modes.v1) ----
  route('GET', '/omp/sessions/{id}/plan', gated('modes.v1', async (request, ctx) => {
    const directory = directoryParam(ctx);
    if (!directory) return badRequest('directory is required');
    const tracker = domain.trackerFor(ctx?.params?.id ?? '', directory);
    const bridge = domain.bridgeFor(ctx?.params?.id ?? '', directory);
    const snapshot = tracker.snapshot();
    const bridgeSnapshot = bridge.snapshot();
    const review = snapshot.plan?.review ?? bridgeSnapshot.review ?? null;
    const planActive = snapshot.mode === 'plan' || snapshot.mode === 'plan_paused';
    if (!planActive && !review) {
      return json({ error: 'plan-mode-inactive' }, { status: 404 });
    }
    // The reviewed plan outranks the mode-state path (TUI handlePlanApproval
    // promotes details.planFilePath, interactive-mode.ts:3982-3983).
    return json({
      planFilePath: review?.planFilePath ?? snapshot.plan?.planFilePath ?? bridgeSnapshot.planFilePath,
      ...(review ? { review } : {}),
    });
  }));

  route('POST', '/omp/sessions/{id}/plan/review', gated('modes.v1', async (request, ctx) => {
    const directory = directoryParam(ctx);
    if (!directory) return badRequest('directory is required');
    const body = await readJsonBody<PlanReviewDecisionInput>(request);
    const tracker = domain.trackerFor(ctx?.params?.id ?? '', directory);
    const bridge = domain.bridgeFor(ctx?.params?.id ?? '', directory);
    try {
      const result = bridge.decide(body ?? {});
      return json({ dispatched: result.dispatched, mode: tracker.snapshot().mode });
    } catch (error) {
      return toResponse(error);
    }
  }));

  // ---- agent-definitions (agentDefinitions.v1) ----
  const agentHandlers = domain.agentDefinitions;
  if (agentHandlers) {
    route('GET', '/omp/agent-definitions', gated('agentDefinitions.v1', (request, ctx) => (agentHandlers.list(request, ctx))));
    route('GET', '/omp/agent-definitions/{name}', gated('agentDefinitions.v1', (request, ctx) => (agentHandlers.get(request, ctx))));
    route('POST', '/omp/agent-definitions', gated('agentDefinitions.v1', async (request, ctx) => {
      try {
        return await agentHandlers.create(request, ctx);
      } catch (error) {
        return toResponse(error);
      }
    }));
    route('PUT', '/omp/agent-definitions/{name}', gated('agentDefinitions.v1', async (request, ctx) => {
      try {
        return await agentHandlers.update(request, ctx);
      } catch (error) {
        return toResponse(error);
      }
    }));
    route('DELETE', '/omp/agent-definitions/{name}', gated('agentDefinitions.v1', async (request, ctx) => {
      try {
        return await agentHandlers.remove(request, ctx);
      } catch (error) {
        return toResponse(error);
      }
    }));
    route('POST', '/omp/agent-definitions/refresh', gated('agentDefinitions.v1', async (request, ctx) => {
      try {
        return await agentHandlers.refresh(request, ctx);
      } catch (error) {
        return toResponse(error);
      }
    }));
    route('POST', '/omp/agent-definitions/{name}/reveal', gated('agentDefinitions.v1', async (request, ctx) => {
      try {
        return await agentHandlers.reveal(request, ctx);
      } catch (error) {
        return toResponse(error);
      }
    }));
  }

  // ---- personas (personas.v1) ----
  const personaHandlers = domain.personas;
  if (personaHandlers) {
    route('GET', '/omp/personas', gated('personas.v1', personaHandlers.list));
    route('GET', '/omp/personas/{name}', gated('personas.v1', personaHandlers.get));
    route('POST', '/omp/personas', gated('personas.v1', async (request, ctx) => {
      try {
        return await personaHandlers.create(request, ctx);
      } catch (error) {
        return toResponse(error);
      }
    }));
    route('PUT', '/omp/personas/{name}', gated('personas.v1', async (request, ctx) => {
      try {
        return await personaHandlers.update(request, ctx);
      } catch (error) {
        return toResponse(error);
      }
    }));
    route('DELETE', '/omp/personas/{name}', gated('personas.v1', async (request, ctx) => {
      try {
        return await personaHandlers.remove(request, ctx);
      } catch (error) {
        return toResponse(error);
      }
    }));
  }
}
