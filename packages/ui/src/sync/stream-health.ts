/**
 * Stream liveness telemetry — single-writer diagnostics for the wire event
 * pipeline (`event-pipeline.ts`), read on demand by the engine status report
 * (Ctrl/Cmd+Shift+L, `buildOpenCodeStatusReport`).
 *
 * Why this exists: a window can look connected while its view is stale. The
 * store-level connection flags say whether a transport is attached, not
 * whether events are flowing. These counters separate the two:
 *
 * | lastWireFrameAt | lastDeliveredEventsAt | Reading                        |
 * |-----------------|------------------------|--------------------------------|
 * | fresh           | fresh                  | healthy                        |
 * | fresh           | stale                  | live-but-deaf stream (frames   |
 * |                 |                        | such as heartbeats arrive, no  |
 * |                 |                        | real events are delivered)     |
 * | stale           | stale                  | wedged transport — the client  |
 * |                 |                        | watchdog should recover it; if |
 * |                 |                        | status stays `connected`, the  |
 * |                 |                        | renderer is not executing      |
 *
 * Writers are called by the pipeline at lifecycle edges and at flush time —
 * plain number/enum assignments, no allocation, no subscribers. Nothing here
 * renders; do not subscribe React to it. The wire pipeline is the only writer;
 * the capability-gated omp pipeline (models/settings/dialogs) is intentionally
 * not instrumented — session truth rides the wire stream.
 */

export type StreamHealthStatus = 'idle' | 'connecting' | 'connected' | 'reconnecting'
export type StreamHealthTransport = 'ws' | 'sse' | null
export type StreamResyncReason = 'stream-resync' | 'queue-overflow'

export interface StreamHealthSnapshot {
  /** Lifecycle of the pipeline mount: idle → connecting → connected ⇄ reconnecting. */
  status: StreamHealthStatus
  /** Transport of the last successful connect (`null` before the first one). */
  transport: StreamHealthTransport
  /** Epoch ms of the last wire frame — events, control frames, and heartbeats. */
  lastWireFrameAt: number | null
  /** Epoch ms of the last event batch delivered to directory stores. */
  lastDeliveredEventsAt: number | null
  /** Total events delivered to stores since this page loaded. */
  deliveredEvents: number
  /** Epoch ms of the last successful connect. */
  connectedAt: number | null
  /** Epoch ms of the last disconnect notification from the pipeline. */
  lastDisconnectAt: number | null
  /** Pipeline-reported disconnect reason (`${transport}_${string}` shaped). */
  lastDisconnectReason: string | null
  /** Epoch ms of the last resync control (server disavowed continuity). */
  lastResyncAt: number | null
  lastResyncReason: StreamResyncReason | null
  /** Total resyncs since this page loaded. */
  resyncs: number
}

const initial = (): StreamHealthSnapshot => ({
  status: 'idle',
  transport: null,
  lastWireFrameAt: null,
  lastDeliveredEventsAt: null,
  deliveredEvents: 0,
  connectedAt: null,
  lastDisconnectAt: null,
  lastDisconnectReason: null,
  lastResyncAt: null,
  lastResyncReason: null,
  resyncs: 0,
})

const state: StreamHealthSnapshot = initial()

/**
 * Record a lifecycle transition. Called once per connect/disconnect cycle —
 * never on the streaming path.
 */
export function markStreamLifecycle(
  status: StreamHealthStatus,
  detail?: { transport?: StreamHealthTransport; reason?: string },
): void {
  state.status = status
  if (status === 'connected') {
    if (detail?.transport) state.transport = detail.transport
    state.connectedAt = Date.now()
  } else if (status === 'reconnecting') {
    state.lastDisconnectAt = Date.now()
    state.lastDisconnectReason = detail?.reason ?? null
  }
}

/** Any wire frame arrived (event, control, or heartbeat). Streaming-frequency. */
export function markStreamWireFrame(at: number): void {
  state.lastWireFrameAt = at
}

/** An event batch was flushed to directory stores. Streaming-frequency. */
export function markStreamEventsDelivered(count: number, at: number): void {
  state.deliveredEvents += count
  state.lastDeliveredEventsAt = at
}

/** The server disavowed stream continuity; consumers must reconcile. */
export function markStreamResync(reason: StreamResyncReason): void {
  state.resyncs += 1
  state.lastResyncAt = Date.now()
  state.lastResyncReason = reason
}

/** Copy for report-time reads. Do not call on hot paths. */
export function getStreamHealth(): StreamHealthSnapshot {
  return { ...state }
}

/** Test-only: isolate module state between test cases in one file. */
export function resetStreamHealthForTest(): void {
  Object.assign(state, initial())
}
