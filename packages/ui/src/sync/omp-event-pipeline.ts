/**
 * omp event pipeline — capability-gated subscription to `/api/omp/events`
 * (spec docs/omp-parity/05 §5.2.2/§5.2.3/§5.2.4).
 *
 * Lifecycle:
 *  1. Capability negotiation (`GET /api/omp/capabilities`). A missing
 *     surface (404/501), a payload without `eventSchema`, or
 *     `features.events === false` means an old engine / gated-off channel:
 *     the pipeline stays DORMANT (wire-only degradation, zero errors logged
 *     as failures — spec 05 §5.2.3 matrix). A transport failure also stays
 *     dormant: this mount never treats a flaky probe as a UI failure.
 *  2. On a healthy capability answer the SSE subscription runs through the
 *     runtime's OmpEventsAPI (reconnect discipline owned there, per the
 *     relay-transport skill).
 *  3. Every envelope is dispatched by directory to the consumer; unknown
 *     types are ignored by the reducer (never an error).
 *  4. `omp.stream.resync` control frames run the §5.2.4 reconciliation
 *     matrix through `runOmpResync` — authoritative GETs per scope, or the
 *     full ordered matrix when the scope is untrustable. Events are only
 *     notifications; GETs are the truth (断流不是空状态, master D2).
 *
 * The pipeline performs no domain reductions itself: `onEvent` hands the
 * envelope to the sync-context mount, which routes it into
 * `useOmpSessionStore` (and executes reducer-described effects).
 */

import type { OmpCapabilitiesAPI, OmpEventEnvelope, OmpEventsAPI } from '@/lib/api/omp';
import { runOmpResync, type OmpResyncContext } from './omp-resync';
import { syncDebug } from './debug';

export interface OmpEventPipelineInput {
  ompCapabilities: Pick<OmpCapabilitiesAPI, 'getCapabilities'>;
  ompEvents: Pick<OmpEventsAPI, 'subscribeEvents'>;
  /** Subscribe scope; `null` = all directories (envelopes route themselves). */
  directory: string | null;
  onEvent: (envelope: OmpEventEnvelope) => void;
  /** Context the resync matrix needs (wire resync hook, session listing). */
  resync: OmpResyncContext;
  /** Test seam; production callers omit it. */
  now?: () => number;
}

export interface OmpEventPipeline {
  /** True once the capability gate passed and the subscription is running. */
  readonly started: () => boolean;
  cleanup: () => void;
}

const RESYNC_MIN_INTERVAL_MS = 2_000;

export function createOmpEventPipeline(input: OmpEventPipelineInput): OmpEventPipeline {
  const now = input.now ?? Date.now;
  const lifecycle = new AbortController();
  let subscription: { close: () => void } | null = null;
  let running = false;
  let lastResyncAt = 0;
  let resyncInFlight: Promise<void> | null = null;

  void (async () => {
    if (lifecycle.signal.aborted) return;
    let capabilities;
    try {
      capabilities = await input.ompCapabilities.getCapabilities();
    } catch {
      // Probe failure (offline, relay old bundle): degrade wire-only for this
      // mount. The wire pipeline's reconnect/recovery loops remain the
      // transport health authority; a later remount (runtime switch) retries.
      syncDebug.omp.dormant('capabilities-unreachable');
      return;
    }
    if (lifecycle.signal.aborted) return;
    if (capabilities === null) {
      syncDebug.omp.dormant('capabilities-missing');
      return;
    }
    if (capabilities.features.events !== true) {
      syncDebug.omp.dormant('events-feature-off');
      return;
    }
    syncDebug.omp.started(capabilities.eventSchema);
    let pendingFullResync = false;
    const runResync = (scope: string[], lastEventId: number | null): void => {
      syncDebug.omp.resync(scope, lastEventId);
      const full = scope.length === 0;
      // Coalesce overlapping resync demands into the in-flight run; a run
      // already covers every scope it listed, and the full matrix covers all.
      if (resyncInFlight !== null) {
        if (full) pendingFullResync = true;
        return;
      }
      // Rate-limit duplicate frames (server may emit one per reconnect).
      const at = now();
      if (!full && at - lastResyncAt < RESYNC_MIN_INTERVAL_MS) return;
      lastResyncAt = at;
      resyncInFlight = runOmpResync(full ? null : scope, input.resync, lifecycle.signal)
        .catch(() => {
          // Authoritative GET failure leaves prior state intact (D2); the
          // next resync frame or directory bootstrap retries.
        })
        .finally(() => {
          resyncInFlight = null;
          if (pendingFullResync) {
            pendingFullResync = false;
            runResync([], null);
          }
        });
    };

    running = true;
    subscription = input.ompEvents.subscribeEvents(input.directory, {
      onEvent: (envelope) => {
        if (lifecycle.signal.aborted) return;
        input.onEvent(envelope);
      },
      onResync: ({ scope, lastEventId }) => {
        if (lifecycle.signal.aborted) return;
        runResync(scope, lastEventId);
      },
    });
  })();

  return {
    started: () => running,
    cleanup: () => {
      lifecycle.abort();
      subscription?.close();
      subscription = null;
      running = false;
    },
  };
}
