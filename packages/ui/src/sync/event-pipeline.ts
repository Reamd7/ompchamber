/**
 * Event Pipeline — transport connection, event coalescing, and batched flush.
 *
 * This module must not make state-dependent decisions about event validity.
 * For example, deciding whether a delta is already represented by a full part
 * snapshot belongs in the reducer, which has access to the current state.
 *
 * Plain closure API:
 *   const { cleanup } = createEventPipeline({ sdk, onEvents })
 *
 * No class, no start/stop lifecycle. One pipeline per mount.
 * Abort controller created once at init, cleaned up via returned cleanup fn.
 */

import type { Event, OpencodeClient, SessionStatus } from '@/lib/opencode/wire'
import { opencodeClient } from "@/lib/opencode/client"
import { getRuntimeUrlResolver } from "@/lib/runtime-url"
import { clearRuntimeUrlAuthToken, refreshRuntimeUrlAuthToken } from "@/lib/runtime-auth"
import { type RelayTunnelWebSocket } from "@/lib/relay/tunnel-client"
import { openRuntimeWebSocket } from "@/lib/relay/runtime-socket"
import { syncDebug } from "./debug"
import { countSyncPerformance } from "./performance-diagnostics"

const FLUSH_FRAME_MS = 33
const BACKPRESSURE_FLUSH_FRAME_MS = 200
const BACKPRESSURE_MODE_MS = 10_000
const STREAM_YIELD_MS = 8
const DEFAULT_RECONNECT_DELAY_MS = 250
const DEFAULT_HEARTBEAT_TIMEOUT_MS = 30_000
const WS_FALLBACK_WINDOW_MS = 60_000
const DEFAULT_WS_READY_TIMEOUT_MS = 2_000
// Retry pacing. Visible+online tabs probe quickly so the user sees connection
// recovery in under a second of real outage; hidden/offline tabs back off
// further so a backgrounded PWA on a flaky link doesn't burn battery probing
// a dead network every few seconds. The browser would throttle hidden-tab
// timers anyway, but this keeps the intent explicit and shrinks server load
// from idle tabs.
const RETRY_BACKOFF_BASE_MS = 250
const RETRY_BACKOFF_CAP_VISIBLE_MS = 5_000
const RETRY_BACKOFF_CAP_HIDDEN_OR_OFFLINE_MS = 60_000
const RETRY_BACKOFF_MAX_EXPONENT = 8
// Local retention caps (docs/plan.md §5.3.1): the per-directory queues are
// the browser-side tail of the transport — without bounds, a stalled
// consumer or hostile directory fan-out turns them into an unbounded
// retention chain. Overflow drops the queued window for that directory and
// asks the consumer to reconcile instead of silently growing.
const MAX_TRACKED_DIRECTORIES = 64
const MAX_DIRECTORY_KEY_LENGTH = 1024
const MAX_QUEUED_EVENTS_PER_DIRECTORY = 4096
const MAX_QUEUED_BYTES_PER_DIRECTORY = 8 * 1024 * 1024
const ESTIMATE_EVENT_MAX_NODES = 512

/** JSON-shaped payload the size estimator walks (recursive wire data). */
type EstimableJson = string | number | boolean | null | EstimableJson[] | { [key: string]: EstimableJson }

/**
 * Bounded shallow serialized-size estimate in UTF-16 units, never a heap
 * claim. Counts the WHOLE event — object keys and full string length — so
 * a single arbitrarily large event cannot pass under the queue's byte cap
 * on a clamped estimate. A structure beyond the node bound reports
 * MAX_SAFE_INTEGER: it overflows the queue cap and takes the drop+resync
 * path instead of being undercounted. Union arms branch by structure
 * (null / Array / Object / scalar), never `typeof`.
 */
const estimateEventBytes = (payload: Event): number => {
  let total = 0
  let nodes = 0
  const stack: unknown[] = [payload]
  while (stack.length > 0) {
    nodes += 1
    if (nodes > ESTIMATE_EVENT_MAX_NODES) return Number.MAX_SAFE_INTEGER
    const current = stack.pop()
    if (current === null || current === undefined) {
      total += 4
    } else if (Array.isArray(current)) {
      total += 8
      for (const item of current) stack.push(item)
    } else if (current instanceof Object) {
      total += 8
      // SAFETY: wire events carry JSON data — an Object arm here is a plain
      // JSON record whose values are again estimable.
      for (const [key, item] of Object.entries(current as Record<string, EstimableJson>)) {
        total += key.length
        stack.push(item)
      }
    } else {
      // Scalar arm (string/number/boolean, plus any smuggled primitive):
      // the string form's length approximates the serialized size.
      total += 4 + String(current).length
    }
  }
  return total
}

type EventPipelineDelivery = {
  onEvent: (directory: string, payload: Event) => void
  onEvents?: never
} | {
  onEvent?: never
  onEvents: (directory: string, payloads: readonly Event[]) => void
}

export type EventPipelineInput = {
  sdk: OpencodeClient
  routeDirectory?: (directory: string, payload: Event) => string
  /** Called after stream reconnects (visibility restore or heartbeat timeout). */
  onReconnect?: () => void
  /** Called when the stream disconnects (heartbeat timeout, network error, or transport failure). */
  onDisconnect?: (reason: string) => void
  /**
   * Called when the server disavows stream continuity (resync control,
   * restart, or local queue overflow): consumers must reconcile
   * authoritative state for every directory before trusting deltas again
   * (docs/plan.md §5.2). The pipeline keeps its transport alive.
   */
  onResync?: (reason: "stream-resync" | "queue-overflow") => void
  /** Called when transport switches (e.g. WS timeout → SSE fallback) without actual disconnection. */
  onTransportSwitch?: () => void
  transport?: "auto" | "ws" | "sse"
  heartbeatTimeoutMs?: number
  reconnectDelayMs?: number
  wsReadyTimeoutMs?: number
} & EventPipelineDelivery

export type EventPipelineStats = {
  /** Events dropped by the directory/queue caps (plan §5.3.1). */
  droppedEvents: number
  /** Overflow-driven reconciles triggered. */
  overflowResyncs: number
  trackedDirectories: number
  queuedEvents: number
}

export type EventPipeline = {
  cleanup: () => void
  reconnect: (reason?: string) => void
  stats: () => EventPipelineStats
}

type MessageStreamWsFrame = {
  type: "ready" | "event" | "error" | "backpressure" | "resync"
  payload?: unknown
  eventId?: string
  directory?: string
  message?: string
  scope?: "global" | "directory"
  /** Transport control metadata carried by resync frames. */
  epoch?: string
}

const normalizeOMPChamberSessionStatus = (payload: Event): Event | null => {
  const record = payload as unknown as {
    id?: unknown
    type?: unknown
    properties?: {
      sessionID?: unknown
      sessionId?: unknown
      status?: unknown
      metadata?: {
        attempt?: unknown
        message?: unknown
        next?: unknown
      }
    }
  }

  if (record.type !== "ompchamber:session-status") return null

  const sessionID = typeof record.properties?.sessionID === "string" && record.properties.sessionID.length > 0
    ? record.properties.sessionID
    : typeof record.properties?.sessionId === "string" && record.properties.sessionId.length > 0
      ? record.properties.sessionId
      : ""
  const rawStatus = typeof record.properties?.status === "string" ? record.properties.status : ""
  if (!sessionID || !rawStatus) return null

  let status: SessionStatus | null = null
  if (rawStatus === "idle" || rawStatus === "busy") {
    status = { type: rawStatus }
  } else if (rawStatus === "retry") {
    const metadata = record.properties?.metadata
    if (
      typeof metadata?.attempt === "number"
      && typeof metadata.message === "string"
      && typeof metadata.next === "number"
    ) {
      status = {
        type: "retry",
        attempt: metadata.attempt,
        message: metadata.message,
        next: metadata.next,
      }
    }
  }
  if (!status) return null

  return {
    id: typeof record.id === "string" && record.id.length > 0
      ? record.id
      : `ompchamber-status-${sessionID}-${Date.now()}`,
    type: "session.status",
    properties: {
      sessionID,
      status,
    },
  } as Event
}

const normalizeEventType = (payload: Event): Event => {
  const normalizedOMPChamberStatus = normalizeOMPChamberSessionStatus(payload)
  if (normalizedOMPChamberStatus) {
    return normalizedOMPChamberStatus
  }

  const type = (payload as { type?: unknown }).type
  if (typeof type !== "string") {
    return payload
  }

  const match = /^(.*)\.(\d+)$/.exec(type)
  if (!match || !match[1]) {
    return payload
  }

  return {
    ...payload,
    type: match[1] as Event["type"],
  } as unknown as Event
}

function resolveEventDirectory(event: unknown, payload: Event): string {
  const directDirectory =
    typeof event === "object" && event !== null && typeof (event as { directory?: unknown }).directory === "string"
      ? (event as { directory: string }).directory
      : null

  if (directDirectory && directDirectory.length > 0) {
    return directDirectory
  }

  const properties =
    typeof payload.properties === "object" && payload.properties !== null
      ? (payload.properties as Record<string, unknown>)
      : null
  const propertyDirectory = typeof properties?.directory === "string" ? properties.directory : null
  if (propertyDirectory && propertyDirectory.length > 0) {
    return propertyDirectory
  }

  // session.created / session.updated carry directory inside properties.info
  const info =
    typeof properties?.info === "object" && properties.info !== null
      ? (properties.info as Record<string, unknown>)
      : null
  const infoDirectory = typeof info?.directory === "string" ? info.directory : null
  if (infoDirectory && infoDirectory.length > 0) {
    return infoDirectory
  }

  return "global"
}

function resolveEventPayload(payload: unknown): Event | null {
  if (!payload || typeof payload !== "object") {
    return null
  }

  const record = payload as { type?: unknown; payload?: unknown }
  if (typeof record.type === "string") {
    return payload as Event
  }

  if (record.payload && typeof record.payload === "object" && typeof (record.payload as { type?: unknown }).type === "string") {
    return record.payload as Event
  }

  return null
}

function buildGlobalEventWsUrl(lastEventId?: string, serverEpoch?: string): string {
  let baseUrl = "/api"
  try {
    const client = opencodeClient as { getBaseUrl?: () => string }
    if (typeof client.getBaseUrl === "function") {
      baseUrl = client.getBaseUrl()
    }
  } catch {
    baseUrl = "/api"
  }
  const normalizedBase = baseUrl.endsWith("/") ? baseUrl : `${baseUrl}/`
  // The resume pair only makes sense together: a numeric event id without
  // its boot epoch is unprovable across an upstream restart (§5.4).
  let resume: { lastEventId: string; epoch?: string } | undefined
  if (lastEventId && lastEventId.length > 0) {
    resume = { lastEventId }
    if (serverEpoch && serverEpoch.length > 0) {
      resume.epoch = serverEpoch
    }
  }
  return getRuntimeUrlResolver().websocket(
    `${normalizedBase}global/event/ws`,
    resume,
  )
}

// In relay mode the global-event WebSocket rides the E2EE tunnel instead of a
// native network socket. The resolver still builds the authenticated URL (it
// carries the oc_url_token the host replays to the loopback origin); we hand
// its path+query to the tunnel, which returns a socket-like with the exact
// on* handler surface this pipeline uses. Direct-URL runtimes keep the native
// WebSocket path, wrapped to the same shape so the caller holds one type.
function openGlobalEventSocket(lastEventId?: string, serverEpoch?: string): RelayTunnelWebSocket {
  const url = buildGlobalEventWsUrl(lastEventId, serverEpoch)
  return openRuntimeWebSocket(url)
}

type DirectoryQueue = {
  queue: Event[]
  /** Serialized-size estimate per queued event, index-aligned with `queue`. */
  sizes: number[]
  /** Estimated retained bytes of `queue` (declared UTF-16 estimate). */
  bytes: number
  buffer: Event[]
  coalesced: Map<string, number>
  timer: ReturnType<typeof setTimeout> | undefined
  last: number
}

type AttemptAbortReason =
  | "pipeline_stopped"
  | `${"ws" | "sse"}_${string}`
  | null

export function createEventPipeline(input: EventPipelineInput): EventPipeline {
  const {
    sdk,
    onEvent,
    onEvents,
    onReconnect,
    onDisconnect,
    onResync,
    onTransportSwitch,
    routeDirectory,
    transport = "auto",
    heartbeatTimeoutMs = DEFAULT_HEARTBEAT_TIMEOUT_MS,
    reconnectDelayMs = DEFAULT_RECONNECT_DELAY_MS,
    wsReadyTimeoutMs = DEFAULT_WS_READY_TIMEOUT_MS,
  } = input
  const abort = new AbortController()
  let disconnected = false
  let lastEventId: string | undefined
  /** Server boot identity learned from omp.stream.boot (echoed on resume). */
  let serverEpoch: string | undefined
  let wsFallbackUntil = 0
  const overflow = { droppedEvents: 0, overflowResyncs: 0, dirCapNotified: false }

  const directories = new Map<string, DirectoryQueue>()

  const getOrCreateDir = (directory: string): DirectoryQueue => {
    let d = directories.get(directory)
    if (d) return d
    d = {
      queue: [],
      sizes: [],
      bytes: 0,
      buffer: [],
      coalesced: new Map(),
      timer: undefined,
      last: 0,
    }
    directories.set(directory, d)
    return d
  }

  const key = (payload: Event): string | undefined => {
    if (payload.type === "session.status") {
      const props = payload.properties as { sessionID: string }
      return `session.status:${props.sessionID}`
    }
    if (payload.type === "session.updated") {
      const props = payload.properties as { info?: { id?: string } }
      return props.info?.id ? `session.updated:${props.info.id}` : undefined
    }
    if (payload.type === "lsp.updated") {
      return "lsp.updated"
    }
    if (payload.type === "message.part.delta") {
      const props = payload.properties as { messageID: string; partID: string; field: string }
      return `message.part.delta:${props.messageID}:${props.partID}:${props.field}`
    }
    return undefined
  }

  const flushDir = (directory: string) => {
    const d = directories.get(directory)
    if (!d) return
    if (d.timer) {
      clearTimeout(d.timer)
      d.timer = undefined
    }
    if (d.queue.length === 0) return

    const events = d.queue
    const sizes = d.sizes
    d.queue = d.buffer
    d.buffer = events
    d.queue.length = 0
    d.sizes = []
    sizes.length = 0
    d.bytes = 0
    d.coalesced.clear()

    d.last = Date.now()
    syncDebug.pipeline.flush(events.length)
    for (let index = 0; index < events.length; index += 1) {
      countSyncPerformance("pipelineDeliveredEvents")
    }
    if (onEvents) {
      onEvents(directory, events)
    } else if (onEvent) {
      for (const payload of events) onEvent(directory, payload)
    }

    d.buffer.length = 0
    if (d.queue.length === 0 && !d.timer) {
      // Quiet directories release their tracking record so the
      // MAX_TRACKED_DIRECTORIES cap bounds PENDING fan-out, not the set of
      // directories ever seen this session. (Events re-enqueued during
      // delivery keep the record alive.)
      directories.delete(directory)
      overflow.dirCapNotified = false
    }
  }

  const flushAll = () => {
    for (const directory of directories.keys()) {
      flushDir(directory)
    }
  }

  const scheduleDir = (directory: string) => {
    const d = getOrCreateDir(directory)
    if (d.timer) return
    const elapsed = Date.now() - d.last
    const flushFrameMs = Date.now() < backpressureUntil ? BACKPRESSURE_FLUSH_FRAME_MS : FLUSH_FRAME_MS
    d.timer = setTimeout(() => flushDir(directory), Math.max(0, flushFrameMs - elapsed))
  }

  const wait = (ms: number) => new Promise<void>((resolve) => setTimeout(resolve, ms))
  const isAbortError = (error: unknown): boolean =>
    error instanceof DOMException && error.name === "AbortError" ||
    (typeof error === "object" && error !== null && (error as { name?: string }).name === "AbortError")

  const isOffline = (): boolean =>
    typeof navigator === "object" && navigator !== null && navigator.onLine === false

  const isHidden = (): boolean =>
    typeof document !== "undefined" && document.visibilityState !== "visible"

  // Extract an HTTP status code from anywhere it might be hiding on the
  // error object. The SDK's unwrap pattern stashes it on `.status`; raw
  // fetch failures may carry `.response.status`; some SDKs also use `.code`.
  const extractStatus = (error: unknown): number | undefined => {
    if (!error || typeof error !== "object") return undefined
    const direct = (error as { status?: unknown }).status
    if (typeof direct === "number") return direct
    const fromResponse = (error as { response?: { status?: unknown } }).response?.status
    if (typeof fromResponse === "number") return fromResponse
    return undefined
  }

  // 4xx errors don't recover from blind retry — wrong path, expired auth,
  // bad request body. Keep retrying anyway (a remote reconfigure or reauth
  // can fix the underlying problem) but at the long cap so we're not
  // hammering the server at 5s intervals indefinitely. 408 (timeout) and
  // 429 (rate limit) are retryable in spirit — let them through to the
  // normal exponential path.
  const isPermanentHttpStatus = (status: number): boolean => {
    if (status < 400 || status >= 500) return false
    if (status === 408 || status === 429) return false
    return true
  }

  /**
   * Wait between reconnect attempts. Resolves early when:
   *   - the browser fires `online` (network came back — probe immediately),
   *   - the tab becomes visible (user came back — probe immediately),
   *   - the pipeline is being torn down (cleanup aborts).
   * Otherwise resolves after `ms` like a plain timer.
   */
  const waitForRetry = (ms: number) => new Promise<void>((resolve) => {
    if (ms <= 0 || abort.signal.aborted) {
      resolve()
      return
    }

    const cleanup = () => {
      if (timer !== undefined) {
        clearTimeout(timer)
        timer = undefined
      }
      if (typeof globalThis.window !== "undefined") {
        globalThis.window.removeEventListener("online", onInterrupt)
      }
      if (typeof document !== "undefined") {
        document.removeEventListener("visibilitychange", onVisibilityInterrupt)
      }
      abort.signal.removeEventListener("abort", onInterrupt)
    }
    const onInterrupt = () => {
      cleanup()
      resolve()
    }
    const onVisibilityInterrupt = () => {
      if (typeof document !== "undefined" && document.visibilityState === "visible") {
        onInterrupt()
      }
    }

    let timer: ReturnType<typeof setTimeout> | undefined = setTimeout(onInterrupt, ms)
    if (typeof globalThis.window !== "undefined") {
      globalThis.window.addEventListener("online", onInterrupt, { once: true })
    }
    if (typeof document !== "undefined") {
      document.addEventListener("visibilitychange", onVisibilityInterrupt)
    }
    abort.signal.addEventListener("abort", onInterrupt, { once: true })
  })

  const computeRetryDelay = (failures: number): number => {
    if (failures <= 0) return 0
    // Offline: don't spin probing a dead network. Use the long cap and rely on
    // waitForRetry to resolve early when the `online` event fires. The cap is
    // also a fallback for browsers that miss `online`.
    if (isOffline()) return RETRY_BACKOFF_CAP_HIDDEN_OR_OFFLINE_MS
    const cap = isHidden() ? RETRY_BACKOFF_CAP_HIDDEN_OR_OFFLINE_MS : RETRY_BACKOFF_CAP_VISIBLE_MS
    const exponent = Math.min(failures - 1, RETRY_BACKOFF_MAX_EXPONENT)
    return Math.min(cap, RETRY_BACKOFF_BASE_MS * 2 ** exponent)
  }

  let streamErrorLogged = false
  let attempt: AbortController | undefined
  let lastEventAt = Date.now()
  let heartbeat: ReturnType<typeof setTimeout> | undefined
  let activeTransport: "ws" | "sse" = transport === "ws" ? "ws" : "sse"
  let attemptAbortReason: AttemptAbortReason = null
  let consecutiveFailures = 0
  let backpressureUntil = 0

  const notifyDisconnected = (reason: string) => {
    if (disconnected) {
      return
    }
    disconnected = true
    onDisconnect?.(reason)
  }

  const markConnected = () => {
    disconnected = false
    consecutiveFailures = 0
    // Fire onReconnect on every successful connect — including the very
    // first one. Consumer state (isConnected) starts at false and needs
    // to be flipped positively; without this the send button throws
    // "Connection lost" until something else (HTTP health check) happens
    // to race a setState({isConnected: true}) through.
    onReconnect?.()
  }

  const enqueueEvent = (directory: string, payload: Event) => {
    countSyncPerformance("pipelineRawEvents")
    const normalizedPayload = normalizeEventType(payload)
    const routedDirectory = routeDirectory?.(directory, normalizedPayload) || directory

    // Directory fan-out caps (plan §5.3.1): an unbounded directories map is
    // an unbounded retention chain in this process.
    if (routedDirectory.length > MAX_DIRECTORY_KEY_LENGTH) {
      overflow.droppedEvents += 1
      return
    }
    if (!directories.has(routedDirectory) && directories.size >= MAX_TRACKED_DIRECTORIES) {
      overflow.droppedEvents += 1
      if (!overflow.dirCapNotified) {
        // 64 directories with pending work means real fan-out pressure —
        // reconcile once per overflow episode instead of losing an unseen
        // directory's events silently (断流不是空状态).
        overflow.dirCapNotified = true
        onResync?.("queue-overflow")
      }
      return
    }
    const d = getOrCreateDir(routedDirectory)

    const eventBytes = estimateEventBytes(normalizedPayload)
    if (
      d.queue.length >= MAX_QUEUED_EVENTS_PER_DIRECTORY
      || d.bytes + eventBytes > MAX_QUEUED_BYTES_PER_DIRECTORY
    ) {
      // The consumer is not draining this directory: drop the queued window
      // and reconcile instead of retaining an unbounded backlog. The
      // incoming event re-seeds the fresh queue (plan §5.3.1) — unless it
      // alone exceeds the budget: reseeding it would re-trip the overflow
      // on the very next event, so it is dropped with the window.
      overflow.droppedEvents += d.queue.length
      overflow.overflowResyncs += 1
      d.queue.length = 0
      d.sizes.length = 0
      d.bytes = 0
      d.coalesced.clear()
      onResync?.("queue-overflow")
      if (eventBytes > MAX_QUEUED_BYTES_PER_DIRECTORY) {
        overflow.droppedEvents += 1
        return
      }
    }

    // A full part snapshot is a coalescing barrier for that part's deltas:
    // drop its pending delta coalescing keys so a delta arriving after the
    // snapshot starts a fresh queue entry instead of merging into a delta
    // queued before the snapshot, which the snapshot would then overwrite and
    // drop the later delta's text. The already-queued delta event stays.
    if (normalizedPayload.type === "message.part.updated") {
      const part = (normalizedPayload.properties as { part?: { id?: unknown; messageID?: unknown } }).part
      const messageID = typeof part?.messageID === "string" ? part.messageID : undefined
      const partID = typeof part?.id === "string" ? part.id : undefined
      if (messageID && partID) {
        const deltaPrefix = `message.part.delta:${messageID}:${partID}:`
        for (const coalesceKey of d.coalesced.keys()) {
          if (coalesceKey.startsWith(deltaPrefix)) {
            d.coalesced.delete(coalesceKey)
          }
        }
      }
    }

    if (
      normalizedPayload.type === "session.idle"
      || normalizedPayload.type === "session.created"
      || normalizedPayload.type === "session.deleted"
    ) {
      const properties = normalizedPayload.properties as {
        sessionID?: unknown
        info?: { id?: unknown }
      }
      const sessionID = typeof properties.sessionID === "string"
        ? properties.sessionID
        : typeof properties.info?.id === "string"
          ? properties.info.id
          : undefined
      if (sessionID) {
        d.coalesced.delete(`session.status:${sessionID}`)
        if (normalizedPayload.type === "session.created" || normalizedPayload.type === "session.deleted") {
          d.coalesced.delete(`session.updated:${sessionID}`)
        }
      }
    }

    const k = key(normalizedPayload)
    if (k) {
      const i = d.coalesced.get(k)
      if (i !== undefined) {
        if (normalizedPayload.type === "message.part.delta") {
          const prev = d.queue[i] as unknown as { properties: { delta: string } }
          const inc = normalizedPayload.properties as { delta: string }
          const merged = {
            ...normalizedPayload,
            properties: {
              ...(normalizedPayload.properties as object),
              delta: prev.properties.delta + inc.delta,
            },
          } as unknown as Event
          d.queue[i] = merged
          const mergedBytes = estimateEventBytes(merged)
          d.bytes += mergedBytes - (d.sizes[i] ?? 0)
          d.sizes[i] = mergedBytes
        } else {
          d.queue[i] = normalizedPayload
          d.bytes += eventBytes - (d.sizes[i] ?? 0)
          d.sizes[i] = eventBytes
        }
        countSyncPerformance("pipelineCoalescedEvents")
        syncDebug.pipeline.coalesced(normalizedPayload.type, k)
        return
      }
      d.coalesced.set(k, d.queue.length)
    }

    d.queue.push(normalizedPayload)
    d.sizes.push(eventBytes)
    d.bytes += eventBytes
    scheduleDir(routedDirectory)
  }

  const resetHeartbeat = () => {
    lastEventAt = Date.now()
    if (heartbeat) clearTimeout(heartbeat)
    heartbeat = setTimeout(() => {
      attemptAbortReason = `${activeTransport}_heartbeat_timeout`
      attempt?.abort()
    }, heartbeatTimeoutMs)
  }

  const clearHeartbeat = () => {
    if (!heartbeat) return
    clearTimeout(heartbeat)
    heartbeat = undefined
  }

  const runSseAttempt = async (signal: AbortSignal) => {
    const request: Parameters<typeof sdk.global.event>[0] = {
      signal,
      onSseEvent: (event: { id?: unknown; event?: unknown; data?: unknown }) => {
        resetHeartbeat()
        const eventName = typeof event.event === "string" ? event.event : ""
        if (eventName === "omp.stream.resync") {
          // In-band wire control (no data): adopt the reconnectable tail —
          // 0 clears the cursor — and reconcile. Never carry the old cursor
          // into the next reconnect (plan §5.2).
          const id = typeof event.id === "string" ? event.id : ""
          lastEventId = id.length > 0 && id !== "0" ? id : undefined
          onResync?.("stream-resync")
          return
        }
        if (eventName === "omp.stream.boot") {
          // SAFETY: the boot frame's data is a JSON object by contract; the
          // prototype tag proves epoch's string arm at this parse boundary.
          const record = event.data instanceof Object ? (event.data as { epoch?: unknown }) : null
          const epoch = record?.epoch
          if (epoch !== undefined && epoch !== null && Object.prototype.toString.call(epoch) === "[object String]") {
            const learned = String(epoch)
            if (learned.length > 0) serverEpoch = learned
          }
          return
        }
        if (typeof event.id === "string" && event.id.length > 0) {
          lastEventId = event.id
        }
      },
      onSseError: (error: unknown) => {
        if (isAbortError(error)) return
        if (streamErrorLogged) return
        streamErrorLogged = true
        console.error("[event-pipeline] SSE stream error", error)
      },
    }
    // Resume headers: Last-Event-ID carries the cursor and x-omp-epoch the
    // boot identity that makes it provable (docs/plan.md §5.2.1). Both ride
    // together — a cursor without its epoch is unprovable.
    if (lastEventId && lastEventId.length > 0) {
      request.headers = { "Last-Event-ID": lastEventId }
      if (serverEpoch && serverEpoch.length > 0) {
        request.headers["x-omp-epoch"] = serverEpoch
      }
    }
    const events = await sdk.global.event(request)

    markConnected()

    let yielded = Date.now()
    resetHeartbeat()

    for await (const event of events.stream) {
      resetHeartbeat()
      streamErrorLogged = false

      const payload = resolveEventPayload((event as { payload?: Event }).payload ?? event)
      if (!payload) {
        continue
      }
      const directory = resolveEventDirectory(event, payload)
      enqueueEvent(directory, payload)

      if (Date.now() - yielded < STREAM_YIELD_MS) continue
      yielded = Date.now()
      await wait(0)
    }
  }

  const runWsAttempt = async (signal: AbortSignal) => {
    // A WebSocket upgrade can't carry an Authorization header, so it
    // authenticates purely via the oc_url_token query param. The sync token
    // getter returns "" while the token is unminted or inside its expiry skew
    // window, which would open the socket WITHOUT credentials — the server then
    // rejects it ("HTTP Authentication failed; no valid credentials available")
    // and the resulting reconnect storm churns the sync store (transient
    // status-missing → idle flicker). Mint/await a valid token BEFORE
    // connecting. (SSE avoids this: the SDK fetch sends the bearer header.)
    try {
      await refreshRuntimeUrlAuthToken()
    } catch (error) {
      const wrapped = error instanceof Error ? error : new Error("Message stream WebSocket auth token unavailable")
      if (transport === "auto") {
        wsFallbackUntil = Date.now() + WS_FALLBACK_WINDOW_MS
        ;(wrapped as Error & { code?: string }).code = "WS_FALLBACK"
      }
      ;(wrapped as Error & { reason?: string }).reason = "ws_auth_token_unavailable"
      throw wrapped
    }
    if (signal.aborted) {
      throw new DOMException("Aborted", "AbortError")
    }
    await new Promise<void>((resolve, reject) => {
      let settled = false
      let opened = false
      let readyAt = 0
      const socket: RelayTunnelWebSocket = openGlobalEventSocket(lastEventId, serverEpoch)
      const setFallbackCode = (error: Error, force = false) => {
        if ((force || !opened) && transport === "auto") {
          wsFallbackUntil = Date.now() + WS_FALLBACK_WINDOW_MS
          ;(error as Error & { code?: string }).code = "WS_FALLBACK"
        }
      }

      let readyTimer: ReturnType<typeof setTimeout> | undefined = setTimeout(() => {
        readyTimer = undefined
        const error = new Error("Message stream WebSocket ready timeout")
        setFallbackCode(error)
        settleReject(error)
        try {
          socket.close()
        } catch {
          // ignore
        }
      }, wsReadyTimeoutMs)

      const cleanup = () => {
        if (readyTimer) {
          clearTimeout(readyTimer)
          readyTimer = undefined
        }
        socket.onopen = null
        socket.onmessage = null
        socket.onerror = null
        socket.onclose = null
      }

      const settleResolve = () => {
        if (settled) return
        settled = true
        signal.removeEventListener("abort", handleAbort)
        cleanup()
        resolve()
      }

      const settleReject = (error: unknown) => {
        if (settled) return
        settled = true
        signal.removeEventListener("abort", handleAbort)
        cleanup()
        reject(error)
      }

      const handleAbort = () => {
        try {
          socket.close()
        } catch {
          // ignore close failures during abort
        }
        settleResolve()
      }

      signal.addEventListener("abort", handleAbort, { once: true })

      socket.onopen = () => {
        // Don't clear streamErrorLogged here. If the socket immediately closes
        // before sending the ready frame, clearing would cause log spam.
      }

      socket.onmessage = (messageEvent) => {
        resetHeartbeat()
        streamErrorLogged = false

        let frame: MessageStreamWsFrame | null = null
        try {
          frame = JSON.parse(String(messageEvent.data)) as MessageStreamWsFrame
        } catch (error) {
          console.warn("[event-pipeline] Failed to parse WS frame", error)
          return
        }

        if (!frame || typeof frame.type !== "string") {
          return
        }

        if (frame.type === "ready") {
          opened = true
          readyAt = Date.now()
          if (readyTimer) {
            clearTimeout(readyTimer)
            readyTimer = undefined
          }
          streamErrorLogged = false
          markConnected()
          return
        }

        if (frame.type === "error") {
          const error = new Error(frame.message || "Message stream WebSocket error")
          ;(error as Error & { reason?: string }).reason = `ws_error_frame:${frame.message || "unknown"}`
          setFallbackCode(error)
          settleReject(error)
          try {
            socket.close()
          } catch {
            // ignore
          }
          return
        }

        if (frame.type === "backpressure") {
          backpressureUntil = Date.now() + BACKPRESSURE_MODE_MS
          return
        }

        if (frame.type === "resync") {
          // Transport control from the bridge: adopt the sentinel tail
          // (empty/0 = fresh) and reconcile — everything before it was
          // disavowed by the server (docs/plan.md §5.2).
          const eventId = typeof frame.eventId === "string" ? frame.eventId : ""
          lastEventId = eventId.length > 0 && eventId !== "0" ? eventId : undefined
          if (frame.epoch !== undefined && frame.epoch.length > 0) {
            serverEpoch = frame.epoch
          }
          onResync?.("stream-resync")
          return
        }

        if (frame.type !== "event") {
          return
        }

        const payload = resolveEventPayload(frame.payload)
        if (!payload) {
          return
        }

        if (typeof frame.eventId === "string" && frame.eventId.length > 0) {
          lastEventId = frame.eventId
        }

        const directory = resolveEventDirectory(
          { directory: frame.directory, payload },
          payload,
        )
        enqueueEvent(directory, payload)
      }

      socket.onerror = () => {
        void 0
      }

      socket.onclose = (event) => {
        if (signal.aborted) {
          settleResolve()
          return
        }

        const error = new Error("Global message stream WebSocket closed")
        ;(error as Error & { reason?: string }).reason = opened
          ? `ws_closed:code=${event?.code ?? "?"}`
          : "ws_closed_before_ready"

        // Closed before the socket ever opened → the server rejected the
        // upgrade, typically an auth failure on the oc_url_token. Drop the
        // cached token so the next attempt mints a fresh one instead of
        // replaying a token the server won't accept (which would loop).
        if (!opened) {
          clearRuntimeUrlAuthToken()
        }

        // If the WS stream connects (ready) but then drops quickly, prefer SSE for a while.
        // This avoids tight reconnect loops with repeated console spam.
        const livedMs = readyAt > 0 ? Date.now() - readyAt : 0
        const unstableAfterReady = opened && livedMs > 0 && livedMs < 2_000
        setFallbackCode(error, unstableAfterReady)
        settleReject(error)
      }
    })
  }

  const resolveTransport = (): "ws" | "sse" => {
    if (typeof WebSocket !== "function") {
      return "sse"
    }
    if (transport === "ws") {
      return "ws"
    }
    if (transport === "sse") {
      return "sse"
    }
    return wsFallbackUntil > Date.now() ? "sse" : "ws"
  }

  void (async () => {
    while (!abort.signal.aborted) {
      attempt = new AbortController()
      lastEventAt = Date.now()
      attemptAbortReason = null
      let retryDelayMs = reconnectDelayMs
      const currentTransport = resolveTransport()
      activeTransport = currentTransport
      const onAbort = () => {
        attemptAbortReason = "pipeline_stopped"
        attempt?.abort()
      }
      abort.signal.addEventListener("abort", onAbort)

      try {
        if (currentTransport === "ws") {
          await runWsAttempt(attempt.signal)
        } else {
          await runSseAttempt(attempt.signal)
        }
      } catch (error) {
        const code = typeof error === "object" && error !== null ? (error as { code?: unknown }).code : undefined
        if (currentTransport === "ws" && code === "WS_FALLBACK") {
          retryDelayMs = 0
          // Transport switch (WS → SSE fallback), not a real disconnection.
          // The consumer still gets a hook so it can resync authoritative
          // state; real networks can lose/buffer events around transport flips.
          onTransportSwitch?.()
        } else if (!isAbortError(error)) {
          consecutiveFailures += 1
          if (!streamErrorLogged) {
            streamErrorLogged = true
            console.error("[event-pipeline] stream failed", error)
          }
          // Notify consumer that the stream has disconnected, so it can
          // update connection state (e.g. set isConnected = false).
          // Guard: only fire once per disconnection cycle to avoid repeated
          // setState calls on every failed retry attempt.
          const taggedReason = typeof error === "object" && error !== null
            ? (error as { reason?: unknown }).reason
            : undefined
          const message = typeof error === "object" && error !== null
            ? (error as { message?: unknown }).message
            : undefined
          const reason = typeof taggedReason === "string" && taggedReason.length > 0
            ? taggedReason
            : typeof message === "string" && message.length > 0
              ? `${currentTransport}_error:${message.slice(0, 80)}`
              : `${currentTransport}_error:unknown`
          notifyDisconnected(reason)

          // Exponential backoff so a hard-down server / dead network doesn't
          // spin the event loop. Caps lower (5s) when the user is foreground
          // and the browser thinks it's online; caps higher (60s) when hidden
          // or offline so a backgrounded PWA on a flaky link doesn't burn
          // battery. waitForRetry below resolves early on `online` or
          // visibility-visible so recovery is still under a second.
          //
          // Override for permanent 4xx errors: stuck-path / bad-auth scenarios
          // won't recover from blind retry. Use the long cap immediately so
          // the client doesn't pound the server log at 12 reqs/min. The
          // waitForRetry interrupters still apply, so a fix on the other end
          // followed by `online`/visibility recovery probes promptly.
          const status = extractStatus(error)
          if (status !== undefined && isPermanentHttpStatus(status)) {
            retryDelayMs = RETRY_BACKOFF_CAP_HIDDEN_OR_OFFLINE_MS
          } else {
            retryDelayMs = computeRetryDelay(consecutiveFailures)
          }
        }
      } finally {
        abort.signal.removeEventListener("abort", onAbort)
        attempt = undefined
        clearHeartbeat()
      }

      if (abort.signal.aborted) return
      if (attemptAbortReason && attemptAbortReason !== "pipeline_stopped") {
        notifyDisconnected(attemptAbortReason)
        retryDelayMs = 0
        attemptAbortReason = null
      }
      if (retryDelayMs > 0) {
        await waitForRetry(retryDelayMs)
      }
    }
  })().finally(flushAll)

  const onVisibility = () => {
    if (typeof document === "undefined") return
    if (document.visibilityState !== "visible") return
    if (Date.now() - lastEventAt < heartbeatTimeoutMs) return
    attempt?.abort()
  }

  const onPageShow = (event: PageTransitionEvent) => {
    if (!event.persisted) return
    attempt?.abort()
  }

  // OS wake-from-sleep (Electron powerMonitor.resume). The SSE connection
  // is almost certainly dead after sleep — abort immediately so the
  // reconnect loop fires on the next tick with retryDelayMs = 0.
  const onSystemResume = () => {
    attemptAbortReason = `${activeTransport}_system_resume`
    attempt?.abort()
  }

  // Browser told us the network is back. If we're already in a disconnected
  // cycle, abort the (stale) attempt and let the loop probe immediately;
  // waitForRetry also resolves early on `online`, so any inter-attempt sleep
  // ends now. Guard on `disconnected` so a spurious `online` from the browser
  // doesn't disrupt a healthy connection.
  const onOnline = () => {
    if (!disconnected) return
    attempt?.abort()
  }

  // Browser told us we're offline. Abort the current attempt — its socket /
  // fetch will throw soon anyway, this just stops sooner. computeRetryDelay
  // then returns the long cap so we wait for `online` instead of hammering
  // a dead network.
  const onOffline = () => {
    attempt?.abort()
  }

  const reconnect = (reason = "manual") => {
    attemptAbortReason = `${activeTransport}_${reason}`
    attempt?.abort()
  }

  if (typeof document !== "undefined") {
    document.addEventListener("visibilitychange", onVisibility)
    window.addEventListener("pageshow", onPageShow)
  }

  // Use globalThis (not window) for the system-resume listener so that
  // test environments can replace globalThis.window with a stub.
  if (typeof globalThis.window !== "undefined") {
    globalThis.window.addEventListener("ompchamber:system-resume", onSystemResume)
    globalThis.window.addEventListener("online", onOnline)
    globalThis.window.addEventListener("offline", onOffline)
  }

  const cleanup = () => {
    if (typeof document !== "undefined") {
      document.removeEventListener("visibilitychange", onVisibility)
      window.removeEventListener("pageshow", onPageShow)
    }
    if (typeof globalThis.window !== "undefined") {
      globalThis.window.removeEventListener("ompchamber:system-resume", onSystemResume)
      globalThis.window.removeEventListener("online", onOnline)
      globalThis.window.removeEventListener("offline", onOffline)
    }
    abort.abort()
    flushAll()
    // Drop every directory timer and queue reference: cleanup must not leave
    // retention chains behind (plan §5.3.1).
    for (const d of directories.values()) {
      clearTimeout(d.timer)
      d.queue.length = 0
      d.sizes.length = 0
      d.buffer.length = 0
      d.coalesced.clear()
      d.bytes = 0
    }
    directories.clear()
  }

  const stats = (): EventPipelineStats => {
    let queuedEvents = 0
    for (const d of directories.values()) {
      queuedEvents += d.queue.length
    }
    return {
      droppedEvents: overflow.droppedEvents,
      overflowResyncs: overflow.overflowResyncs,
      trackedDirectories: directories.size,
      queuedEvents,
    }
  }

  return { cleanup, reconnect, stats }
}
