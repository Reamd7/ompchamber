/**
 * omp event reducer — pure state transitions for the chapter-05 stream
 * domain (spec docs/omp-parity/05 §5.2.2; skill sync-state-invariants).
 *
 * Discipline:
 * - Event payloads are network data of unknown shape: each event type parses
 *   once here through its zod schema (skill ui-api-decoupling rule 11) and
 *   the reducer consumes the named output type only.
 * - Same-entity events merge; semantically identical events return no change.
 * - Volatile state carries timestamps; a volatile frame older than the state
 *   it would replace is rejected so late/replayed frames cannot resurrect
 *   expired loaders or warnings.
 * - The high-water mark (`lastAppliedEventId`) advances for every frame that
 *   passes the id gate — including no-ops and unknown types — so a replayed
 *   duplicate is skipped wholesale on the next pass.
 * - Unknown event types are ignored (never an error); syncDebug records them
 *   (spec 05 §5.2.3: minor-version additions must not throw).
 * - The reducer computes state + *effect descriptors* (toast, refetch); the
 *   pipeline executes them. `changed: false` with non-empty `effects` is
 *   valid (volatile-only events such as notices). No I/O in here.
 *
 * Domains owned by other chapters (dialogs/agents/jobs/tree/queue/settings
 * surfaces) are tracked as lastEventId/revision markers only; their surfaces
 * land later and subscribe through the store.
 */

import type { OmpEventEnvelope, OmpPendingDialog } from '@/lib/api/omp';
import { parseOmpPendingDialog } from '@/lib/api/omp';
import { z } from 'zod';
import { syncDebug } from './debug';

// ---------------------------------------------------------------------------
// Payload schemas — the parsing boundary between the SSE envelope and state
// ---------------------------------------------------------------------------

const nonEmptyString = z.string().refine((value) => value.length > 0);

const RetryStartedPayload = z.object({
  attempt: z.number().optional(),
  maxAttempts: z.number().optional(),
  delayMs: z.number().optional(),
  errorMessage: z.string().optional(),
  supersededMessageID: nonEmptyString.optional(),
});

const RetryEndedPayload = z.object({
  success: z.boolean().optional(),
  attempt: z.number().optional(),
  finalError: z.string().optional(),
  retryErrors: z.array(z.object({
    messageID: nonEmptyString,
    note: z.string().optional(),
  }).loose()).optional(),
});

const CompactionStartedPayload = z.object({
  reason: z.string().optional(),
  action: z.string().optional(),
});

const CompactionEndedPayload = z.object({
  action: z.string().optional(),
  aborted: z.boolean().optional(),
  willRetry: z.boolean().optional(),
  errorMessage: z.string().optional(),
  wireMessageID: z.string().optional(),
}).loose();

const CustomAppendedPayload = z.object({
  message: z.object({
    wireMessageID: nonEmptyString,
    customType: nonEmptyString,
    attribution: z.string().optional(),
    timestamp: z.number().optional(),
    text: z.string().optional(),
    details: z.unknown().optional(),
    display: z.boolean().optional(),
  }),
});

const NoticeRaisedPayload = z.object({
  level: z.enum(['info', 'warning', 'error']).optional(),
  message: nonEmptyString,
  source: z.string().optional(),
});

const ModelChangedPayload = z.object({
  model: z.object({
    provider: z.string().optional(),
    id: z.string().optional(),
  }).loose().optional(),
  thinkingLevel: z.string().optional(),
  role: z.string().optional(),
});

const FallbackAppliedPayload = z.object({
  from: z.string().optional(),
  to: z.string().optional(),
  role: z.string().optional(),
});

const FallbackSucceededPayload = z.object({
  model: z.string().optional(),
  role: z.string().optional(),
});

const ModeChangedPayload = z.object({
  mode: nonEmptyString,
  data: z.unknown().optional(),
});

const GoalUpdatedPayload = z.object({
  goal: z.unknown().optional(),
  state: z.string().optional(),
});

const PlanReviewRequestedPayload = z.object({
  details: z.object({
    planFilePath: nonEmptyString,
    title: z.string(),
    planExists: z.boolean(),
  }).nullable(),
});

const ThinkingChangedPayload = z.object({
  thinkingLevel: z.string().optional(),
  configured: z.string().optional(),
  resolved: z.string().optional(),
});

const SessionSettledPayload = z.object({
  isTerminal: z.boolean().optional(),
});

const TtsrTriggeredPayload = z.object({
  rules: z.array(z.object({ name: nonEmptyString }).loose()),
});

const UsageTurnPayload = z.object({
  messageID: nonEmptyString,
  usage: z.record(z.string(), z.unknown()).optional(),
  ttftMs: z.number().optional(),
  durationMs: z.number().optional(),
  timestamp: z.number().optional(),
});

const SettingsUpdatedPayload = z.object({
  revision: z.number(),
  keys: z.array(z.string()).optional(),
  origin: z.string().optional(),
});

const AgentsUpdatedPayload = z.object({
  revision: z.number().optional(),
}).loose();

const QueueChangedPayload = z.object({
  version: z.number(),
});

// ---------------------------------------------------------------------------
// Per-directory omp state (the slice owned by useOmpSessionStore)
// ---------------------------------------------------------------------------

export interface OmpRetryLoader {
  attempt: number;
  maxAttempts?: number;
  startedAt: number;
}

export interface OmpCompactionLoader {
  reason?: string;
  action?: string;
  startedAt: number;
}

export interface OmpSessionLoaders {
  retry?: OmpRetryLoader;
  compaction?: OmpCompactionLoader;
}

export interface OmpRetrySupersession {
  /** Envelope createdAt of the omp.retry.started that superseded the message. */
  since: number;
}

export interface OmpRetryNote {
  note?: string;
  appliedAt: number;
}

export interface OmpCustomDetails {
  customType: string;
  attribution?: string;
  timestamp?: number;
  text?: string;
  details?: unknown;
}

export interface OmpSessionModel {
  provider?: string;
  id?: string;
  thinkingLevel?: string;
  role?: string;
  updatedAt: number;
}

export interface OmpFallbackState {
  active: boolean;
  from?: string;
  to?: string;
  model?: string;
  updatedAt: number;
}

export interface OmpModeState {
  mode: string;
  data?: unknown;
  updatedAt: number;
}

/** Pending plan review (omp.plan.review_requested) — 02 §5.5 step 3. */
export interface OmpPlanReviewState {
  details: {
    planFilePath: string;
    title: string;
    planExists: boolean;
  };
  requestedAt: number;
}

export interface OmpGoalState {
  goal?: unknown;
  state?: string;
  updatedAt: number;
}

export interface OmpThinkingState {
  thinkingLevel?: string;
  configured?: string;
  resolved?: string;
  updatedAt: number;
}

export interface OmpTtsrWarning {
  rules: string[];
  raisedAt: number;
}

export interface OmpTelemetryTurn {
  messageID: string;
  usage?: Record<string, unknown>;
  ttftMs?: number;
  durationMs?: number;
  timestamp?: number;
}

export interface OmpDomainTracking {
  lastEventId: number;
  /** agents snapshot revision (omp.agents.updated {revision}) — jump ⇒ refetch. */
  agentsRevision?: number;
  /** settings revision (omp.settings.updated {revision}) — jump ⇒ refetch. */
  settingsRevision?: number;
  settingsKeys?: string[];
  /** queue versions per session (omp.queue.changed {version}). */
  queueVersionBySession: Record<string, number>;
}

export interface OmpDirectoryState {
  /** Highest omp envelope id consumed for this directory (global monotonic). */
  lastAppliedEventId: number;
  loaders: Record<string, OmpSessionLoaders>;
  superseded: Record<string, OmpRetrySupersession>;
  notes: Record<string, OmpRetryNote>;
  customDetails: Record<string, OmpCustomDetails>;
  sessionModel: Record<string, OmpSessionModel>;
  fallback: Record<string, OmpFallbackState>;
  mode: Record<string, OmpModeState>;
  goal: Record<string, OmpGoalState>;
  /** Pending plan review per session (omp.plan.review_requested, 02 §5.5). */
  planReview: Record<string, OmpPlanReviewState>;
  thinking: Record<string, OmpThinkingState>;
  /** Terminal retry failure per session (omp.retry.ended success=false). */
  retryTerminal: Record<string, { attempt: number; finalError?: string; at: number }>;
  /** awaiting-async secondary status (omp.session.settled {isTerminal:false}). */
  awaitingAsync: Record<string, { since: number }>;
  ttsr: Record<string, OmpTtsrWarning>;
  telemetry: Record<string, OmpTelemetryTurn[]>;
  /** Domain-level lastEventId tracking for surfaces that land later. */
  domains: OmpDomainTracking;
}

export const createEmptyOmpDirectoryState = (): OmpDirectoryState => ({
  lastAppliedEventId: 0,
  loaders: {},
  superseded: {},
  notes: {},
  customDetails: {},
  sessionModel: {},
  fallback: {},
  mode: {},
  goal: {},
  planReview: {},
  thinking: {},
  retryTerminal: {},
  awaitingAsync: {},
  ttsr: {},
  telemetry: {},
  domains: {
    lastEventId: 0,
    queueVersionBySession: {},
  },
});

/** Bounded per-session telemetry retention (oldest turns drop beyond the cap). */
const MAX_TELEMETRY_TURNS_PER_SESSION = 200;

// ---------------------------------------------------------------------------
// Effects — executed by the pipeline, described by the reducer
// ---------------------------------------------------------------------------

export type OmpEventEffect =
  | { kind: 'notice'; level: 'info' | 'warning' | 'error'; message: string; source?: string }
  | { kind: 'settings-revision'; revision: number; keys: string[]; origin?: string }
  | { kind: 'dialog-requested'; dialog: OmpPendingDialog }
  | { kind: 'dialog-settled'; dialogId: string; sessionId: string; outcome: string };

export interface OmpReducerOutcome {
  /** Whether `draft` mutated (store commits only on true). */
  changed: boolean;
  /** Side effects for the pipeline to execute regardless of `changed`. */
  effects: OmpEventEffect[];
}

const NO_CHANGE: OmpReducerOutcome = { changed: false, effects: [] };

/** True when a volatile envelope is older than the state it would replace. */
const isStaleVolatile = (stateTimestamp: number | undefined, envelope: OmpEventEnvelope): boolean =>
  stateTimestamp !== undefined && envelope.createdAt < stateTimestamp;

const requireSessionID = (envelope: OmpEventEnvelope): string | null =>
  typeof envelope.sessionID === 'string' && envelope.sessionID.length > 0 ? envelope.sessionID : null;

/** Frame failed its payload schema — consumed (id gate advanced), no state change. */
const drop = (envelope: OmpEventEnvelope): OmpReducerOutcome => {
  syncDebug.omp.droppedEvent(envelope.type, envelope.id);
  return NO_CHANGE;
};

// ---------------------------------------------------------------------------
// Reducer — mutates `draft` in place (one store transaction per directory),
// mirroring applyDirectoryEvent's contract in event-reducer.ts.
// ---------------------------------------------------------------------------

export function applyOmpEvent(draft: OmpDirectoryState, envelope: OmpEventEnvelope): OmpReducerOutcome {
  // Id gate: a replayed/duplicated frame at or below the high-water mark was
  // already consumed. Ids form one global monotonic sequence, so per-directory
  // arrival order is increasing and this gate also rejects cross-reconnect
  // replay overlap (durable entries re-sent inside the ring window).
  if (envelope.id <= draft.lastAppliedEventId) {
    return NO_CHANGE;
  }
  draft.lastAppliedEventId = envelope.id;

  const sessionID = requireSessionID(envelope);
  const effects: OmpEventEffect[] = [];

  switch (envelope.type) {
    case 'omp.retry.started': {
      if (!sessionID) return drop(envelope);
      const payload = RetryStartedPayload.safeParse(envelope.payload);
      if (!payload.success) return drop(envelope);
      const loaders = draft.loaders[sessionID] ?? {};
      if (isStaleVolatile(loaders.retry?.startedAt, envelope)) return NO_CHANGE;
      draft.loaders[sessionID] = {
        ...loaders,
        retry: {
          attempt: payload.data.attempt ?? 1,
          ...(payload.data.maxAttempts !== undefined ? { maxAttempts: payload.data.maxAttempts } : {}),
          startedAt: envelope.createdAt,
        },
      };
      const supersededMessageID = payload.data.supersededMessageID;
      if (supersededMessageID) {
        const existing = draft.superseded[supersededMessageID];
        // Overlay is pure presentation keyed by message id; keep the earliest
        // marker (the first retry that superseded the message).
        if (!existing || envelope.createdAt < existing.since) {
          draft.superseded[supersededMessageID] = { since: envelope.createdAt };
        }
      }
      return { changed: true, effects };
    }

    case 'omp.retry.ended': {
      if (!sessionID) return drop(envelope);
      const payload = RetryEndedPayload.safeParse(envelope.payload);
      if (!payload.success) return drop(envelope);
      const success = payload.data.success === true;
      const current = draft.loaders[sessionID];
      if (current?.retry && isStaleVolatile(current.retry.startedAt, envelope)) {
        return NO_CHANGE;
      }
      if (current?.retry) {
        draft.loaders[sessionID] = { ...current, retry: undefined };
      }
      let mutated = current?.retry !== undefined;
      for (const entry of payload.data.retryErrors ?? []) {
        const existing = draft.notes[entry.messageID];
        if (existing && existing.appliedAt >= envelope.createdAt && existing.note === entry.note) continue;
        draft.notes[entry.messageID] = {
          ...(entry.note !== undefined ? { note: entry.note } : {}),
          appliedAt: envelope.createdAt,
        };
        mutated = true;
      }
      if (!success) {
        const existing = draft.retryTerminal[sessionID];
        if (!existing || existing.at < envelope.createdAt) {
          draft.retryTerminal[sessionID] = {
            attempt: payload.data.attempt ?? 1,
            ...(payload.data.finalError !== undefined ? { finalError: payload.data.finalError } : {}),
            at: envelope.createdAt,
          };
          mutated = true;
        }
      } else if (draft.retryTerminal[sessionID]) {
        // A later successful retry supersedes the terminal-failure banner.
        delete draft.retryTerminal[sessionID];
        mutated = true;
      }
      return mutated ? { changed: true, effects } : NO_CHANGE;
    }

    case 'omp.compaction.started': {
      if (!sessionID) return drop(envelope);
      const payload = CompactionStartedPayload.safeParse(envelope.payload);
      if (!payload.success) return drop(envelope);
      const loaders = draft.loaders[sessionID] ?? {};
      if (isStaleVolatile(loaders.compaction?.startedAt, envelope)) return NO_CHANGE;
      draft.loaders[sessionID] = {
        ...loaders,
        compaction: {
          ...(payload.data.reason !== undefined ? { reason: payload.data.reason } : {}),
          ...(payload.data.action !== undefined ? { action: payload.data.action } : {}),
          startedAt: envelope.createdAt,
        },
      };
      return { changed: true, effects };
    }

    case 'omp.compaction.ended': {
      if (!sessionID) return drop(envelope);
      // Payload shape parsed for future divider-join data; state transition
      // only needs the loader.
      if (!CompactionEndedPayload.safeParse(envelope.payload).success) return drop(envelope);
      const loaders = draft.loaders[sessionID];
      if (!loaders?.compaction) return NO_CHANGE;
      if (isStaleVolatile(loaders.compaction.startedAt, envelope)) return NO_CHANGE;
      draft.loaders[sessionID] = { ...loaders, compaction: undefined };
      return { changed: true, effects };
    }

    case 'omp.custom.appended': {
      if (!sessionID) return drop(envelope);
      const payload = CustomAppendedPayload.safeParse(envelope.payload);
      if (!payload.success) return drop(envelope);
      const message = payload.data.message;
      // display:false never builds a card on any path (spec 05 §5.8.2 T3).
      if (message.display === false) {
        syncDebug.omp.customHidden(message.wireMessageID, message.customType);
        return NO_CHANGE;
      }
      const next: OmpCustomDetails = {
        customType: message.customType,
        ...(message.attribution !== undefined ? { attribution: message.attribution } : {}),
        ...(message.timestamp !== undefined ? { timestamp: message.timestamp } : {}),
        ...(message.text !== undefined ? { text: message.text } : {}),
        ...(message.details !== undefined ? { details: message.details } : {}),
      };
      const existing = draft.customDetails[message.wireMessageID];
      if (
        existing
        && existing.customType === next.customType
        && existing.attribution === next.attribution
        && existing.timestamp === next.timestamp
        && existing.text === next.text
        && existing.details === next.details
      ) {
        return NO_CHANGE;
      }
      draft.customDetails[message.wireMessageID] = next;
      return { changed: true, effects };
    }

    case 'omp.notice.raised': {
      const payload = NoticeRaisedPayload.safeParse(envelope.payload);
      if (!payload.success) return drop(envelope);
      const source = payload.data.source;
      effects.push({
        kind: 'notice',
        level: payload.data.level ?? 'info',
        message: payload.data.message,
        ...(source !== undefined ? { source } : {}),
      });
      // Volatile by design: the toast is the whole surface; nothing stored.
      return { changed: false, effects };
    }

    case 'omp.model.changed': {
      if (!sessionID) return drop(envelope);
      const payload = ModelChangedPayload.safeParse(envelope.payload);
      if (!payload.success) return drop(envelope);
      const existing = draft.sessionModel[sessionID];
      const model = payload.data.model;
      const provider = model?.provider || existing?.provider;
      const id = model?.id || existing?.id;
      const thinkingLevel = payload.data.thinkingLevel || existing?.thinkingLevel;
      const role = payload.data.role || existing?.role;
      if (
        existing
        && existing.provider === provider
        && existing.id === id
        && existing.thinkingLevel === thinkingLevel
        && existing.role === role
      ) {
        return NO_CHANGE;
      }
      draft.sessionModel[sessionID] = {
        ...(provider !== undefined && provider.length > 0 ? { provider } : {}),
        ...(id !== undefined && id.length > 0 ? { id } : {}),
        ...(thinkingLevel !== undefined && thinkingLevel.length > 0 ? { thinkingLevel } : {}),
        ...(role !== undefined && role.length > 0 ? { role } : {}),
        updatedAt: envelope.createdAt,
      };
      return { changed: true, effects };
    }

    case 'omp.fallback.applied': {
      if (!sessionID) return drop(envelope);
      const payload = FallbackAppliedPayload.safeParse(envelope.payload);
      if (!payload.success) return drop(envelope);
      const existing = draft.fallback[sessionID];
      if (existing?.active && isStaleVolatile(existing.updatedAt, envelope)) return NO_CHANGE;
      draft.fallback[sessionID] = {
        active: true,
        ...(payload.data.from !== undefined ? { from: payload.data.from } : {}),
        ...(payload.data.to !== undefined ? { to: payload.data.to } : {}),
        updatedAt: envelope.createdAt,
      };
      return { changed: true, effects };
    }

    case 'omp.fallback.succeeded': {
      if (!sessionID) return drop(envelope);
      const payload = FallbackSucceededPayload.safeParse(envelope.payload);
      if (!payload.success) return drop(envelope);
      const existing = draft.fallback[sessionID];
      if (existing && !existing.active) return NO_CHANGE;
      draft.fallback[sessionID] = {
        active: false,
        ...(payload.data.model !== undefined ? { model: payload.data.model } : {}),
        updatedAt: envelope.createdAt,
      };
      return { changed: true, effects };
    }

    case 'omp.mode.changed': {
      if (!sessionID) return drop(envelope);
      const payload = ModeChangedPayload.safeParse(envelope.payload);
      if (!payload.success) return drop(envelope);
      const existing = draft.mode[sessionID];
      const sameMode = existing !== undefined
        && existing.mode === payload.data.mode
        && existing.data === payload.data.data;
      if (sameMode && draft.planReview[sessionID] === undefined) {
        return NO_CHANGE;
      }
      // Leaving plan mode settles/clears the review bridge (domain-modes
      // exitMode → bridge.clear) — a pending overlay is no longer actionable.
      if (payload.data.mode !== 'plan' && payload.data.mode !== 'plan_paused') {
        delete draft.planReview[sessionID];
      }
      if (!sameMode) {
        draft.mode[sessionID] = {
          mode: payload.data.mode,
          ...(payload.data.data !== undefined ? { data: payload.data.data } : {}),
          updatedAt: envelope.createdAt,
        };
      }
      return { changed: true, effects };
    }

    case 'omp.plan.review_requested': {
      if (!sessionID) return drop(envelope);
      const payload = PlanReviewRequestedPayload.safeParse(envelope.payload);
      if (!payload.success) return drop(envelope);
      draft.domains.lastEventId = envelope.id;
      // A newer proposal supersedes any pending one (bridge SUPERSEDED_RESULT
      // settles the older propose); details:null carries no review to show.
      if (payload.data.details === null) {
        if (draft.planReview[sessionID] === undefined) return NO_CHANGE;
        delete draft.planReview[sessionID];
        return { changed: true, effects };
      }
      draft.planReview[sessionID] = {
        details: payload.data.details,
        requestedAt: envelope.createdAt,
      };
      return { changed: true, effects };
    }


    case 'omp.goal.updated': {
      if (!sessionID) return drop(envelope);
      const payload = GoalUpdatedPayload.safeParse(envelope.payload);
      if (!payload.success) return drop(envelope);
      const existing = draft.goal[sessionID];
      if (existing && existing.goal === payload.data.goal && existing.state === payload.data.state) {
        return NO_CHANGE;
      }
      draft.goal[sessionID] = {
        ...(payload.data.goal !== undefined ? { goal: payload.data.goal } : {}),
        ...(payload.data.state !== undefined ? { state: payload.data.state } : {}),
        updatedAt: envelope.createdAt,
      };
      return { changed: true, effects };
    }

    case 'omp.thinking.changed': {
      if (!sessionID) return drop(envelope);
      const payload = ThinkingChangedPayload.safeParse(envelope.payload);
      if (!payload.success) return drop(envelope);
      const existing = draft.thinking[sessionID];
      const thinkingLevel = payload.data.thinkingLevel || existing?.thinkingLevel;
      const configured = payload.data.configured || existing?.configured;
      const resolved = payload.data.resolved || existing?.resolved;
      if (
        existing
        && existing.thinkingLevel === thinkingLevel
        && existing.configured === configured
        && existing.resolved === resolved
      ) {
        return NO_CHANGE;
      }
      draft.thinking[sessionID] = {
        ...(thinkingLevel !== undefined ? { thinkingLevel } : {}),
        ...(configured !== undefined ? { configured } : {}),
        ...(resolved !== undefined ? { resolved } : {}),
        updatedAt: envelope.createdAt,
      };
      return { changed: true, effects };
    }

    case 'omp.session.settled': {
      if (!sessionID) return drop(envelope);
      if (!SessionSettledPayload.safeParse(envelope.payload).success) return drop(envelope);
      if ((envelope.payload as { isTerminal?: unknown }).isTerminal !== false) return NO_CHANGE;
      const existing = draft.awaitingAsync[sessionID];
      if (existing && isStaleVolatile(existing.since, envelope)) return NO_CHANGE;
      if (existing) return NO_CHANGE;
      draft.awaitingAsync[sessionID] = { since: envelope.createdAt };
      return { changed: true, effects };
    }

    case 'omp.ttsr.triggered': {
      if (!sessionID) return drop(envelope);
      const payload = TtsrTriggeredPayload.safeParse(envelope.payload);
      if (!payload.success || payload.data.rules.length === 0) return drop(envelope);
      const rules = payload.data.rules.map((rule) => rule.name);
      const existing = draft.ttsr[sessionID];
      if (existing && isStaleVolatile(existing.raisedAt, envelope)) return NO_CHANGE;
      // Consecutive triggers merge into the previous warning block (TUI
      // event-controller semantics: merge into the last uncommitted block).
      draft.ttsr[sessionID] = {
        rules: [...(existing?.rules ?? []), ...rules],
        raisedAt: envelope.createdAt,
      };
      return { changed: true, effects };
    }

    case 'omp.usage.turn': {
      if (!sessionID) return drop(envelope);
      const payload = UsageTurnPayload.safeParse(envelope.payload);
      if (!payload.success) return drop(envelope);
      const turns = draft.telemetry[sessionID] ?? [];
      const { messageID, usage, ttftMs, durationMs, timestamp } = payload.data;
      const index = turns.findIndex((turn) => turn.messageID === messageID);
      const current = index >= 0 ? turns[index] : undefined;
      if (
        current
        && current.usage === usage
        && current.ttftMs === ttftMs
        && current.durationMs === durationMs
        && current.timestamp === timestamp
      ) {
        return NO_CHANGE;
      }
      const nextTurns = [...turns];
      if (index >= 0) {
        nextTurns[index] = { messageID, ...(usage !== undefined ? { usage } : {}), ...(ttftMs !== undefined ? { ttftMs } : {}), ...(durationMs !== undefined ? { durationMs } : {}), ...(timestamp !== undefined ? { timestamp } : {}) };
      } else {
        nextTurns.push({ messageID, ...(usage !== undefined ? { usage } : {}), ...(ttftMs !== undefined ? { ttftMs } : {}), ...(durationMs !== undefined ? { durationMs } : {}), ...(timestamp !== undefined ? { timestamp } : {}) });
        if (nextTurns.length > MAX_TELEMETRY_TURNS_PER_SESSION) {
          nextTurns.splice(0, nextTurns.length - MAX_TELEMETRY_TURNS_PER_SESSION);
        }
      }
      draft.telemetry[sessionID] = nextTurns;
      return { changed: true, effects };
    }

    case 'omp.settings.updated': {
      const payload = SettingsUpdatedPayload.safeParse(envelope.payload);
      if (!payload.success) return drop(envelope);
      const domains = draft.domains;
      if (domains.settingsRevision !== undefined && payload.data.revision <= domains.settingsRevision) {
        // Already-seen revision — renotification only, nothing to refetch.
        return NO_CHANGE;
      }
      domains.settingsRevision = payload.data.revision;
      domains.settingsKeys = payload.data.keys ?? [];
      effects.push({
        kind: 'settings-revision',
        revision: payload.data.revision,
        keys: domains.settingsKeys,
        ...(payload.data.origin !== undefined ? { origin: payload.data.origin } : {}),
      });
      return { changed: true, effects };
    }

    case 'omp.agents.updated': {
      const payload = AgentsUpdatedPayload.safeParse(envelope.payload);
      if (!payload.success) return drop(envelope);
      if (payload.data.revision !== undefined) {
        const known = draft.domains.agentsRevision;
        if (known !== undefined && payload.data.revision < known) return NO_CHANGE;
        draft.domains.agentsRevision = payload.data.revision;
      }
      draft.domains.lastEventId = envelope.id;
      return { changed: true, effects };
    }

    case 'omp.queue.changed': {
      if (!sessionID) return drop(envelope);
      const payload = QueueChangedPayload.safeParse(envelope.payload);
      if (!payload.success) return drop(envelope);
      const known = draft.domains.queueVersionBySession[sessionID];
      if (known !== undefined && payload.data.version <= known) return NO_CHANGE;
      draft.domains.queueVersionBySession[sessionID] = payload.data.version;
      draft.domains.lastEventId = envelope.id;
      return { changed: true, effects };
    }

    case 'omp.dialog.requested': {
      const payloadRaw = envelope.payload as { dialog?: unknown } | null;
      const dialog = parseOmpPendingDialog(payloadRaw?.dialog);
      if (dialog === null) return drop(envelope);
      draft.domains.lastEventId = envelope.id;
      effects.push({ kind: 'dialog-requested', dialog });
      return { changed: true, effects };
    }

    case 'omp.dialog.settled': {
      const payload = z.object({
        dialogId: nonEmptyString,
        sessionId: nonEmptyString,
        outcome: z.string(),
      }).safeParse(envelope.payload);
      if (!payload.success) return drop(envelope);
      draft.domains.lastEventId = envelope.id;
      effects.push({ kind: 'dialog-settled', ...payload.data });
      return { changed: true, effects };
    }

    // Surfaces land later — track the last event id per domain only.
    case 'omp.jobs.updated':
    case 'omp.tree.updated':
    case 'omp.plan.updated': {
      draft.domains.lastEventId = envelope.id;
      return { changed: true, effects };
    }

    default: {
      // Unknown type: minor-version addition (spec 05 §5.2.3) — ignore, never
      // error. syncDebug records it for diagnosis.
      syncDebug.omp.unknownEvent(envelope.type, envelope.id);
      return NO_CHANGE;
    }
  }
}
