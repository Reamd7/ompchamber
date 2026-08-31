/**
 * omp parity HTTP/SSE surface — shared runtime-agnostic factories (spec
 * docs/omp-parity/05 §5.2.2; skills ui-api-decoupling + relay-transport).
 *
 * All omp-native endpoints are explicit OMPChamber routes served by the omp
 * host and reached through `runtimeFetch` (including its SSE streaming
 * branch), so one implementation is correct for web, desktop, VS Code,
 * hosted mobile, and Capacitor mobile — the relay tunnel routes allowlisted
 * `/api/*` HTTP+SSE transparently. No component may build these URLs; this
 * module is the single owner of omp route literals.
 *
 * `GET /api/omp/events` is consumed as a streaming fetch (not `EventSource`)
 * so the request carries bearer auth and rides the relay tunnel / VS Code
 * SSE bridge exactly like the wire stream.
 */

import { runtimeFetch, type RuntimeFetchOptions } from '@/lib/runtime-fetch';
import { z } from 'zod';

// ---------------------------------------------------------------------------
// Envelope + payload contracts (spec 05 §5.2.1; registry = single authority)
// ---------------------------------------------------------------------------

/** SSE frame envelope; payloads stay untyped — domain modules parse at their boundary. */
export interface OmpEventEnvelope {
  id: number;
  type: string;
  directory: string;
  sessionID?: string;
  schemaVersion?: string;
  createdAt: number;
  payload: unknown;
}

/** Control frame emitted when the durable ring cannot bridge a gap (master D2). */
export interface OmpStreamResyncPayload {
  scope: string[];
  lastEventId: number | null;
}

export interface OmpCapabilities {
  version: number;
  eventSchema: string;
  features: Record<string, boolean>;
  minUiVersion: string;
}

export interface OmpCustomMessageEntry {
  wireMessageID: string;
  customType: string;
  timestamp?: number;
  attribution?: string;
  details?: unknown;
}

export interface OmpTurnTelemetryEntry {
  messageID: string;
  timestamp?: number;
  input?: number;
  output?: number;
  cacheRead?: number;
  cacheWrite?: number;
  reasoningTokens?: number;
  totalTokens?: number;
  ttftMs?: number;
  durationMs?: number;
}

export type OmpSessionEntryKind =
  | 'compaction'
  | 'branch_summary'
  | 'model_change'
  | 'mode_change'
  | 'ttsr_injection'
  | 'retry_recovery';

/** Session entry rows are owned by their kinds; kind is the only guaranteed field. */
export interface OmpSessionEntry {
  kind: string;
  [key: string]: unknown;
}

// ---------------------------------------------------------------------------
// Path registry — the only place omp route literals live
// ---------------------------------------------------------------------------

export const OMP_ENDPOINTS = {
  capabilities: '/api/omp/capabilities',
  events: '/api/omp/events',
  sessionCustomMessages: (sessionID: string) => `/api/omp/sessions/${encodeURIComponent(sessionID)}/custom-messages`,
  sessionTelemetry: (sessionID: string) => `/api/omp/sessions/${encodeURIComponent(sessionID)}/telemetry`,
  sessionEntries: (sessionID: string) => `/api/omp/sessions/${encodeURIComponent(sessionID)}/entries`,
  sessionMode: (sessionID: string) => `/api/omp/sessions/${encodeURIComponent(sessionID)}/mode`,
  sessionQueue: (sessionID: string) => `/api/omp/sessions/${encodeURIComponent(sessionID)}/queue`,
  sessionTree: (sessionID: string) => `/api/omp/sessions/${encodeURIComponent(sessionID)}/tree`,
  sessionModel: (sessionID: string) => `/api/omp/sessions/${encodeURIComponent(sessionID)}/model`,
  sessionPlan: (sessionID: string) => `/api/omp/sessions/${encodeURIComponent(sessionID)}/plan`,
  sessionPlanReview: (sessionID: string) => `/api/omp/sessions/${encodeURIComponent(sessionID)}/plan/review`,
  models: '/api/omp/models',
  dialogs: '/api/omp/dialogs',
  chrome: '/api/omp/chrome',
  dialogsLease: '/api/omp/dialogs/lease',
  dialogsLeaseRelease: '/api/omp/dialogs/lease/release',
  dialogRespond: (dialogID: string) => `/api/omp/dialogs/${encodeURIComponent(dialogID)}/respond`,
  dialogPresented: (dialogID: string) => `/api/omp/dialogs/${encodeURIComponent(dialogID)}/presented`,
  dialogAbort: (dialogID: string) => `/api/omp/dialogs/${encodeURIComponent(dialogID)}/abort`,
  settings: '/api/omp/settings',
  agentDefinitions: '/api/omp/agent-definitions',
  agentDefinition: (name: string) => `/api/omp/agent-definitions/${encodeURIComponent(name)}`,
  agentDefinitionReveal: (name: string) => `/api/omp/agent-definitions/${encodeURIComponent(name)}/reveal`,
  personas: '/api/omp/personas',
  persona: (name: string) => `/api/omp/personas/${encodeURIComponent(name)}`,
  agentRuns: '/api/omp/agent-runs',
  jobs: '/api/omp/jobs',
  commands: '/api/omp/commands',
  providers: '/api/omp/providers',
  provider: (id: string) => `/api/omp/providers/${encodeURIComponent(id)}`,
  providerFetch: (id: string) => `/api/omp/providers/${encodeURIComponent(id)}/fetch-models`,
  uriResolve: '/api/omp/uri/resolve',
  uriOpen: '/api/omp/uri/open',
  uriTokenContent: (id: string) => `/api/omp/uri/tokens/${encodeURIComponent(id)}/content`,
  artifacts: '/api/omp/artifacts',
  plugins: '/api/omp/plugins',
  plugin: (id: string) => `/api/omp/plugins/${encodeURIComponent(id)}`,
  pluginExtension: (id: string) => `/api/omp/plugins/extensions/${encodeURIComponent(id)}`,
  pluginReveal: (id: string) => `/api/omp/plugins/${encodeURIComponent(id)}/reveal`,
  extensionReveal: (id: string) => `/api/omp/plugins/extensions/${encodeURIComponent(id)}/reveal`,
  pluginsApplied: '/api/omp/plugins/applied',
  pluginsReload: '/api/omp/plugins/reload',
  pluginExtensions: '/api/omp/plugins/extensions',
 } as const;

// ---------------------------------------------------------------------------
// JSON GET helper — parses at the boundary. Failures are reported distinctly
// so callers can degrade per feature (missing surface vs transport error).
// ---------------------------------------------------------------------------

export type OmpFetchJsonResult<T> =
  | { ok: true; data: T }
  /** 404/501: the omp host does not offer this surface (feature off / old engine). */
  | { ok: false; unavailable: true }
  /** Transport failure or malformed payload — never authoritative empty success. */
  | { ok: false; unavailable: false };

const parseCapabilities = (value: unknown): OmpCapabilities | null => {
  if (!value || typeof value !== 'object' || Array.isArray(value)) return null;
  const record = value as Record<string, unknown>;
  const featuresRaw = record.features;
  if (typeof record.eventSchema !== 'string' || record.eventSchema.length === 0) return null;
  if (!featuresRaw || typeof featuresRaw !== 'object' || Array.isArray(featuresRaw)) return null;
  return {
    version: typeof record.version === 'number' ? record.version : 0,
    eventSchema: record.eventSchema,
    features: featuresRaw as Record<string, boolean>,
    minUiVersion: typeof record.minUiVersion === 'string' ? record.minUiVersion : '0.0.0',
  };
};

const parseArrayPayload = <T>(validateItem: (item: unknown) => T | null) => (value: unknown): T[] | null => {
  if (!Array.isArray(value)) return null;
  const items: T[] = [];
  for (const item of value) {
    const parsed = validateItem(item);
    if (parsed === null) return null;
    items.push(parsed);
  }
  return items;
};

const parseCustomMessageEntry = (value: unknown): OmpCustomMessageEntry | null => {
  if (!value || typeof value !== 'object') return null;
  const record = value as Record<string, unknown>;
  if (typeof record.wireMessageID !== 'string' || typeof record.customType !== 'string') return null;
  return {
    wireMessageID: record.wireMessageID,
    customType: record.customType,
    ...(typeof record.timestamp === 'number' ? { timestamp: record.timestamp } : {}),
    ...(typeof record.attribution === 'string' ? { attribution: record.attribution } : {}),
    ...('details' in record ? { details: record.details } : {}),
  };
};

const parseTelemetryEntry = (value: unknown): OmpTurnTelemetryEntry | null => {
  if (!value || typeof value !== 'object') return null;
  const record = value as Record<string, unknown>;
  if (typeof record.messageID !== 'string') return null;
  return { ...(record as Omit<OmpTurnTelemetryEntry, 'messageID'>), messageID: record.messageID };
};

const parseSessionEntry = (value: unknown): OmpSessionEntry | null => {
  if (!value || typeof value !== 'object') return null;
  const record = value as Record<string, unknown>;
  if (typeof record.kind !== 'string') return null;
  return record as OmpSessionEntry;
};

export interface OmpJsonApiOptions {
  /** Injection seam for tests; production callers omit it. */
  fetchImpl?: typeof runtimeFetch;
}

const ompFetchJson = async <T>(
  fetchImpl: typeof runtimeFetch,
  path: string,
  validate: (value: unknown) => T | null,
  options: RuntimeFetchOptions = {},
  invalidIsUnavailable = false,
): Promise<OmpFetchJsonResult<T>> => {
  let response: Response;
  try {
    response = await fetchImpl(path, {
      method: 'GET',
      headers: { Accept: 'application/json' },
      ...options,
    });
  } catch {
    return { ok: false, unavailable: false };
  }
  // 404 = old engine without the surface; 501 = capability explicitly off
  // (omp-parity featureUnavailable). Both mean "degrade", not "failure".
  if (response.status === 404 || response.status === 501) {
    return { ok: false, unavailable: true };
  }
  if (!response.ok) {
    return { ok: false, unavailable: false };
  }
  let value: unknown;
  try {
    value = await response.json();
  } catch {
    return { ok: false, unavailable: false };
  }
  const parsed = validate(value);
  return parsed === null
    ? { ok: false, unavailable: invalidIsUnavailable }
    : { ok: true, data: parsed };
};

// ---------------------------------------------------------------------------
// Capabilities API
// ---------------------------------------------------------------------------

export interface OmpCapabilitiesAPI {
  /**
   * Resolves the server's omp capability switchboard. Returns `null` ONLY for
   * a definitive "no omp surface" answer (404/501, or a 200 payload without
   * `eventSchema` — an old engine, spec 05 §5.2.3 matrix row 1). Transport
   * failures throw so callers can retry instead of degrading on a fluke.
   */
  getCapabilities(): Promise<OmpCapabilities | null>;
}

export const createOmpCapabilitiesAPI = (options: OmpJsonApiOptions = {}): OmpCapabilitiesAPI => ({
  async getCapabilities() {
    // A 200 that isn't a capabilities payload is an old engine answering the
    // route itself — definitive absence (spec 05 §5.2.3 matrix row 1).
    const result = await ompFetchJson(
      options.fetchImpl ?? runtimeFetch,
      OMP_ENDPOINTS.capabilities,
      parseCapabilities,
      {},
      true,
    );
    if (!result.ok) {
      if (result.unavailable) return null;
      throw new Error('omp capabilities fetch failed');
    }
    return result.data;
  },
});

// ---------------------------------------------------------------------------
// Session reads API (custom messages / telemetry / entries — spec 05 §5.2.1)
// ---------------------------------------------------------------------------

export interface OmpSessionReadOptions {
  directory: string;
  kinds?: readonly OmpSessionEntryKind[];
}

export interface OmpSessionAPI {
  getCustomMessages(sessionID: string, options: OmpSessionReadOptions): Promise<OmpFetchJsonResult<OmpCustomMessageEntry[]>>;
  getTelemetry(sessionID: string, options: OmpSessionReadOptions): Promise<OmpFetchJsonResult<OmpTurnTelemetryEntry[]>>;
  getEntries(sessionID: string, options: OmpSessionReadOptions): Promise<OmpFetchJsonResult<OmpSessionEntry[]>>;
}

export const createOmpSessionAPI = (apiOptions: OmpJsonApiOptions = {}): OmpSessionAPI => {
  const fetchImpl = apiOptions.fetchImpl ?? runtimeFetch;
  const readJson = <T>(path: string, validate: (value: unknown) => T | null, options: RuntimeFetchOptions = {}) =>
    ompFetchJson(fetchImpl, path, validate, options);
  return {
    getCustomMessages(sessionID, options) {
      return readJson(
        OMP_ENDPOINTS.sessionCustomMessages(sessionID),
        parseArrayPayload(parseCustomMessageEntry),
        { query: { directory: options.directory } },
      );
    },
    getTelemetry(sessionID, options) {
      return readJson(
        OMP_ENDPOINTS.sessionTelemetry(sessionID),
        parseArrayPayload(parseTelemetryEntry),
        { query: { directory: options.directory } },
      );
    },
    getEntries(sessionID, options) {
      return readJson(
        OMP_ENDPOINTS.sessionEntries(sessionID),
        parseArrayPayload(parseSessionEntry),
        {
          query: {
            directory: options.directory,
            ...(options.kinds && options.kinds.length > 0 ? { kinds: options.kinds.join(',') } : {}),
          },
        },
      );
    },
  };
};

// ---------------------------------------------------------------------------
// Models + session modes API (spec 01 §5.3(1) / 02 §5.4 — capability-gated
// surfaces consumed by the composer picker; roles are configured through
// /api/omp/settings, never written here).
// ---------------------------------------------------------------------------

const RoleEntrySchema = z.object({
  configured: z.string().min(1),
  provider: z.string().nullable(),
  id: z.string().nullable(),
  thinkingLevel: z.string().optional(),
  source: z.string().optional(),
});

const RoleMetaSchema = z.object({
  name: z.string(),
  tag: z.string().optional(),
  color: z.string().optional(),
  hidden: z.literal(true).optional(),
});

const ModelThinkingSchema = z.object({
  supported: z.array(z.string()).default([]),
  defaultLevel: z.string().nullable().default(null),
});

const ModelEntrySchema = z.object({
  provider: z.string(),
  id: z.string(),
  name: z.string().optional(),
  reasoning: z.boolean().default(false),
  contextWindow: z.number().optional(),
  maxTokens: z.number().optional(),
  thinking: ModelThinkingSchema.default({ supported: [], defaultLevel: null }),
});

const ModelsSnapshotSchema = z.object({
  schemaVersion: z.string(),
  directory: z.string(),
  models: z.array(ModelEntrySchema).default([]),
  roles: z.record(z.string(), z.union([RoleEntrySchema, z.null()])),
  roleMeta: z.record(z.string(), RoleMetaSchema),
  cycleOrder: z.array(z.string()),
  enabledModels: z.array(z.string()).default([]),
  fallbackChains: z.record(z.string(), z.array(z.string())).default({}),
  modelRoleStorage: z.string().default('global'),
  defaultThinkingLevel: z.string().default(''),
  legacyDefaults: z.object({
    defaultModel: z.string(),
    defaultProvider: z.string().optional(),
  }).nullable().default(null),
});

export type OmpModelRoleMeta = z.infer<typeof RoleMetaSchema>;
/** Registry model projection from GET /api/omp/models (identity + thinking). */
export type OmpModelEntry = z.infer<typeof ModelEntrySchema>;
/** GET /api/omp/models?directory=… payload (settings-side role truth). */
export type OmpModelsSnapshot = z.infer<typeof ModelsSnapshotSchema>;

const parseModelsSnapshot = (value: unknown): OmpModelsSnapshot | null => {
  const parsed = ModelsSnapshotSchema.safeParse(value);
  return parsed.success ? parsed.data : null;
};

/** Per-role assignment from GET /api/omp/models; `null` = role not configured. */
export type OmpModelRoleEntry = z.infer<typeof RoleEntrySchema>;

export type OmpSetSessionModelResult =
  | { ok: true; model: string }
  | { ok: false; unavailable: boolean; error?: string };

export interface OmpModelsAPI {
  /** Roles snapshot for a directory's keyed Settings instance. */
  getModels(options: { directory: string }): Promise<OmpFetchJsonResult<OmpModelsSnapshot>>;
  /** Session-only model switch; prompts remain model-free under modelRoles.v1.
   * `thinkingLevel` applies an in-session thinking change (GAP-06); when the
   * model matches the session's current model the engine only calls
   * setThinkingLevel. `'inherit'` is the wire sentinel that clears the
   * explicit level (engine maps it to setThinkingLevel(undefined)). */
  setSessionModel(
    sessionID: string,
    model: { providerID: string; modelID: string },
    options: { directory: string; thinkingLevel?: string },
  ): Promise<OmpSetSessionModelResult>;
}

export const createOmpModelsAPI = (apiOptions: OmpJsonApiOptions = {}): OmpModelsAPI => {
  const fetchImpl = apiOptions.fetchImpl ?? runtimeFetch;
  return {
    getModels(options) {
      return ompFetchJson(fetchImpl, OMP_ENDPOINTS.models, parseModelsSnapshot, {
        query: { directory: options.directory },
      });
    },
    async setSessionModel(sessionID, model, options) {
      let response: Response;
      try {
        response = await fetchImpl(OMP_ENDPOINTS.sessionModel(sessionID), {
          method: 'POST',
          headers: { Accept: 'application/json', 'Content-Type': 'application/json' },
          query: { directory: options.directory },
          body: JSON.stringify({ model, ...(options.thinkingLevel ? { thinkingLevel: options.thinkingLevel } : {}) }),
        });
      } catch {
        return { ok: false, unavailable: false };
      }
      if (response.status === 404 || response.status === 501) return { ok: false, unavailable: true };
      let payload: unknown = null;
      try { payload = await response.json(); } catch { /* status is enough */ }
      if (!response.ok) {
        const parsed = z.object({ error: z.string().optional() }).safeParse(payload);
        return { ok: false, unavailable: false, ...(parsed.success && parsed.data.error ? { error: parsed.data.error } : {}) };
      }
      const parsed = z.object({ ok: z.literal(true), model: z.string().min(1) }).safeParse(payload);
      return parsed.success ? parsed.data : { ok: false, unavailable: false };
    },
  };
};


const ModeSnapshotSchema = z.looseObject({ mode: z.string().min(1) });
const ModeConflictSchema = z.object({ conflict: z.string() });

/** GET/POST /api/omp/sessions/{id}/mode snapshot (02 §5.4; `mode` is the only guaranteed field). */
export type OmpModeSnapshot = z.infer<typeof ModeSnapshotSchema>;

/** PlanApprovalDetails as the engine's preparePlanForReview shapes it (SDK plan-mode/approved-plan.ts). */
const PlanReviewDetailsSchema = z.object({
  planFilePath: z.string().min(1),
  title: z.string(),
  planExists: z.boolean(),
});

/** GET /api/omp/sessions/{id}/plan payload (02 §5.5 step 7). */
const PlanSnapshotSchema = z.looseObject({
  planFilePath: z.string().min(1),
  review: PlanReviewDetailsSchema.optional(),
});

export type OmpPlanReviewDetails = z.infer<typeof PlanReviewDetailsSchema>;
export type OmpPlanSnapshot = z.infer<typeof PlanSnapshotSchema>;

/** Review choices (domain-modes PLAN_REVIEW_CHOICES; TUI overlay options 3979-3982). */
export const OMP_PLAN_REVIEW_CHOICES = ['approve-execute', 'approve-compact', 'approve-keep', 'refine'] as const;
export type OmpPlanReviewChoice = (typeof OMP_PLAN_REVIEW_CHOICES)[number];

/** POST /api/omp/sessions/{id}/plan/review body (02 §5.5 step 5). */
export interface OmpPlanReviewInput {
  choice: OmpPlanReviewChoice;
  /** Refine loop: annotation feedback text re-prompting the planning turn. */
  feedback?: string;
  /** In-overlay edited plan full text (approve paths). */
  editedContent?: string;
  /** Tier-slider role selection (01 chapter cycleOrder roles). */
  executionRole?: string;
}

export type OmpSubmitPlanReviewResult =
  | { ok: true; dispatched: boolean; mode: string }
  /** 404/501: plan surface not offered (feature off / old engine / inactive plan). */
  | { ok: false; unavailable: true }
  /** Transport failure, malformed payload, or a rejected decision (invalid-choice, …). */
  | { ok: false; unavailable: false; reason?: string };

export type OmpSetModeResult =
  | { ok: true; snapshot: OmpModeSnapshot }
  /** 404/501: modes surface not offered (feature off / old engine). */
  | { ok: false; unavailable: true }
  /** Transport failure, malformed payload, or a rejected transition. */
  | { ok: false; unavailable: false; conflict?: string };
export interface OmpModesAPI {
  getMode(sessionID: string, options: { directory: string }): Promise<OmpFetchJsonResult<OmpModeSnapshot>>;
  /**
   * Enters a mode (`'plan' | 'goal' | 'vibe' | …`) or exits to standard
   * (`'none'`). A 409 `mode-conflict` surfaces as `{ ok: false, conflict }`
   * so callers can tell the user which mode must be exited first.
   */
  setMode(sessionID: string, mode: string, options: { directory: string }): Promise<OmpSetModeResult>;
  /**
   * GET /api/omp/sessions/{id}/plan (02 §5.5 step 7): the reviewed plan file
   * path plus the pending review, when plan mode or a review is active.
   * `unavailable` also covers the inactive 404 — there is nothing to review.
   */
  getPlan(sessionID: string, options: { directory: string }): Promise<OmpFetchJsonResult<OmpPlanSnapshot>>;
  /**
   * POST /api/omp/sessions/{id}/plan/review (02 §5.5 step 5): settle the
   * pending proposal. `refine` keeps the turn planning (`dispatched: false`)
   * and re-prompts with the feedback text.
   */
  submitPlanReview(sessionID: string, input: OmpPlanReviewInput & { directory: string }): Promise<OmpSubmitPlanReviewResult>;
}

const parseModeSnapshot = (value: unknown): OmpModeSnapshot | null => {
  const parsed = ModeSnapshotSchema.safeParse(value);
  return parsed.success ? parsed.data : null;
};

export const createOmpModesAPI = (apiOptions: OmpJsonApiOptions = {}): OmpModesAPI => {
  const fetchImpl = apiOptions.fetchImpl ?? runtimeFetch;
  const readMode = async (
    method: 'GET' | 'POST',
    sessionID: string,
    options: { directory: string },
    body?: Record<string, unknown>,
  ): Promise<Response> => fetchImpl(OMP_ENDPOINTS.sessionMode(sessionID), {
    method,
    headers: { Accept: 'application/json', ...(body !== undefined ? { 'Content-Type': 'application/json' } : {}) },
    ...(body !== undefined ? { body: JSON.stringify(body) } : {}),
    query: { directory: options.directory },
  });

  return {
    async getMode(sessionID, options) {
      let response: Response;
      try {
        response = await readMode('GET', sessionID, options);
      } catch {
        return { ok: false, unavailable: false };
      }
      if (response.status === 404 || response.status === 501) {
        return { ok: false, unavailable: true };
      }
      if (!response.ok) {
        return { ok: false, unavailable: false };
      }
      let value: unknown;
      try {
        value = await response.json();
      } catch {
        return { ok: false, unavailable: false };
      }
      const parsed = parseModeSnapshot(value);
      return parsed === null ? { ok: false, unavailable: false } : { ok: true, data: parsed };
    },

    async setMode(sessionID, mode, options) {
      let response: Response;
      try {
        response = await readMode('POST', sessionID, options, { mode });
      } catch {
        return { ok: false, unavailable: false };
      }
      if (response.status === 404 || response.status === 501) {
        return { ok: false, unavailable: true };
      }
      if (response.status === 409) {
        let conflict: string | undefined;
        try {
          const parsed = ModeConflictSchema.safeParse(await response.json());
          if (parsed.success) conflict = parsed.data.conflict;
        } catch {
          // conflict label is best-effort; the generic failure path covers it
        }
        return { ok: false, unavailable: false, ...(conflict !== undefined ? { conflict } : {}) };
      }
      if (!response.ok) {
        return { ok: false, unavailable: false };
      }
      let value: unknown;
      try {
        value = await response.json();
      } catch {
        return { ok: false, unavailable: false };
      }
      const parsed = parseModeSnapshot(value);
      return parsed === null ? { ok: false, unavailable: false } : { ok: true, snapshot: parsed };
    },

    async getPlan(sessionID, options) {
      let response: Response;
      try {
        response = await fetchImpl(OMP_ENDPOINTS.sessionPlan(sessionID), {
          method: 'GET',
          headers: { Accept: 'application/json' },
          query: { directory: options.directory },
        });
      } catch {
        return { ok: false, unavailable: false };
      }
      if (response.status === 404 || response.status === 501) {
        return { ok: false, unavailable: true };
      }
      if (!response.ok) {
        return { ok: false, unavailable: false };
      }
      let value: unknown;
      try {
        value = await response.json();
      } catch {
        return { ok: false, unavailable: false };
      }
      const parsed = PlanSnapshotSchema.safeParse(value);
      return parsed.success ? { ok: true, data: parsed.data } : { ok: false, unavailable: false };
    },

    async submitPlanReview(sessionID, input) {
      const { directory, choice, feedback, editedContent, executionRole } = input;
      let response: Response;
      try {
        response = await fetchImpl(OMP_ENDPOINTS.sessionPlanReview(sessionID), {
          method: 'POST',
          headers: { Accept: 'application/json', 'Content-Type': 'application/json' },
          query: { directory },
          body: JSON.stringify({
            choice,
            ...(feedback !== undefined ? { feedback } : {}),
            ...(editedContent !== undefined ? { editedContent } : {}),
            ...(executionRole !== undefined ? { executionRole } : {}),
          }),
        });
      } catch {
        return { ok: false, unavailable: false };
      }
      if (response.status === 404 || response.status === 501) {
        return { ok: false, unavailable: true };
      }
      if (!response.ok) {
        let reason: string | undefined;
        try {
          const parsed = z.object({ error: z.string().optional() }).safeParse(await response.json());
          if (parsed.success && parsed.data.error) reason = parsed.data.error;
        } catch {
          // status is enough; the domain body is best-effort
        }
        return { ok: false, unavailable: false, ...(reason !== undefined ? { reason } : {}) };
      }
      const parsed = z.object({ dispatched: z.boolean(), mode: z.string() }).safeParse(await response.json().catch(() => null));
      return parsed.success
        ? { ok: true, dispatched: parsed.data.dispatched, mode: parsed.data.mode }
        : { ok: false, unavailable: false };
    },
  };
};

// ---------------------------------------------------------------------------
// Agent definitions + personas (spec 02 §5.2/§5.2a — worker-definition CRUD
// and the OC-original persona resource; server gates each group by
// agentDefinitions.v1 / personas.v1).
// ---------------------------------------------------------------------------

const AgentDefinitionRecordSchema = z.looseObject({
  name: z.string().min(1),
  description: z.string(),
  /** Definition layer (omp discovery chain; bundled is read-only). */
  source: z.enum(['project', 'user', 'bundled']),
  filePath: z.string().optional(),
  /** Definition body — the worker's system prompt (markdown). */
  systemPrompt: z.string(),
  /** Model patterns, e.g. ["@smol", "anthropic/*:high"]. */
  model: z.array(z.string()).optional(),
  thinkingLevel: z.string().optional(),
  tools: z.array(z.string()).optional(),
  spawns: z.union([z.array(z.string()), z.literal('*')]).optional(),
  prewalk: z.union([z.boolean(), z.string()]).optional(),
  advisor: z.union([z.boolean(), z.string()]).optional(),
  readSummarize: z.boolean().optional(),
  /** Settings-level task.* override projection (02 §5.2). */
  disabled: z.boolean().optional(),
  modelOverride: z.string().optional(),
  prewalkOverride: z.string().optional(),
  advisorOverride: z.string().optional(),
});

const AgentDefinitionsListSchema = z.object({
  agents: z.array(AgentDefinitionRecordSchema),
  projectAgentsDir: z.string().nullable(),
});

/** GET /api/omp/agent-definitions record (OmpAgent, 02 §5.2 discovery contract). */
export type OmpAgentDefinitionRecord = z.infer<typeof AgentDefinitionRecordSchema>;

/** POST/PUT definition body (omp AgentDefinition frontmatter fields). */
export interface OmpAgentDefinitionInput {
  name: string;
  description: string;
  systemPrompt: string;
  model?: string[];
  thinkingLevel?: string;
  tools?: string[];
  spawns?: string[] | '*';
  prewalk?: boolean | string;
  advisor?: boolean | string;
  readSummarize?: boolean;
}

const PersonaSchema = z.looseObject({
  name: z.string().min(1),
  description: z.string().optional(),
  systemPrompt: z.string().optional(),
  tools: z.array(z.string()).optional(),
});

const PersonasListSchema = z.object({ personas: z.array(PersonaSchema) });

/** GET /api/omp/personas record (OmpPersona, spec 02 §5.2a — no model binding). */
export type OmpPersona = z.infer<typeof PersonaSchema>;

/** POST/PUT persona body (rename rides `name` in the update body). */
export interface OmpPersonaInput {
  name?: string;
  description?: string;
  systemPrompt?: string;
  tools?: string[];
}

/** Shared error body of the definitions/personas domains (ModeDomainError). */
const CrudErrorSchema = z.object({
  error: z.string().optional(),
  message: z.string().optional(),
  name: z.string().optional(),
  tools: z.array(z.string()).optional(),
});

export type OmpCrudMutationResult<T> =
  | { ok: true; record: T }
  /** 501, or a 404 without a domain body (old engine without the surface). */
  | { ok: false; unavailable: true }
  /**
   * 400/409 the server explained: `reason` is the domain error code
   * (invalid-prompt, agent-definition-exists, persona-exists, not-found, …)
   * and `conflictName` the taken name on a 409 name conflict.
   */
  | { ok: false; unavailable: false; kind: 'rejected'; reason?: string; conflictName?: string }
  /** Transport failure, malformed payload, or any other non-2xx. */
  | { ok: false; unavailable: false; kind: 'error' };

export type OmpCrudDeleteResult =
  | { ok: true }
  | { ok: false; unavailable: true }
  | { ok: false; unavailable: false; kind: 'not-found' | 'error' };

const parseCrudError = (payload: unknown): { reason?: string; conflictName?: string } | null => {
  const parsed = CrudErrorSchema.safeParse(payload);
  if (!parsed.success) return null;
  return {
    ...(parsed.data.error !== undefined ? { reason: parsed.data.error } : {}),
    ...(parsed.data.name !== undefined ? { conflictName: parsed.data.name } : {}),
  };
};

const readCrudMutationResponse = async <T>(
  response: Response,
  parseRecord: (value: unknown) => T | null,
): Promise<OmpCrudMutationResult<T>> => {
  if (response.status === 501) return { ok: false, unavailable: true };
  let payload: unknown = null;
  try { payload = await response.json(); } catch { /* status is enough */ }
  if (response.status === 404) {
    // Distinguish "record missing" (domain answers {error:'not-found'}) from
    // an old engine without the surface (no domain body).
    const error = parseCrudError(payload);
    return error?.reason === 'not-found'
      ? { ok: false, unavailable: false, kind: 'rejected', reason: 'not-found' }
      : { ok: false, unavailable: true };
  }
  if (response.status === 400 || response.status === 409) {
    const error = parseCrudError(payload);
    return error === null
      ? { ok: false, unavailable: false, kind: 'rejected' }
      : { ok: false, unavailable: false, kind: 'rejected', ...error };
  }
  if (!response.ok) return { ok: false, unavailable: false, kind: 'error' };
  const parsed = parseRecord(payload);
  return parsed === null ? { ok: false, unavailable: false, kind: 'error' } : { ok: true, record: parsed };
};

const readCrudDeleteResponse = async (response: Response): Promise<OmpCrudDeleteResult> => {
  if (response.status === 501) return { ok: false, unavailable: true };
  if (response.ok) return { ok: true };
  if (response.status === 404) {
    let payload: unknown = null;
    try { payload = await response.json(); } catch { /* status is enough */ }
    return parseCrudError(payload)?.reason === 'not-found'
      ? { ok: false, unavailable: false, kind: 'not-found' }
      : { ok: false, unavailable: true };
  }
  return { ok: false, unavailable: false, kind: 'error' };
};
export interface OmpAgentDefinitionsAPI {
  list(directory?: string): Promise<OmpFetchJsonResult<OmpAgentDefinitionRecord[]>>;
  get(name: string, directory?: string): Promise<OmpFetchJsonResult<OmpAgentDefinitionRecord>>;
  create(
    input: OmpAgentDefinitionInput & { scope?: 'user' | 'project' },
    directory?: string,
  ): Promise<OmpCrudMutationResult<OmpAgentDefinitionRecord>>;
  update(
    name: string,
    patch: {
      definition?: Partial<OmpAgentDefinitionInput>;
      renameTo?: string;
      scope?: 'user' | 'project';
    },
    directory?: string,
  ): Promise<OmpCrudMutationResult<OmpAgentDefinitionRecord>>;
  remove(name: string, directory?: string): Promise<OmpCrudDeleteResult>;
  /** POST /agent-definitions/{name}/reveal — open the definition file in the system file manager. */
  reveal(name: string, directory?: string): Promise<{ ok: boolean } | { ok: false; error?: string }>;
}

const parseAgentDefinitionRecord = (value: unknown): OmpAgentDefinitionRecord | null => {
  const parsed = AgentDefinitionRecordSchema.safeParse(value);
  return parsed.success ? parsed.data : null;
};

const definitionBody = (input: OmpAgentDefinitionInput): Record<string, unknown> => ({
  name: input.name,
  description: input.description,
  systemPrompt: input.systemPrompt,
  ...(input.model !== undefined ? { model: input.model } : {}),
  ...(input.thinkingLevel !== undefined ? { thinkingLevel: input.thinkingLevel } : {}),
  ...(input.tools !== undefined ? { tools: input.tools } : {}),
  ...(input.spawns !== undefined ? { spawns: input.spawns } : {}),
  ...(input.prewalk !== undefined ? { prewalk: input.prewalk } : {}),
  ...(input.advisor !== undefined ? { advisor: input.advisor } : {}),
  ...(input.readSummarize !== undefined ? { readSummarize: input.readSummarize } : {}),
});

const withDirectory = (options: RuntimeFetchOptions, directory?: string): RuntimeFetchOptions =>
  (directory ? { ...options, headers: { 'x-opencode-directory': directory, ...(options.headers ?? {}) } } : options);

export const createOmpAgentDefinitionsAPI = (apiOptions: OmpJsonApiOptions = {}): OmpAgentDefinitionsAPI => {
  const fetchImpl = apiOptions.fetchImpl ?? runtimeFetch;
  return {
    list: (directory) => ompFetchJson(
      fetchImpl,
      OMP_ENDPOINTS.agentDefinitions,
      (value) => {
        const parsed = AgentDefinitionsListSchema.safeParse(value);
        return parsed.success ? parsed.data.agents : null;
      },
      withDirectory({}, directory),
    ),
    get: (name, directory) => ompFetchJson(
      fetchImpl,
      OMP_ENDPOINTS.agentDefinition(name),
      parseAgentDefinitionRecord,
      withDirectory({}, directory),
    ),
    async create(input, directory) {
      let response: Response;
      try {
        response = await fetchImpl(OMP_ENDPOINTS.agentDefinitions, {
          method: 'POST',
          headers: {
            Accept: 'application/json',
            'Content-Type': 'application/json',
            ...(directory ? { 'x-opencode-directory': directory } : {}),
          },
          body: JSON.stringify({
            ...(input.scope !== undefined ? { scope: input.scope } : {}),
            definition: definitionBody(input),
          }),
        });
      } catch {
        return { ok: false, unavailable: false, kind: 'error' };
      }
      return readCrudMutationResponse(response, parseAgentDefinitionRecord);
    },
    async update(name, patch, directory) {
      let response: Response;
      try {
        response = await fetchImpl(OMP_ENDPOINTS.agentDefinition(name), {
          method: 'PUT',
          headers: {
            Accept: 'application/json',
            'Content-Type': 'application/json',
            ...(directory ? { 'x-opencode-directory': directory } : {}),
          },
          body: JSON.stringify({
            ...(patch.definition !== undefined ? { definition: definitionBody(patch.definition as OmpAgentDefinitionInput) } : {}),
            ...(patch.renameTo !== undefined ? { renameTo: patch.renameTo } : {}),
            ...(patch.scope !== undefined ? { scope: patch.scope } : {}),
          }),
        });
      } catch {
        return { ok: false, unavailable: false, kind: 'error' };
      }
      return readCrudMutationResponse(response, parseAgentDefinitionRecord);
    },
    async remove(name, directory) {
      let response: Response;
      try {
        response = await fetchImpl(OMP_ENDPOINTS.agentDefinition(name), {
          method: 'DELETE',
          ...(directory ? { headers: { 'x-opencode-directory': directory } } : {}),
        });
      } catch {
        return { ok: false, unavailable: false, kind: 'error' };
      }
      return readCrudDeleteResponse(response);
    },
    async reveal(name, directory) {
      let response: Response;
      try {
        response = await fetchImpl(OMP_ENDPOINTS.agentDefinitionReveal(name), {
          method: 'POST',
          headers: {
            Accept: 'application/json',
            ...(directory ? { 'x-opencode-directory': directory } : {}),
          },
        });
      } catch {
        return { ok: false, error: undefined };
      }
      if (!response.ok) {
        const payload = await response.json().catch(() => null) as { error?: unknown } | null;
        return { ok: false, error: typeof payload?.error === 'string' ? payload.error : undefined };
      }
      return { ok: true };
    },
  };
};

export interface OmpPersonasAPI {
  list(): Promise<OmpFetchJsonResult<OmpPersona[]>>;
  get(name: string): Promise<OmpFetchJsonResult<OmpPersona>>;
  create(persona: OmpPersonaInput & { name: string }): Promise<OmpCrudMutationResult<OmpPersona>>;
  update(name: string, persona: OmpPersonaInput): Promise<OmpCrudMutationResult<OmpPersona>>;
  remove(name: string): Promise<OmpCrudDeleteResult>;
}

const parsePersona = (value: unknown): OmpPersona | null => {
  const parsed = PersonaSchema.safeParse(value);
  return parsed.success ? parsed.data : null;
};

export const createOmpPersonasAPI = (apiOptions: OmpJsonApiOptions = {}): OmpPersonasAPI => {
  const fetchImpl = apiOptions.fetchImpl ?? runtimeFetch;
  const personaBody = (persona: OmpPersonaInput) => ({
    ...(persona.name !== undefined ? { name: persona.name } : {}),
    ...(persona.description !== undefined ? { description: persona.description } : {}),
    ...(persona.systemPrompt !== undefined ? { systemPrompt: persona.systemPrompt } : {}),
    ...(persona.tools !== undefined ? { tools: persona.tools } : {}),
  });
  return {
    list: () => ompFetchJson(
      fetchImpl,
      OMP_ENDPOINTS.personas,
      (value) => {
        const parsed = PersonasListSchema.safeParse(value);
        return parsed.success ? parsed.data.personas : null;
      },
    ),
    get: (name) => ompFetchJson(fetchImpl, OMP_ENDPOINTS.persona(name), parsePersona),
    async create(persona) {
      let response: Response;
      try {
        response = await fetchImpl(OMP_ENDPOINTS.personas, {
          method: 'POST',
          headers: { Accept: 'application/json', 'Content-Type': 'application/json' },
          body: JSON.stringify({ persona: personaBody(persona) }),
        });
      } catch {
        return { ok: false, unavailable: false, kind: 'error' };
      }
      return readCrudMutationResponse(response, parsePersona);
    },
    async update(name, persona) {
      let response: Response;
      try {
        response = await fetchImpl(OMP_ENDPOINTS.persona(name), {
          method: 'PUT',
          headers: { Accept: 'application/json', 'Content-Type': 'application/json' },
          body: JSON.stringify({ persona: personaBody(persona) }),
        });
      } catch {
        return { ok: false, unavailable: false, kind: 'error' };
      }
      return readCrudMutationResponse(response, parsePersona);
    },
    async remove(name) {
      let response: Response;
      try {
        response = await fetchImpl(OMP_ENDPOINTS.persona(name), { method: 'DELETE' });
      } catch {
        return { ok: false, unavailable: false, kind: 'error' };
      }
      return readCrudDeleteResponse(response);
    },
  };
};
// ---------------------------------------------------------------------------
// Settings API — schema-driven engine settings proxy (spec 06 §5.2/§5.3) +
// model-role assignment writes (spec 01 §5.3(2)/§5.5 GAP-05; role values live
// under /api/omp/settings `modelRoles.<role>` keys).
// ---------------------------------------------------------------------------

const SettingUiSchema = z.looseObject({
  tab: z.string().optional(),
  group: z.string().optional(),
  label: z.string().optional(),
  description: z.string().optional(),
  condition: z.string().optional(),
  secret: z.boolean().optional(),
  ordered: z.boolean().optional(),
  /** Labeled enum options, or 'runtime-unresolved' when only the TUI can fill them. */
  options: z.union([
    z.string(),
    z.array(z.looseObject({ value: z.string(), label: z.string().optional(), description: z.string().optional() })),
  ]).optional(),
});

/**
 * One SETTINGS_SCHEMA key projection from GET /api/omp/settings (06 §5.2).
 * Parsed loosely — unknown entry fields are tolerated (schema drift), and
 * `default`/`value` stay `unknown` because the schema spans scalars, enums,
 * arrays, and records. Credential entries carry value/default `null` plus a
 * `configured` boolean (R9: values never echo).
 */
const SettingEntrySchema = z.looseObject({
  type: z.string(),
  values: z.array(z.string()).nullish(),
  default: z.unknown().optional(),
  value: z.unknown().optional(),
  configured: z.boolean().optional(),
  scope: z.string().optional(),
  editable: z.boolean().optional(),
  credential: z.boolean().optional(),
  writeOnly: z.boolean().optional(),
  excluded: z.string().nullish(),
  hidden: z.boolean().optional(),
  ui: SettingUiSchema.optional(),
});

const SettingsTabSchema = z.object({
  id: z.string(),
  label: z.string(),
  groups: z.array(z.string()).default([]),
});

const SettingsSnapshotSchema = z.looseObject({
  schemaVersion: z.string(),
  directory: z.string().nullish(),
  revision: z.number().default(0),
  tabs: z.array(SettingsTabSchema).default([]),
  keys: z.record(z.string(), SettingEntrySchema),
});

export type OmpSettingUi = z.infer<typeof SettingUiSchema>;
/** GET /api/omp/settings entry (06 §5.2; `modelRoles` arrives as a record view). */
export type OmpSettingEntry = z.infer<typeof SettingEntrySchema>;
export type OmpSettingsTab = z.infer<typeof SettingsTabSchema>;
/** GET /api/omp/settings?directory=… payload — the schema-driven settings face. */
export type OmpSettingsSnapshot = z.infer<typeof SettingsSnapshotSchema>;

const parseSettingsSnapshot = (value: unknown): OmpSettingsSnapshot | null => {
  const parsed = SettingsSnapshotSchema.safeParse(value);
  return parsed.success ? parsed.data : null;
};

/** PUT /api/omp/settings outcome (06 §5.3); failures are discriminated so the UI can surface rejected keys inline. */
export type OmpPutSettingsResult =
  | { ok: true; revision: number; applied: Record<string, unknown> }
  /** 404/501: settings surface not offered (feature off / old engine). */
  | { ok: false; unavailable: true }
  /** 400 validation: per-key rejections carry key + reason only (R9 — never the submitted value). */
  | { ok: false; unavailable: false; kind: 'rejected'; rejected: Array<{ key: string; reason?: string }> }
  /** 409: config.yml was quarantined after an invalid-YAML read; the backup path is in the server logs. */
  | { ok: false; unavailable: false; kind: 'quarantined' }
  /** Transport failure, malformed payload, or any other non-2xx. */
  | { ok: false; unavailable: false; kind: 'error' };

export type OmpPutModelRoleResult =
  | { ok: true; value: string | null }
  /** 404/501: settings surface not offered (feature off / old engine). */
  | { ok: false; unavailable: true }
  | { ok: false; unavailable: false; rejected?: string };

export interface OmpSettingsAPI {
  /**
   * Schema-driven settings snapshot for a directory (06 §5.2): entries per
   * SETTINGS_SCHEMA key (credential values never echo — R9), tab/group
   * layout projections, and the special `modelRoles` record view.
   */
  getSettings(options: {
    directory: string;
    /** Optional key filter (`?keys=` csv) — the roles editor uses it for targeted reads. */
    keys?: string[];
  }): Promise<OmpFetchJsonResult<OmpSettingsSnapshot>>;
  /**
   * Commit setting changes (06 §5.3). `scope` defaults to 'global'; role
   * writes omit it so the server honors the directory's `modelRoleStorage`.
   * Rejected keys surface per-key (`kind: 'rejected'`); a quarantined
   * config.yml surfaces as `kind: 'quarantined'`.
   */
  putSettings(options: {
    directory: string;
    changes: Record<string, unknown>;
    scope?: 'global' | 'project';
  }): Promise<OmpPutSettingsResult>;
  /**
   * Assign (`value: 'provider/model[:thinking]'`) or clear (`value: null`) a
   * model role. `scope` defaults to the directory's `modelRoleStorage`.
   */
  putModelRole(options: {
    directory: string;
    role: string;
    value: string | null;
    scope?: 'global' | 'project';
  }): Promise<OmpPutModelRoleResult>;
}

export const createOmpSettingsAPI = (apiOptions: OmpJsonApiOptions = {}): OmpSettingsAPI => {
  const fetchImpl = apiOptions.fetchImpl ?? runtimeFetch;
  return {
    getSettings(options) {
      return ompFetchJson(fetchImpl, OMP_ENDPOINTS.settings, parseSettingsSnapshot, {
        query: {
          directory: options.directory,
          ...(options.keys && options.keys.length > 0 ? { keys: options.keys.join(',') } : {}),
        },
      });
    },

    async putSettings({ directory, changes, scope }) {
      let response: Response;
      try {
        response = await fetchImpl(OMP_ENDPOINTS.settings, {
          method: 'PUT',
          headers: { Accept: 'application/json', 'Content-Type': 'application/json' },
          query: { directory },
          body: JSON.stringify({ changes, ...(scope ? { scope } : {}), directory }),
        });
      } catch {
        return { ok: false, unavailable: false, kind: 'error' };
      }
      if (response.status === 404 || response.status === 501) return { ok: false, unavailable: true };
      let payload: unknown = null;
      try { payload = await response.json(); } catch { /* status is enough */ }
      if (response.status === 400) {
        const parsed = z.object({
          error: z.string().optional(),
          rejected: z.array(z.object({ key: z.string(), reason: z.string().optional() })).optional(),
        }).safeParse(payload);
        return parsed.success && parsed.data.rejected && parsed.data.rejected.length > 0
          ? { ok: false, unavailable: false, kind: 'rejected', rejected: parsed.data.rejected }
          : { ok: false, unavailable: false, kind: 'error' };
      }
      if (response.status === 409) {
        const parsed = z.object({ error: z.string().optional() }).safeParse(payload);
        return parsed.success && parsed.data.error === 'config-quarantined'
          ? { ok: false, unavailable: false, kind: 'quarantined' }
          : { ok: false, unavailable: false, kind: 'error' };
      }
      if (!response.ok) return { ok: false, unavailable: false, kind: 'error' };
      const parsed = z.object({
        revision: z.number().default(0),
        applied: z.record(z.string(), z.unknown()).default({}),
      }).safeParse(payload);
      return parsed.success
        ? { ok: true, revision: parsed.data.revision, applied: parsed.data.applied }
        : { ok: false, unavailable: false, kind: 'error' };
    },
    async putModelRole({ directory, role, value, scope }) {
      let response: Response;
      try {
        response = await fetchImpl(OMP_ENDPOINTS.settings, {
          method: 'PUT',
          headers: { Accept: 'application/json', 'Content-Type': 'application/json' },
          query: { directory },
          body: JSON.stringify({
            changes: { [`modelRoles.${role}`]: value },
            ...(scope ? { scope } : {}),
            directory,
          }),
        });
      } catch {
        return { ok: false, unavailable: false };
      }
      if (response.status === 404 || response.status === 501) return { ok: false, unavailable: true };
      let payload: unknown = null;
      try { payload = await response.json(); } catch { /* status is enough */ }
      if (!response.ok) {
        const parsed = z.object({
          rejected: z.array(z.object({ key: z.string(), reason: z.string().optional() })).optional(),
          error: z.string().optional(),
        }).safeParse(payload);
        const rejected = parsed.success
          ? (parsed.data.rejected?.[0]?.reason ?? parsed.data.error)
          : undefined;
        return { ok: false, unavailable: false, ...(rejected ? { rejected } : {}) };
      }
      const parsed = z.object({
        applied: z.record(z.string(), z.union([z.string(), z.null()])).default({}),
      }).safeParse(payload);
      if (!parsed.success) return { ok: false, unavailable: false };
      const applied = parsed.data.applied[`modelRoles.${role}`];
      return { ok: true, value: typeof applied === 'string' ? applied : null };
    },
  };
};

// ---------------------------------------------------------------------------
// Providers API — GUI CRUD over the engine's custom provider file
// (~/.omp/agent/models.yml; capability `providers.v1`). Credentials never
// travel back from the engine — file rows carry `hasApiKey` only.
// ---------------------------------------------------------------------------

export const OMP_PROVIDER_API_VALUES = [
  'openai-completions',
  'openai-responses',
  'openai-codex-responses',
  'azure-openai-responses',
  'anthropic-messages',
  'bedrock-converse-stream',
  'google-generative-ai',
  'google-gemini-cli',
  'google-vertex',
] as const;
export type OmpProviderApiValue = (typeof OMP_PROVIDER_API_VALUES)[number];

export const OMP_MODEL_EFFORTS = ['minimal', 'low', 'medium', 'high', 'xhigh', 'max'] as const;
export type OmpModelEffort = (typeof OMP_MODEL_EFFORTS)[number];

const OmpModelThinkingSchema = z.looseObject({
  efforts: z.array(z.string()).default([]),
  defaultLevel: z.string().optional(),
});

const OmpModelCostSchema = z.looseObject({
  input: z.number(),
  output: z.number(),
  cacheRead: z.number(),
  cacheWrite: z.number(),
});

const OmpProviderModelSchema = z.looseObject({
  id: z.string(),
  name: z.string().optional(),
  reasoning: z.boolean().optional(),
  contextWindow: z.number().optional(),
  maxTokens: z.number().optional(),
  thinking: OmpModelThinkingSchema.optional(),
  hasThinking: z.boolean().optional(),
  input: z.array(z.enum(['text', 'image'])).optional(),
  supportsTools: z.boolean().optional(),
  omitMaxOutputTokens: z.boolean().optional(),
  cost: OmpModelCostSchema.optional(),
  baseUrl: z.string().optional(),
  api: z.string().optional(),
  contextPromotionTarget: z.string().optional(),
  compactionModel: z.string().optional(),
});

const OmpFileProviderSchema = z.looseObject({
  id: z.string(),
  source: z.literal('file'),
  baseUrl: z.string().optional(),
  api: z.string().optional(),
  authHeader: z.boolean().optional(),
  headers: z.record(z.string(), z.string()).optional(),
  hasApiKey: z.boolean(),
  models: z.array(OmpProviderModelSchema),
});

const OmpEngineProviderSchema = z.looseObject({
  id: z.string(),
  source: z.literal('engine'),
  models: z.array(z.unknown()).default([]),
});

export type OmpProviderModelRow = z.infer<typeof OmpProviderModelSchema>;
export type OmpFileProvider = z.infer<typeof OmpFileProviderSchema>;
export type OmpEngineProviderRow = z.infer<typeof OmpEngineProviderSchema>;
export type OmpProviderRow = OmpFileProvider | OmpEngineProviderRow;

/** `thinking` replaces the block; `null` removes it; omit to keep the file's block. */
export interface OmpProviderModelThinkingInput {
  mode?: 'effort';
  efforts: string[];
  defaultLevel?: string;
}

export type OmpModelInputModality = 'text' | 'image';

export interface OmpModelCostInput {
  input: number;
  output: number;
  cacheRead: number;
  cacheWrite: number;
}

/**
 * Extended managed keys. `null` clears a key; omitted keys keep the file's
 * value server-side (hand-authored config survives untouched).
 */
export interface OmpProviderModelInput {
  id: string;
  name?: string;
  reasoning?: boolean;
  contextWindow?: number;
  maxTokens?: number;
  thinking?: OmpProviderModelThinkingInput | null;
  input?: OmpModelInputModality[] | null;
  supportsTools?: boolean | null;
  omitMaxOutputTokens?: boolean | null;
  cost?: OmpModelCostInput | null;
  baseUrl?: string | null;
  api?: string;
  contextPromotionTarget?: string | null;
  compactionModel?: string | null;
}

/** PUT payload. `null` clears a key; omitted keys keep the file's value. */
export interface OmpProviderInput {
  id: string;
  baseUrl?: string;
  api?: OmpProviderApiValue;
  apiKey?: string | null;
  authHeader?: boolean | null;
  headers?: Record<string, string> | null;
  models?: OmpProviderModelInput[] | null;
}

export type OmpProviderWriteResult =
  | { ok: true; provider: OmpFileProvider }
  | { ok: false; unavailable: boolean; message?: string };

export type OmpProviderDeleteResult =
  | { ok: true }
  | { ok: false; unavailable: boolean; message?: string };

const OmpProvidersListSchema = z.looseObject({
  providers: z.array(z.union([OmpFileProviderSchema, OmpEngineProviderSchema])),
});

export interface OmpProvidersAPI {
  /**
   * Engine providers tagged by origin: `file` rows (models.yml-defined) are
   * GUI-editable and carry config with a masked key (`hasApiKey`); `engine`
   * rows (builtin/login) are read-only. `unavailable` = providers.v1 off or
   * old engine.
   */
  listProviders(): Promise<OmpFetchJsonResult<OmpProviderRow[]>>;
  putProvider(input: { provider: OmpProviderInput }): Promise<OmpProviderWriteResult>;
  deleteProvider(id: string): Promise<OmpProviderDeleteResult>;
  /**
   * POST /omp/providers/{id}/fetch-models — the engine host queries the
   * provider's own model-list endpoint ({baseUrl}/models) server-side and
   * returns the ids (Cherry Studio / LobeChat "Fetch models" pattern).
   */
  fetchModels(id: string, options?: { baseUrl?: string; apiKey?: string }): Promise<OmpProviderFetchResult>;
}

export type OmpProviderFetchResult =
  | { ok: true; models: string[] }
  | { ok: false; unavailable: boolean; message?: string };

const parseOmpProviders = (value: unknown): OmpProviderRow[] | null => {
  const parsed = OmpProvidersListSchema.safeParse(value);
  return parsed.success ? parsed.data.providers as OmpProviderRow[] : null;
};
const OmpFileProviderPutResponseSchema = z.looseObject({ provider: OmpFileProviderSchema });

export const createOmpProvidersAPI = (apiOptions: OmpJsonApiOptions = {}): OmpProvidersAPI => {
  const fetchImpl = apiOptions.fetchImpl ?? runtimeFetch;
  const writeErrorPayload = async (response: Response): Promise<{ unavailable: boolean; message?: string }> => {
    let payload: unknown = null;
    try { payload = await response.json(); } catch { /* status is enough */ }
    if (response.status === 404 || response.status === 501) return { unavailable: true };
    const parsed = z.object({ message: z.string().optional(), error: z.string().optional() }).safeParse(payload);
    const message = parsed.success ? (parsed.data.message ?? parsed.data.error) : undefined;
    return { unavailable: false, ...(message ? { message } : {}) };
  };
  return {
    listProviders() {
      return ompFetchJson(fetchImpl, OMP_ENDPOINTS.providers, parseOmpProviders);
    },
    async putProvider({ provider }) {
      let response: Response;
      try {
        response = await fetchImpl(OMP_ENDPOINTS.providers, {
          method: 'PUT',
          headers: { Accept: 'application/json', 'Content-Type': 'application/json' },
          body: JSON.stringify({ provider }),
        });
      } catch {
        return { ok: false, unavailable: false };
      }
      if (response.status === 404 || response.status === 501) return { ok: false, unavailable: true };
      if (!response.ok) return { ok: false, ...(await writeErrorPayload(response)) };
      const parsed = OmpFileProviderPutResponseSchema.safeParse(await response.json().catch(() => null));
      return parsed.success
        ? { ok: true, provider: parsed.data.provider }
        : { ok: false, unavailable: false };
    },
    async fetchModels(id, options = {}) {
      let response: Response;
      try {
        response = await fetchImpl(OMP_ENDPOINTS.providerFetch(id), {
          method: 'POST',
          headers: { Accept: 'application/json', 'Content-Type': 'application/json' },
          // Draft overrides let an unsaved create/edit fetch before its first
          // save (the server falls back to the persisted provider otherwise).
          body: JSON.stringify({
            ...(options.baseUrl ? { baseUrl: options.baseUrl } : {}),
            ...(options.apiKey ? { apiKey: options.apiKey } : {}),
          }),
        });
      } catch {
        return { ok: false, unavailable: false };
      }
      if (response.status === 404 || response.status === 501) return { ok: false, unavailable: true };
      if (!response.ok) return { ok: false, ...(await writeErrorPayload(response)) };
      const parsed = z.looseObject({ models: z.array(z.string()).default([]) }).safeParse(await response.json().catch(() => null));
      return parsed.success
        ? { ok: true, models: parsed.data.models }
        : { ok: false, unavailable: false };
    },
    async deleteProvider(id) {
      let response: Response;
      try {
        response = await fetchImpl(OMP_ENDPOINTS.provider(id), {
          method: 'DELETE',
          headers: { Accept: 'application/json' },
        });
      } catch {
        return { ok: false, unavailable: false };
      }
      if (response.status === 404 || response.status === 501) return { ok: false, unavailable: true };
      if (!response.ok) return { ok: false, ...(await writeErrorPayload(response)) };
      return { ok: true };
    },
  };
};

// ---------------------------------------------------------------------------
// Events API — SSE over runtimeFetch with the relay-transport reconnect
// discipline (exponential backoff, offline/hidden long backoff, permanent 4xx
// long backoff, interruptible waits).
// ---------------------------------------------------------------------------

export interface OmpStreamHandlers {
  onEvent: (envelope: OmpEventEnvelope) => void;
  /** Control frame: the durable ring could not bridge from `lastEventId`. */
  onResync: (payload: OmpStreamResyncPayload) => void;
  /** Fired on every successful stream connection, including the first. */
  onReconnect?: () => void;
  /** Fired once per disconnection cycle (not per failed attempt). */
  onDisconnect?: (reason: string) => void;
}

export interface OmpEventsSubscription {
  close: () => void;
}

export interface OmpEventsAPI {
  /**
   * Subscribes to the omp event stream. `directory === null` subscribes to
   * all directories (envelopes carry their own `directory` for routing).
   * Reconnects indefinitely until closed.
   */
  subscribeEvents(directory: string | null, handlers: OmpStreamHandlers): OmpEventsSubscription;
}

const RETRY_BACKOFF_BASE_MS = 250;
const RETRY_BACKOFF_CAP_VISIBLE_MS = 5_000;
const RETRY_BACKOFF_CAP_HIDDEN_OR_OFFLINE_MS = 60_000;
const RETRY_BACKOFF_MAX_EXPONENT = 8;
const DEFAULT_HEARTBEAT_TIMEOUT_MS = 45_000;
const RESYNC_EVENT_NAME = 'omp.stream.resync';

const isOffline = (): boolean =>
  typeof navigator === 'object' && navigator !== null && navigator.onLine === false;

const isHidden = (): boolean =>
  typeof document !== 'undefined' && document.visibilityState !== 'visible';

const isAbortError = (error: unknown): boolean =>
  (error instanceof DOMException && error.name === 'AbortError')
  || (typeof error === 'object' && error !== null && (error as { name?: unknown }).name === 'AbortError');

const extractStatus = (error: unknown): number | undefined => {
  if (!error || typeof error !== 'object') return undefined;
  const direct = (error as { status?: unknown }).status;
  if (typeof direct === 'number') return direct;
  const fromResponse = (error as { response?: { status?: unknown } }).response?.status;
  return typeof fromResponse === 'number' ? fromResponse : undefined;
};

// 408/429 stay on the normal exponential path; other 4xx are permanent-class
// failures only a server-side fix or reauth resolves, so they take the long
// cap immediately (mirrors the wire pipeline's policy).
const isPermanentHttpStatus = (status: number): boolean => {
  if (status < 400 || status >= 500) return false;
  return status !== 408 && status !== 429;
};

const parseEnvelope = (value: unknown): OmpEventEnvelope | null => {
  if (!value || typeof value !== 'object') return null;
  const record = value as Record<string, unknown>;
  if (typeof record.id !== 'number' || typeof record.type !== 'string' || typeof record.directory !== 'string') {
    return null;
  }
  return {
    id: record.id,
    type: record.type,
    directory: record.directory,
    ...(typeof record.sessionID === 'string' && record.sessionID.length > 0 ? { sessionID: record.sessionID } : {}),
    schemaVersion: typeof record.schemaVersion === 'string' ? record.schemaVersion : '',
    createdAt: typeof record.createdAt === 'number' ? record.createdAt : 0,
    payload: 'payload' in record ? record.payload : undefined,
  };
};

export const parseOmpEnvelope = parseEnvelope;

interface SseFrame {
  eventName?: string;
  data?: string;
}

/** Parses one SSE block (already split on blank-line boundaries); comments are ignored. */
export const parseOmpSseBlock = (block: string): SseFrame | null => {
  const lines = block.split('\n');
  let eventName: string | undefined;
  let data: string | undefined;
  for (const line of lines) {
    if (line.startsWith(':')) continue;
    if (line.startsWith('data:')) {
      const chunk = line.replace(/^data:\s?/, '');
      data = data === undefined ? chunk : `${data}\n${chunk}`;
    } else if (line.startsWith('event:')) {
      eventName = line.replace(/^event:\s?/, '');
    }
  }
  return eventName === undefined && data === undefined ? null : { eventName, data };
};

const safeJsonParse = (raw: string): unknown => {
  try {
    return JSON.parse(raw) as unknown;
  } catch {
    return null;
  }
};

const waitForRetry = (signal: AbortSignal, ms: number): Promise<void> =>
  new Promise<void>((resolve) => {
    if (ms <= 0 || signal.aborted) {
      resolve();
      return;
    }
    let timer: ReturnType<typeof setTimeout> | undefined;
    const onInterrupt = () => {
      if (timer !== undefined) clearTimeout(timer);
      if (typeof globalThis.window !== 'undefined') {
        globalThis.window.removeEventListener('online', onInterrupt);
      }
      if (typeof document !== 'undefined') {
        document.removeEventListener('visibilitychange', onVisibilityInterrupt);
      }
      signal.removeEventListener('abort', onInterrupt);
      resolve();
    };
    const onVisibilityInterrupt = () => {
      if (typeof document !== 'undefined' && document.visibilityState === 'visible') {
        onInterrupt();
      }
    };
    timer = setTimeout(onInterrupt, ms);
    if (typeof globalThis.window !== 'undefined') {
      globalThis.window.addEventListener('online', onInterrupt, { once: true });
    }
    if (typeof document !== 'undefined') {
      document.addEventListener('visibilitychange', onVisibilityInterrupt);
    }
    signal.addEventListener('abort', onInterrupt, { once: true });
  });

export interface OmpEventsApiOptions {
  /** Injection seam for tests; production callers omit it. */
  fetchImpl?: typeof runtimeFetch;
  now?: () => number;
  heartbeatTimeoutMs?: number;
}

export const createOmpEventsAPI = (options: OmpEventsApiOptions = {}): OmpEventsAPI => {
  const fetchImpl = options.fetchImpl ?? runtimeFetch;
  const now = options.now ?? Date.now;
  const heartbeatTimeoutMs = options.heartbeatTimeoutMs ?? DEFAULT_HEARTBEAT_TIMEOUT_MS;

  const computeRetryDelay = (failures: number): number => {
    if (failures <= 0) return 0;
    if (isOffline()) return RETRY_BACKOFF_CAP_HIDDEN_OR_OFFLINE_MS;
    const cap = isHidden() ? RETRY_BACKOFF_CAP_HIDDEN_OR_OFFLINE_MS : RETRY_BACKOFF_CAP_VISIBLE_MS;
    const exponent = Math.min(failures - 1, RETRY_BACKOFF_MAX_EXPONENT);
    return Math.min(cap, RETRY_BACKOFF_BASE_MS * 2 ** exponent);
  };

  return {
    subscribeEvents(directory, handlers) {
      const lifecycle = new AbortController();
      let lastEventId: number | null = null;
      let heartbeatTimer: ReturnType<typeof setTimeout> | undefined;
      let consecutiveFailures = 0;
      let disconnected = false;

      const notifyDisconnected = (reason: string) => {
        if (disconnected) return;
        disconnected = true;
        handlers.onDisconnect?.(reason);
      };

      /** One SSE connection attempt; resolves on stream end, throws on failure. */
      const runAttempt = async (attempt: AbortController): Promise<void> => {
        const headers: Record<string, string> = { Accept: 'text/event-stream' };
        if (lastEventId !== null) {
          headers['Last-Event-ID'] = String(lastEventId);
        }
        const response = await fetchImpl(OMP_ENDPOINTS.events, {
          method: 'GET',
          headers,
          ...(directory ? { query: { directory } } : {}),
          signal: attempt.signal,
        } as RuntimeFetchOptions);
        if (!response.ok) {
          const error = new Error(`omp events stream failed: ${response.status} ${response.statusText}`);
          (error as Error & { status?: number }).status = response.status;
          throw error;
        }
        if (!response.body) {
          throw new Error('omp events stream returned no body');
        }

        const reader = response.body.pipeThrough(new TextDecoderStream()).getReader();
        const clearHeartbeat = () => {
          if (heartbeatTimer !== undefined) {
            clearTimeout(heartbeatTimer);
            heartbeatTimer = undefined;
          }
        };
        const resetHeartbeat = () => {
          clearHeartbeat();
          // Quiet stream: abort so the reconnect loop rebuilds with
          // Last-Event-ID instead of sitting on a dead connection forever.
          heartbeatTimer = setTimeout(() => attempt.abort(), heartbeatTimeoutMs);
        };

        let announcedConnected = false;
        let buffer = '';
        try {
          for (;;) {
            const { done, value } = await reader.read();
            if (done) {
              throw new Error('omp events stream closed');
            }
            resetHeartbeat();
            if (!announcedConnected) {
              announcedConnected = true;
              disconnected = false;
              consecutiveFailures = 0;
              handlers.onReconnect?.();
            }
            buffer += value.replace(/\r\n/g, '\n').replace(/\r/g, '\n');
            const blocks = buffer.split('\n\n');
            buffer = blocks.pop() ?? '';
            for (const block of blocks) {
              const frame = parseOmpSseBlock(block);
              if (frame === null) continue;
              if (frame.eventName === RESYNC_EVENT_NAME) {
                const envelope = frame.data !== undefined ? parseEnvelope(safeJsonParse(frame.data)) : null;
                const payload = envelope?.payload;
                if (payload && typeof payload === 'object' && Array.isArray((payload as { scope?: unknown }).scope)) {
                  const scopeRecord = payload as { scope: unknown[]; lastEventId?: unknown };
                  handlers.onResync({
                    scope: scopeRecord.scope.filter((name): name is string => typeof name === 'string'),
                    lastEventId: typeof scopeRecord.lastEventId === 'number' ? scopeRecord.lastEventId : null,
                  });
                }
                continue;
              }
              if (frame.data === undefined) continue;
              const envelope = parseEnvelope(safeJsonParse(frame.data));
              if (envelope === null) continue;
              lastEventId = envelope.id;
              handlers.onEvent(envelope);
            }
          }
        } finally {
          clearHeartbeat();
          reader.releaseLock();
        }
      };

      void (async () => {
        while (!lifecycle.signal.aborted) {
          const attempt = new AbortController();
          let retryDelayMs = 0;
          try {
            await runAttempt(attempt);
          } catch (error) {
            if (!isAbortError(error)) {
              consecutiveFailures += 1;
              notifyDisconnected(error instanceof Error ? error.message : 'omp events stream error');
              const status = extractStatus(error);
              retryDelayMs = status !== undefined && isPermanentHttpStatus(status)
                ? RETRY_BACKOFF_CAP_HIDDEN_OR_OFFLINE_MS
                : computeRetryDelay(consecutiveFailures);
            }
          }
          if (lifecycle.signal.aborted) return;
          if (retryDelayMs > 0) {
            await waitForRetry(lifecycle.signal, retryDelayMs);
          }
        }
      })();

      return {
        close: () => {
          lifecycle.abort();
        },
      };
    },
  };
};

// ---------------------------------------------------------------------------
// Dialogs API (spec 03 §5.2 — approval/ask bridge: lease + respond + snapshot)
// ---------------------------------------------------------------------------

export const OMP_DIALOG_KINDS = ['approval', 'select', 'confirm', 'input', 'editor', 'ask'] as const;
export type OmpDialogKind = (typeof OMP_DIALOG_KINDS)[number];

export interface OmpApprovalDialogPayload {
  prompt: string;
  approvalMode?: string;
  toolName?: string;
  toolCallId?: string;
  tier?: string;
  reason?: string;
}

export interface OmpSelectDialogPayload {
  title: string;
  options: string[];
}

export interface OmpConfirmDialogPayload {
  title: string;
  message?: string;
}

export interface OmpInputDialogPayload {
  title: string;
  placeholder?: string;
}

export interface OmpAskOption {
  label: string;
  description?: string;
  preview?: string;
}

export interface OmpAskQuestion {
  id: string;
  question: string;
  header?: string;
  options: OmpAskOption[];
  multi?: boolean;
  recommended?: string;
}

export interface OmpAskDialogPayload {
  questions: OmpAskQuestion[];
  timeoutMs: number;
}

/** Public OmpDialog projection (server snapshotDialog; internals never leak). */
export type OmpPendingDialog = {
  id: string;
  sessionId: string;
  createdAt: number;
  presentedAt?: number;
} & (
  | { kind: 'approval'; approval: OmpApprovalDialogPayload }
  | { kind: 'select'; select: OmpSelectDialogPayload }
  | { kind: 'confirm'; confirm: OmpConfirmDialogPayload }
  | { kind: 'input'; input: OmpInputDialogPayload }
  | { kind: 'editor'; editor: OmpInputDialogPayload }
  | { kind: 'ask'; ask: OmpAskDialogPayload }
);

const DialogBaseSchema = z.object({
  id: z.string().min(1),
  sessionId: z.string().min(1),
  createdAt: z.number(),
  presentedAt: z.number().optional(),
  kind: z.enum(OMP_DIALOG_KINDS),
});

const ApprovalPayloadSchema = z.object({
  prompt: z.string(),
  approvalMode: z.string().optional(),
  toolName: z.string().optional(),
  toolCallId: z.string().optional(),
  tier: z.string().optional(),
  reason: z.string().optional(),
});

const SelectPayloadSchema = z.object({
  title: z.string(),
  options: z.array(z.string()),
});

const ConfirmPayloadSchema = z.object({
  title: z.string(),
  message: z.string().optional(),
});

const InputPayloadSchema = z.object({
  title: z.string(),
  placeholder: z.string().optional(),
});

const AskPayloadSchema = z.object({
  questions: z.array(z.object({
    id: z.string().min(1),
    question: z.string(),
    header: z.string().optional(),
    options: z.array(z.object({
      label: z.string(),
      description: z.string().optional(),
      preview: z.string().optional(),
    })),
    multi: z.boolean().optional(),
    recommended: z.string().optional(),
  })),
  timeoutMs: z.number(),
});

/**
 * Parses one dialog projection. Kind decides which payload key is required;
 * a mismatched shape is failure (never a half-dialog), mirroring the
 * registry's RESPOND_KINDS contract.
 */
export const parseOmpPendingDialog = (value: unknown): OmpPendingDialog | null => {
  const base = DialogBaseSchema.safeParse(value);
  if (!base.success) return null;
  const raw = value as Record<string, unknown>;
  switch (base.data.kind) {
    case 'approval': {
      const payload = ApprovalPayloadSchema.safeParse(raw.approval);
      return payload.success ? { ...base.data, kind: 'approval', approval: payload.data } : null;
    }
    case 'select': {
      const payload = SelectPayloadSchema.safeParse(raw.select);
      return payload.success ? { ...base.data, kind: 'select', select: payload.data } : null;
    }
    case 'confirm': {
      const payload = ConfirmPayloadSchema.safeParse(raw.confirm);
      return payload.success ? { ...base.data, kind: 'confirm', confirm: payload.data } : null;
    }
    case 'input':
    case 'editor': {
      const payload = InputPayloadSchema.safeParse(raw[base.data.kind]);
      return payload.success
        ? { ...base.data, kind: base.data.kind, [base.data.kind]: payload.data } as OmpPendingDialog
        : null;
    }
    case 'ask': {
      const payload = AskPayloadSchema.safeParse(raw.ask);
      return payload.success ? { ...base.data, kind: 'ask', ask: payload.data } : null;
    }
  }
};

const DialogsSnapshotSchema = z.object({
  dialogs: z.array(z.unknown()),
});

/**
 * Parses a `GET /api/omp/dialogs` payload into trusted dialog records.
 * Unparseable entries are dropped (the server projection is the authority;
 * a half-shape is never a dialog) — null means the whole payload was
 * malformed, which callers treat as fetch failure, never empty success.
 */
export const parseOmpDialogsSnapshotPayload = (value: unknown): OmpPendingDialog[] | null => {
  const parsed = DialogsSnapshotSchema.safeParse(value);
  if (!parsed.success) return null;
  const dialogs: OmpPendingDialog[] = [];
  for (const item of parsed.data.dialogs) {
    const dialog = parseOmpPendingDialog(item);
    if (dialog !== null) dialogs.push(dialog);
  }
  return dialogs;
};

const ChromeSnapshotSchema = z.object({
  revision: z.number(),
  widgets: z.array(z.unknown()),
  status: z.array(z.unknown()),
  dropped: z.record(z.string(), z.number()).optional(),
});

const ChromeWidgetSchema = z.object({
  key: z.string().min(1),
  lines: z.array(z.string()),
  placement: z.enum(['aboveEditor', 'belowEditor']).optional(),
  sessionId: z.string(),
  updatedAt: z.number(),
});

const ChromeStatusSchema = z.object({
  key: z.string().min(1),
  text: z.string(),
  sessionId: z.string(),
  updatedAt: z.number(),
});

export interface OmpChromeSnapshot {
  widgets: Array<{ key: string; lines: string[]; placement?: 'aboveEditor' | 'belowEditor'; sessionId: string; updatedAt: number }>;
  status: Array<{ key: string; text: string; sessionId: string; updatedAt: number }>;
}

/**
 * Parses a `GET /api/omp/chrome` payload into trusted chrome records
 * (spec 09 §5.0). Unparseable entries are dropped; null means the whole
 * payload was malformed — callers treat that as fetch failure, never as an
 * authoritative empty success (D2).
 */
export const parseOmpChromeSnapshotPayload = (value: unknown): OmpChromeSnapshot | null => {
  const parsed = ChromeSnapshotSchema.safeParse(value);
  if (!parsed.success) return null;
  const widgets: OmpChromeSnapshot['widgets'] = [];
  for (const item of parsed.data.widgets) {
    const widget = ChromeWidgetSchema.safeParse(item);
    if (widget.success) widgets.push(widget.data);
  }
  const status: OmpChromeSnapshot['status'] = [];
  for (const item of parsed.data.status) {
    const row = ChromeStatusSchema.safeParse(item);
    if (row.success) status.push(row.data);
  }
  return { widgets, status };
};

/** RespondResult union (server RESPOND_KINDS is the authority). */
export type OmpDialogRespondResult =
  | { kind: 'select'; value?: string }
  | { kind: 'cancel' }
  | { kind: 'confirm'; value: boolean }
  | { kind: 'input'; value?: string }
  | { kind: 'editor'; value?: string }
  | {
    kind: 'ask';
    results: Array<{ id: string; selectedOptions: string[]; customInput?: string; note?: string }>;
  }
  | { kind: 'chat' };

export type OmpDialogsSnapshotResult =
  | { ok: true; dialogs: OmpPendingDialog[] }
  | { ok: false; unavailable: boolean };

export type OmpDialogMutationResult =
  | { ok: true; outcome?: string }
  | { ok: false; unavailable: boolean; status?: number; error?: string; outcome?: string };

export interface OmpLeaseInfo {
  leaseId: string;
  expiresAt: number;
  heartbeatIntervalMs: number;
}

export type OmpLeaseResult =
  | { ok: true; lease: OmpLeaseInfo }
  | { ok: false; unavailable: boolean };

export interface OmpDialogsAPI {
  getSnapshot(directory: string): Promise<OmpDialogsSnapshotResult>;
  acquireLease(input: { directory: string; sessionId: string; clientId: string }): Promise<OmpLeaseResult>;
  releaseLease(input: { directory: string; sessionId: string; clientId: string }): Promise<OmpDialogMutationResult>;
  presented(directory: string, dialogId: string): Promise<{ ok: true; presentedAt: number } | { ok: false; unavailable: boolean }>;
  respond(
    directory: string,
    dialogId: string,
    result: OmpDialogRespondResult,
    clientId?: string,
  ): Promise<OmpDialogMutationResult>;
  abort(directory: string, dialogId: string): Promise<OmpDialogMutationResult>;
}

export const createOmpDialogsAPI = (apiOptions: OmpJsonApiOptions = {}): OmpDialogsAPI => {
  const fetchImpl = apiOptions.fetchImpl ?? runtimeFetch;

  const post = async (
    path: string,
    body: Record<string, unknown>,
  ): Promise<{ response?: Response; unavailable: boolean; status: number; payload: unknown }> => {
    try {
      const response = await fetchImpl(path, {
        method: 'POST',
        headers: { Accept: 'application/json', 'Content-Type': 'application/json' },
        body: JSON.stringify(body),
      });
      if (response.status === 404 || response.status === 501 || response.status === 503) {
        return { unavailable: true, status: response.status, payload: null };
      }
      let payload: unknown = null;
      try {
        payload = await response.json();
      } catch {
        payload = null;
      }
      return { response, unavailable: false, status: response.status, payload };
    } catch {
      return { unavailable: false, status: 0, payload: null };
    }
  };

  const mutationError = (status: number, payload: unknown): OmpDialogMutationResult => {
    const parsed = z.object({ error: z.string().optional(), outcome: z.string().optional() }).safeParse(payload);
    return {
      ok: false,
      unavailable: false,
      status,
      ...(parsed.success && parsed.data.error !== undefined ? { error: parsed.data.error } : {}),
      ...(parsed.success && parsed.data.outcome !== undefined ? { outcome: parsed.data.outcome } : {}),
    };
  };

  return {
    async getSnapshot(directory) {
      let response: Response;
      try {
        response = await fetchImpl(OMP_ENDPOINTS.dialogs, {
          method: 'GET',
          headers: { Accept: 'application/json' },
          query: { directory },
        });
      } catch {
        return { ok: false, unavailable: false };
      }
      if (response.status === 404 || response.status === 501) {
        return { ok: false, unavailable: true };
      }
      if (!response.ok) {
        return { ok: false, unavailable: false };
      }
      let value: unknown;
      try {
        value = await response.json();
      } catch {
        return { ok: false, unavailable: false };
      }
      const dialogs = parseOmpDialogsSnapshotPayload(value);
      return dialogs === null ? { ok: false, unavailable: false } : { ok: true, dialogs };
    },

    async acquireLease(input) {
      const { unavailable, status, response, payload } = await post(OMP_ENDPOINTS.dialogsLease, { ...input });
      if (unavailable) return { ok: false, unavailable: true };
      if (response === undefined || !response.ok) return { ok: false, unavailable: false, ...(status ? { status } : {}) } as OmpLeaseResult;
      const parsed = z.object({
        leaseId: z.string(),
        expiresAt: z.number(),
        heartbeatIntervalMs: z.number(),
      }).safeParse(payload);
      if (!parsed.success) return { ok: false, unavailable: false };
      return { ok: true, lease: parsed.data };
    },

    async releaseLease(input) {
      const { unavailable, response, status, payload } = await post(OMP_ENDPOINTS.dialogsLeaseRelease, { ...input });
      if (unavailable) return { ok: false, unavailable: true };
      if (response === undefined || !response.ok) return mutationError(status, payload);
      return { ok: true };
    },

    async presented(directory, dialogId) {
      const { unavailable, response, status, payload } = await post(OMP_ENDPOINTS.dialogPresented(dialogId), { directory });
      if (unavailable) return { ok: false, unavailable: true };
      if (response === undefined || !response.ok) return { ok: false, unavailable: false };
      const parsed = z.object({ presentedAt: z.number() }).safeParse(payload);
      return parsed.success ? { ok: true, presentedAt: parsed.data.presentedAt } : { ok: false, unavailable: false };
    },

    async respond(directory, dialogId, result, clientId) {
      const { unavailable, response, status, payload } = await post(OMP_ENDPOINTS.dialogRespond(dialogId), {
        directory,
        ...(clientId !== undefined ? { clientId } : {}),
        result,
      });
      if (unavailable) return { ok: false, unavailable: true };
      if (response === undefined || !response.ok) return mutationError(status, payload);
      const parsed = z.object({ outcome: z.string().optional() }).loose().safeParse(payload);
      return { ok: true, ...(parsed.success && parsed.data.outcome !== undefined ? { outcome: parsed.data.outcome } : {}) };
    },

    async abort(directory, dialogId) {
      const { unavailable, response, status, payload } = await post(OMP_ENDPOINTS.dialogAbort(dialogId), { directory });
      if (unavailable) return { ok: false, unavailable: true };
      if (response === undefined || !response.ok) return mutationError(status, payload);
      return { ok: true };
    },
  };
};

// ---------------------------------------------------------------------------
// URI bridge API — internal-URI resolve + token redemption (spec 04 §5.2.1-
// §5.2.4; P1 = 'C:\Users\reamd\.omp\agent\sessions\-Documents-experiment_area-ompchamber\2026-08-20T03-54-19-322Z_01a01d4e-3339-7000-b114-20d6ff429062\local' read only, session-pinned, sourcePath never returned)
// ---------------------------------------------------------------------------

/** Opaque resource token minted by resolve (04 §5.2.4; no path inside). */
export interface OmpUriToken {
  id: string;
  expiresAt: number;
}


/** POST /api/omp/uri/resolve payload — InternalResource minus sourcePath, plus the token. */
export interface OmpUriResource {
  url: string;
  /** Text body. Absent for previewable binaries — those render via fetchContent. */
  content?: string;
  contentType: string;
  size?: number;
  immutable?: boolean;
  /** Previewable binary (image): render via fetchContent bytes, not `content`. */
  binary?: boolean;
  isDirectory?: boolean;
  notes?: string[];
  token?: OmpUriToken;
}
/** POST /api/omp/uri/open payload — token redemption result (same content shape, no token). */
export interface OmpUriOpenPayload {
  url: string;
  content: string;
  size: number;
  filename: string;
  editable: boolean;
  contentType?: string;
  immutable?: boolean;
}
/**
 * Failure half shared by resolve/open. Only 501 maps to `unavailable: true`
 * (uri.v1 off / scheme not enabled — the surface is not offered). A 404 here
 * is a legitimate user-facing resolve failure (traversal, not-found, unknown
 * session) whose `message` is the handler's own contract text, and a 413
 * carries the offending `size` — the viewer surfaces both inline.
 */
export type OmpUriFailure =
  | { ok: false; unavailable: true }
  | { ok: false; unavailable: false; status?: number; error?: string; message?: string; size?: number };

export type OmpUriResolveResult = { ok: true; resource: OmpUriResource } | OmpUriFailure;
export type OmpUriOpenResult = { ok: true; payload: OmpUriOpenPayload } | OmpUriFailure;

const UriTokenSchema = z.object({ id: z.string().min(1), expiresAt: z.number() });

const UriResourceSchema = z.looseObject({
  url: z.string().min(1),
  /** Text body; absent for previewable binaries (fetched as bytes separately). */
  content: z.string().optional(),
  contentType: z.string().min(1),
  binary: z.boolean().optional(),
  immutable: z.boolean().optional(),
  isDirectory: z.boolean().optional(),
  notes: z.array(z.string()).optional(),
  token: UriTokenSchema.optional(),
});

const UriOpenSchema = z.looseObject({
  url: z.string().min(1),
  content: z.string(),
  size: z.number(),
  filename: z.string(),
  editable: z.boolean(),
  contentType: z.string().optional(),
  immutable: z.boolean().optional(),
});

const UriErrorSchema = z.looseObject({
  error: z.string().optional(),
  message: z.string().optional(),
  size: z.number().optional(),
});

const uriFailure = (status: number | undefined, payload: unknown): Extract<OmpUriFailure, { unavailable: false }> => {
  const parsed = UriErrorSchema.safeParse(payload);
  if (!parsed.success || (parsed.data.error === undefined && parsed.data.message === undefined && parsed.data.size === undefined)) {
    // status 0 = transport failure (no response); nothing meaningful to attach.
    return { ok: false, unavailable: false, ...(typeof status === 'number' && status > 0 ? { status } : {}) };
  }
  const { error, message, size } = parsed.data;
  return {
    ok: false,
    unavailable: false,
    ...(typeof status === 'number' && status > 0 ? { status } : {}),
    ...(error !== undefined ? { error } : {}),
    ...(message !== undefined ? { message } : {}),
    ...(typeof size === 'number' ? { size } : {}),
  };
};

export interface OmpUriAPI {
  /**
   * POST /api/omp/uri/resolve (04 §5.2.1): session-pinned host-side
   * resolution. `sessionID` + `directory` are required by the endpoint —
   * the local:// root is private to that session.
   */
  resolve(options: {
    url: string;
    sessionID: string;
    directory: string;
    pathOnly?: boolean;
  }): Promise<OmpUriResolveResult>;
  /**
   * POST /api/omp/uri/open (04 §5.2.4): redeem a resolve-issued resource
   * token for content. `directory` must match the issuing directory
   * (server-side defense in depth); expired/exhausted tokens 404.
   */
  open(options: { token: string; directory: string }): Promise<OmpUriOpenResult>;
  /**
   * GET /api/omp/uri/tokens/{id}/content (04 §5.2.4): redeem a resolve-issued
   * token for RAW bytes (previewable images). Small-asset pattern: fetch via
   * runtimeFetch, read a blob, render an object URL — no URL-token allowlist.
   */
  fetchContent(options: { token: string; directory: string }): Promise<{ ok: true; blob: Blob; contentType?: string } | OmpUriFailure>;
}

export const createOmpUriAPI = (apiOptions: OmpJsonApiOptions = {}): OmpUriAPI => {
  const fetchImpl = apiOptions.fetchImpl ?? runtimeFetch;

  const post = async (
    path: string,
    body: Record<string, unknown>,
  ): Promise<{ response?: Response; unavailable: boolean; status: number; payload: unknown }> => {
    try {
      const response = await fetchImpl(path, {
        method: 'POST',
        headers: { Accept: 'application/json', 'Content-Type': 'application/json' },
        body: JSON.stringify(body),
      });
      if (response.status === 501) {
        return { unavailable: true, status: response.status, payload: null };
      }
      let payload: unknown = null;
      try {
        payload = await response.json();
      } catch {
        payload = null;
      }
      return { response, unavailable: false, status: response.status, payload };
    } catch {
      return { unavailable: false, status: 0, payload: null };
    }
  };

  return {
    async resolve({ url, sessionID, directory, pathOnly }) {
      const { response, unavailable, status, payload } = await post(OMP_ENDPOINTS.uriResolve, {
        u: url,
        sessionID,
        directory,
        ...(pathOnly ? { pathOnly: true } : {}),
      });
      if (unavailable) return { ok: false, unavailable: true };
      if (response === undefined || !response.ok) return uriFailure(status, payload);
      const parsed = UriResourceSchema.safeParse(payload);
      return parsed.success
        ? { ok: true, resource: parsed.data }
        : { ok: false, unavailable: false, status };
    },

    async open({ token, directory }) {
      const { response, unavailable, status, payload } = await post(OMP_ENDPOINTS.uriOpen, { token, directory });
      if (unavailable) return { ok: false, unavailable: true };
      if (response === undefined || !response.ok) return uriFailure(status, payload);
      const parsed = UriOpenSchema.safeParse(payload);
      return parsed.success
        ? { ok: true, payload: parsed.data }
        : { ok: false, unavailable: false, status };
    },

    async fetchContent({ token, directory }) {
      let response;
      try {
        response = await fetchImpl(OMP_ENDPOINTS.uriTokenContent(token), {
          method: 'GET',
          headers: { Accept: '*/*' },
          query: { directory },
        });
      } catch {
        return { ok: false, unavailable: false };
      }
      if (response.status === 501) return { ok: false, unavailable: true };
      if (!response.ok) {
        let payload: { error?: string; message?: string; size?: number } | null = null;
        try {
          payload = await response.json();
        } catch {
          payload = null;
        }
        return uriFailure(response.status, payload);
      }
      return { ok: true, blob: await response.blob(), contentType: response.headers.get('content-type') ?? undefined };
    },
  };
};

// ---------------------------------------------------------------------------
// Artifacts browse API — host-level read-only local:// listing (spec 04;
// capability `artifacts`). Refs are relative to each session's private
// local:// root; responses never carry absolute paths.
// ---------------------------------------------------------------------------

/** One file row: `ref` is the local:// suffix (e.g. 'PLAN.md', 'scratch/notes.md'). */
export interface OmpArtifactsFileRow {
  ref: string;
  size: number;
  modifiedAt: number;
}

export type OmpArtifactsFilesResult =
  | { ok: true; files: OmpArtifactsFileRow[]; truncated: boolean }
  | OmpUriFailure;

const ArtifactsFilesSchema = z.looseObject({
  directory: z.string(),
  sessionID: z.string(),
  files: z.array(
    z.looseObject({
      ref: z.string().min(1),
      size: z.number(),
      modifiedAt: z.number(),
    }),
  ),
  truncated: z.boolean(),
});

export interface OmpArtifactsAPI {
  /**
   * GET /api/omp/artifacts?directory=&sessionID= (spec 04): ONE session's
   * file rows, mtime desc — the browser is per-session by design. `ok:false`
   * with 404 means the session is unknown to the directory (deleted while
   * browsing); failure is never empty success.
   */
  listSessionArtifacts(options: { directory: string; sessionID: string }): Promise<OmpArtifactsFilesResult>;
}


/**
 * GET /api/omp/artifacts — payload misses (absent/malformed JSON) are
 * payload-level nulls, not transport failures; 501 maps to unavailable.
 */
export const createOmpArtifactsAPI = (apiOptions: OmpJsonApiOptions = {}): OmpArtifactsAPI => {
  const fetchImpl = apiOptions.fetchImpl ?? runtimeFetch;

  const get = async (query: { directory: string; sessionID?: string }) => {
    try {
      const response = await fetchImpl(OMP_ENDPOINTS.artifacts, {
        method: 'GET',
        headers: { Accept: 'application/json' },
        query,
      });
      if (response.status === 501) {
        return { unavailable: true, status: response.status, payload: null };
      }
      const payload = await response.json().catch(() => null);
      return { unavailable: false, status: response.status, payload, ok: response.ok };
    } catch {
      return { unavailable: false, status: 0, payload: null, ok: false };
    }
  };
  return {
    async listSessionArtifacts({ directory, sessionID }) {
      const { unavailable, status, payload, ok } = await get({ directory, sessionID });
      if (unavailable) return { ok: false, unavailable: true };
      if (!ok) return uriFailure(status, payload);
      const parsed = ArtifactsFilesSchema.safeParse(payload);
      return parsed.success
        ? { ok: true, files: parsed.data.files, truncated: parsed.data.truncated }
        : { ok: false, unavailable: false, status };
    },
  };
};

// ---------------------------------------------------------------------------
// Session tree API — fork-lineage snapshot (spec 04 §5.4.1; capability
// tree.v1). The mounted GET returns the SESSION-level fork tree
// {leafId, nodes:[{id,parentId,title,time}]}: the ancestor chain of the
// session plus every descendant fork. Selecting a node re-pulls that
// session's timeline through the normal session-switch machinery (navigate
// is not exposed by the tree domain yet — selection semantics fold there).
// ---------------------------------------------------------------------------

export interface OmpSessionTreeNode {
  id: string;
  parentId: string | null;
  title: string;
  time: { created: number; updated: number };
}

/** GET /api/omp/sessions/{id}/tree payload (domain-uri.js buildSessionSubtree). */
export interface OmpSessionTreeSnapshot {
  leafId: string | null;
  nodes: OmpSessionTreeNode[];
}

const SessionTreeNodeSchema = z.looseObject({
  id: z.string().min(1),
  parentId: z.string().nullable(),
  title: z.string(),
  time: z.object({ created: z.number(), updated: z.number() }),
});

const SessionTreeSnapshotSchema = z.object({
  leafId: z.string().nullable(),
  nodes: z.array(SessionTreeNodeSchema),
});

const parseSessionTreeSnapshot = (value: unknown): OmpSessionTreeSnapshot | null => {
  const parsed = SessionTreeSnapshotSchema.safeParse(value);
  return parsed.success ? parsed.data : null;
};

export interface OmpTreeAPI {
  /**
   * GET /api/omp/sessions/{id}/tree: the fork lineage of one session.
   * `unavailable` = tree.v1 off / old engine (the tree dialog stays closed).
   */
  getSessionTree(sessionID: string, options: { directory: string }): Promise<OmpFetchJsonResult<OmpSessionTreeSnapshot>>;
}

export const createOmpTreeAPI = (apiOptions: OmpJsonApiOptions = {}): OmpTreeAPI => {
  const fetchImpl = apiOptions.fetchImpl ?? runtimeFetch;
  return {
    getSessionTree(sessionID, { directory }) {
      return ompFetchJson(
        fetchImpl,
        OMP_ENDPOINTS.sessionTree(sessionID),
        parseSessionTreeSnapshot,
        { query: { directory } },
      );
    },
  };
};

// ---------------------------------------------------------------------------
// Agent runs API — directory-level subagent snapshot (spec 04 §5.5.1;
// capability agentRuns.v1). Rows are `sessionID::agentId` keyed projections
// of every live session's private AgentRegistry (parked/historical included;
// never any absolute paths).
// ---------------------------------------------------------------------------

export const OMP_AGENT_RUN_STATUSES = ['running', 'idle', 'parked', 'aborted', 'historical'] as const;
export type OmpAgentRunStatus = (typeof OMP_AGENT_RUN_STATUSES)[number];

/** GET /api/omp/agent-runs row (domain-uri.js projectAgentRun). */
export interface OmpAgentRunRecord {
  key: string;
  sessionID: string;
  directory: string;
  agentId: string;
  displayName: string;
  status: OmpAgentRunStatus;
  createdAt: number;
  lastActivity: number;
  activity?: unknown;
}

export interface OmpAgentRunsSnapshot {
  agentRuns: OmpAgentRunRecord[];
  generatedAt: number;
  revision: number;
}

const AgentRunRecordSchema = z.looseObject({
  key: z.string().min(1),
  sessionID: z.string().min(1),
  directory: z.string(),
  agentId: z.string().min(1),
  displayName: z.string(),
  status: z.enum(OMP_AGENT_RUN_STATUSES),
  createdAt: z.number(),
  lastActivity: z.number(),
  activity: z.unknown().optional(),
});

const AgentRunsSnapshotSchema = z.object({
  agentRuns: z.array(AgentRunRecordSchema),
  generatedAt: z.number(),
  revision: z.number(),
});

const parseAgentRunsSnapshot = (value: unknown): OmpAgentRunsSnapshot | null => {
  const parsed = AgentRunsSnapshotSchema.safeParse(value);
  return parsed.success ? parsed.data : null;
};

export interface OmpAgentRunsAPI {
  /**
   * GET /api/omp/agent-runs?directory=… — one row per sessionID::agentId.
   * `unavailable` = agentRuns.v1 off / old engine (legacy child-session list).
   */
  list(options: { directory: string }): Promise<OmpFetchJsonResult<OmpAgentRunsSnapshot>>;
}

export const createOmpAgentRunsAPI = (apiOptions: OmpJsonApiOptions = {}): OmpAgentRunsAPI => {
  const fetchImpl = apiOptions.fetchImpl ?? runtimeFetch;
  return {
    list({ directory }) {
      return ompFetchJson(
        fetchImpl,
        OMP_ENDPOINTS.agentRuns,
        parseAgentRunsSnapshot,
        { query: { directory } },
      );
    },
  };
};

// ---------------------------------------------------------------------------
// Commands API — omp slash-command discovery for the three-layer pipeline
// (spec 08 §5.4; capability commands.v1). Tier 'client-builtin' rows are the
// engine's reserved names (client-side semantics); 'engine' rows expand
// inside a materialized session when sent through the command channel.
// ---------------------------------------------------------------------------

export const OMP_COMMAND_TIERS = ['client-builtin', 'engine'] as const;
export type OmpCommandTier = (typeof OMP_COMMAND_TIERS)[number];

export const OMP_COMMAND_SOURCES = ['builtin', 'skill', 'extension', 'custom', 'mcp_prompt', 'file'] as const;
export type OmpCommandSource = (typeof OMP_COMMAND_SOURCES)[number];

/** GET /api/omp/commands row (domain-commands.js projectOmpCommand). */
export interface OmpCommandRecord {
  name: string;
  description?: string;
  tier: OmpCommandTier;
  source: OmpCommandSource;
  aliases?: string[];
  argumentHint?: string;
}

const CommandRecordSchema = z.looseObject({
  name: z.string().min(1),
  description: z.string().optional(),
  tier: z.enum(OMP_COMMAND_TIERS),
  source: z.enum(OMP_COMMAND_SOURCES),
  aliases: z.array(z.string()).optional(),
  argumentHint: z.string().optional(),
});

export interface OmpCommandsAPI {
  /**
   * GET /api/omp/commands?directory=… — the omp command list for that
   * directory. `unavailable` = commands.v1 off / old engine (callers keep
   * the legacy skills + OC-commands resolution).
   */
  getCommands(options: { directory: string }): Promise<OmpFetchJsonResult<OmpCommandRecord[]>>;
}

export const createOmpCommandsAPI = (apiOptions: OmpJsonApiOptions = {}): OmpCommandsAPI => {
  const fetchImpl = apiOptions.fetchImpl ?? runtimeFetch;
  return {
    getCommands({ directory }) {
      return ompFetchJson(
        fetchImpl,
        OMP_ENDPOINTS.commands,
        parseArrayPayload((value) => {
          const parsed = CommandRecordSchema.safeParse(value);
          return parsed.success ? parsed.data : null;
        }),
        { query: { directory } },
      );
    },
  };
};

// ---------------------------------------------------------------------------
// Plugins API — omp PluginManager + extension files. OpenCode's `/api/config/plugins`
// is intentionally not used here; this is the engine-owned settings surface.
// ---------------------------------------------------------------------------

export interface OmpPluginFeature {
  name: string;
  enabled: boolean;
  description?: string;
}

export interface OmpPluginSetting {
  type?: string;
  description?: string;
  secret?: boolean;
  configured?: boolean;
  value?: unknown;
  default?: unknown;
  values?: string[];
  min?: number;
  max?: number;
  step?: number;
}

export interface OmpPluginRecord {
  id: string;
  kind: 'npm' | 'marketplace';
  scope: 'user' | 'project';
  name: string;
  version: string;
  enabled: boolean;
  editable: boolean;
  description?: string;
  features?: OmpPluginFeature[];
  settings?: Record<string, OmpPluginSetting>;
  extensionEntries: string[];
  permissions: {
    toggle: boolean;
    features: boolean;
    settings: boolean;
    uninstall: boolean;
  };
}

export interface OmpExtensionRecord {
  id: string;
  kind: 'extension';
  scope: 'user' | 'project';
  name: string;
  source: 'native' | 'configured' | 'discovered' | 'plugin-manifest';
  editable: boolean;
  loaded: boolean;
  pluginId?: string;
  pluginName?: string;
  declaredEntry?: string;
}

export interface OmpPluginsSnapshot {
  plugins: OmpPluginRecord[];
  extensions: OmpExtensionRecord[];
}

export interface OmpAppliedSession {
  sessionId: string;
  directory: string;
  appliedAt: number;
  extensionIds: string[];
  pluginNames: string[];
}

const OmpAppliedSessionSchema = z.object({
  sessionId: z.string().min(1),
  directory: z.string().min(1),
  appliedAt: z.number(),
  extensionIds: z.array(z.string()),
  pluginNames: z.array(z.string()),
});

const OmpAppliedSnapshotSchema = z.object({
  sessions: z.array(OmpAppliedSessionSchema),
});

const OmpPluginFeatureSchema = z.object({
  name: z.string().min(1),
  enabled: z.boolean(),
  description: z.string().optional(),
});

const OmpPluginSettingSchema = z.object({
  type: z.string().optional(),
  description: z.string().optional(),
  secret: z.boolean().optional(),
  configured: z.boolean().optional(),
  value: z.unknown().optional(),
  default: z.unknown().optional(),
  values: z.array(z.string()).optional(),
  min: z.number().optional(),
  max: z.number().optional(),
  step: z.number().optional(),
});

const PluginPermissionsSchema = z.object({
  toggle: z.boolean(),
  features: z.boolean(),
  settings: z.boolean(),
  uninstall: z.boolean(),
});

const OmpPluginRecordSchema = z.object({
  id: z.string().min(1),
  kind: z.enum(['npm', 'marketplace']),
  scope: z.enum(['user', 'project']),
  name: z.string().min(1),
  version: z.string().min(1),
  enabled: z.boolean(),
  editable: z.boolean(),
  description: z.string().optional(),
  features: z.array(OmpPluginFeatureSchema).optional(),
  settings: z.record(z.string(), OmpPluginSettingSchema).optional(),
  extensionEntries: z.array(z.string()),
  permissions: PluginPermissionsSchema,
});

const OmpExtensionRecordSchema = z.object({
  id: z.string().min(1),
  kind: z.literal('extension'),
  scope: z.enum(['user', 'project']),
  name: z.string().min(1),
  source: z.enum(['native', 'configured', 'discovered', 'plugin-manifest']),
  editable: z.boolean(),
  loaded: z.boolean(),
  pluginId: z.string().optional(),
  pluginName: z.string().optional(),
  declaredEntry: z.string().optional(),
});

const OmpPluginsSnapshotSchema = z.object({
  plugins: z.array(OmpPluginRecordSchema),
  extensions: z.array(OmpExtensionRecordSchema),
});

const parseOmpPluginsSnapshot = (value: unknown): OmpPluginsSnapshot | null => {
  const parsed = OmpPluginsSnapshotSchema.safeParse(value);
  return parsed.success ? parsed.data : null;
};

export type OmpPluginMutationResult =
  | { ok: true; restartDeferred: boolean; message?: string }
  | { ok: false; unavailable: boolean; error?: string };

export interface OmpPluginsAPI {
  list(options: { directory: string }): Promise<OmpFetchJsonResult<OmpPluginsSnapshot>>;
  listApplied(options: { directory: string }): Promise<OmpFetchJsonResult<OmpAppliedSession[]>>;
  reload(input: { directory: string; sessionId?: string }): Promise<OmpPluginMutationResult & { sessionsRefreshed?: number }>;
  revealPlugin(input: { id: string; directory: string }): Promise<OmpPluginMutationResult>;
  install(input: { spec: string; directory: string; scope?: 'user' | 'project' }): Promise<OmpPluginMutationResult>;
  update(input: {
    id: string;
    directory: string;
    enabled?: boolean;
    enabledFeatures?: string[];
    setting?: { key: string; value?: unknown; remove?: boolean };
  }): Promise<OmpPluginMutationResult>;
  remove(input: { id: string; directory: string }): Promise<OmpPluginMutationResult>;
  setEnabled(input: { id: string; enabled: boolean; directory: string }): Promise<OmpPluginMutationResult>;
  readExtension(input: { id: string; directory: string }): Promise<OmpFetchJsonResult<{
    fileName: string;
    scope: 'user' | 'project';
    content: string;
    editable: boolean;
    source: OmpExtensionRecord['source'];
  }>>;
  createExtension(input: { fileName: string; content: string; scope: 'user' | 'project'; directory: string }): Promise<OmpPluginMutationResult>;
  updateExtension(input: { id: string; content: string; directory: string }): Promise<OmpPluginMutationResult>;
  removeExtension(input: { id: string; directory: string }): Promise<OmpPluginMutationResult>;
  revealExtension(input: { id: string; directory: string }): Promise<OmpPluginMutationResult>;
}

const readOmpPluginMutation = async (response: Response): Promise<OmpPluginMutationResult> => {
  const payload = await response.json().catch(() => null) as { message?: unknown; error?: unknown; restartDeferred?: unknown } | null;
  if (response.status === 404 || response.status === 501) return { ok: false, unavailable: true };
  if (!response.ok) return { ok: false, unavailable: false, error: typeof payload?.error === 'string' ? payload.error : undefined };
  return {
    ok: true,
    restartDeferred: payload?.restartDeferred === true,
    ...(typeof payload?.message === 'string' ? { message: payload.message } : {}),
  };
};


const readOmpReloadResult = async (response: Response): Promise<OmpPluginMutationResult & { sessionsRefreshed?: number }> => {
  const payload = await response.clone().json().catch(() => null) as { sessionsRefreshed?: unknown } | null;
  const base = await readOmpPluginMutation(response);
  return { ...base, ...(typeof payload?.sessionsRefreshed === 'number' ? { sessionsRefreshed: payload.sessionsRefreshed } : {}) };
};

export const createOmpPluginsAPI = (apiOptions: OmpJsonApiOptions = {}): OmpPluginsAPI => {
  const fetchImpl = apiOptions.fetchImpl ?? runtimeFetch;
  const mutation = async (path: string, init: RequestInit & { query?: Record<string, string> }): Promise<OmpPluginMutationResult> =>
    readOmpPluginMutation(await fetchImpl(path, init));
  const update = ({ id, directory, enabled, enabledFeatures, setting }: {
    id: string;
    directory: string;
    enabled?: boolean;
    enabledFeatures?: string[];
    setting?: { key: string; value?: unknown; remove?: boolean };
  }) => mutation(OMP_ENDPOINTS.plugin(id), {
    method: 'PATCH',
    query: { directory },
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({
      directory,
      ...(enabled === undefined ? {} : { enabled }),
      ...(enabledFeatures ? { enabledFeatures } : {}),
      ...(setting ? { setting } : {}),
    }),
  });
  return {
    revealPlugin({ id, directory }) {
      return mutation(OMP_ENDPOINTS.pluginReveal(id), { method: 'POST', query: { directory } });
    },
    revealExtension({ id, directory }) {
      return mutation(OMP_ENDPOINTS.extensionReveal(id), { method: 'POST', query: { directory } });
    },
    list({ directory }) {
      return ompFetchJson(fetchImpl, OMP_ENDPOINTS.plugins, parseOmpPluginsSnapshot, { query: { directory } });
    },
    listApplied({ directory }) {
      return ompFetchJson(fetchImpl, OMP_ENDPOINTS.pluginsApplied, (value) => {
        const parsed = OmpAppliedSnapshotSchema.safeParse(value);
        return parsed.success ? parsed.data.sessions : null;
      }, { query: { directory } });
    },
    reload({ directory, sessionId }) {
      return fetchImpl(OMP_ENDPOINTS.pluginsReload, {
        method: 'POST',
        query: sessionId ? { directory, sessionId } : { directory },
      }).then(readOmpReloadResult);
    },
    install({ spec, directory, scope }) {
      return mutation(OMP_ENDPOINTS.plugins, {
        method: 'POST',
        query: { directory },
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ spec, directory, ...(scope ? { scope } : {}) }),
      });
    },
    update,
    setEnabled(input) {
      return update(input);
    },
    remove({ id, directory }) {
      return mutation(OMP_ENDPOINTS.plugin(id), { method: 'DELETE', query: { directory } });
    },
    readExtension({ id, directory }) {
      return ompFetchJson(fetchImpl, OMP_ENDPOINTS.pluginExtension(id), (value) => {
        const parsed = z.object({
          fileName: z.string().min(1),
          scope: z.enum(['user', 'project']),
          content: z.string(),
          editable: z.boolean(),
          source: z.enum(['native', 'configured', 'discovered', 'plugin-manifest']),
        }).safeParse(value);
        return parsed.success ? parsed.data : null;
      }, { query: { directory } });
    },
    createExtension({ fileName, content, scope, directory }) {
      return mutation(OMP_ENDPOINTS.pluginExtensions, {
        method: 'POST',
        query: { directory },
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ fileName, content, scope, directory }),
      });
    },
    updateExtension({ id, content, directory }) {
      return mutation(OMP_ENDPOINTS.pluginExtension(id), {
        method: 'PUT',
        query: { directory },
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ content, directory }),
      });
    },
    removeExtension({ id, directory }) {
      return mutation(OMP_ENDPOINTS.pluginExtension(id), { method: 'DELETE', query: { directory } });
    },
  };
};
