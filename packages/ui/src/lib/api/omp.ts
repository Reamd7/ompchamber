/**
 * omp parity HTTP/SSE surface — shared runtime-agnostic factories (spec
 * docs/omp-parity/05 §5.2.2; skills ui-api-decoupling + relay-transport).
 *
 * All omp-native endpoints are explicit OpenChamber routes served by the omp
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
  schemaVersion: string;
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
  models: '/api/omp/models',
  dialogs: '/api/omp/dialogs',
  settings: '/api/omp/settings',
  agentRuns: '/api/omp/agent-runs',
  jobs: '/api/omp/jobs',
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

const ModelsSnapshotSchema = z.object({
  schemaVersion: z.string(),
  directory: z.string(),
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

/** Per-role assignment from GET /api/omp/models; `null` = role not configured. */
export type OmpModelRoleEntry = z.infer<typeof RoleEntrySchema>;
export type OmpModelRoleMeta = z.infer<typeof RoleMetaSchema>;
/** GET /api/omp/models?directory=… payload (settings-side role truth). */
export type OmpModelsSnapshot = z.infer<typeof ModelsSnapshotSchema>;

const parseModelsSnapshot = (value: unknown): OmpModelsSnapshot | null => {
  const parsed = ModelsSnapshotSchema.safeParse(value);
  return parsed.success ? parsed.data : null;
};

export interface OmpModelsAPI {
  /** Roles snapshot for a directory's keyed Settings instance. */
  getModels(options: { directory: string }): Promise<OmpFetchJsonResult<OmpModelsSnapshot>>;
}

export const createOmpModelsAPI = (apiOptions: OmpJsonApiOptions = {}): OmpModelsAPI => {
  const fetchImpl = apiOptions.fetchImpl ?? runtimeFetch;
  return {
    getModels(options) {
      return ompFetchJson(fetchImpl, OMP_ENDPOINTS.models, parseModelsSnapshot, {
        query: { directory: options.directory },
      });
    },
  };
};


const ModeSnapshotSchema = z.looseObject({ mode: z.string().min(1) });
const ModeConflictSchema = z.object({ conflict: z.string() });

/** GET/POST /api/omp/sessions/{id}/mode snapshot (02 §5.4; `mode` is the only guaranteed field). */
export type OmpModeSnapshot = z.infer<typeof ModeSnapshotSchema>;

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
