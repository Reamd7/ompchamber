/**
 * Sync debug logging — gated behind localStorage flag.
 *
 * Enable in browser console:
 *   localStorage.setItem("ompchamber:sync:debug", "1")
 *
 * Disable:
 *   localStorage.removeItem("ompchamber:sync:debug")
 *
 * All checks are early-returns on the hot path — zero cost when disabled.
 */

const FLAG_KEY = "ompchamber:sync:debug"

let _enabled: boolean | undefined

function isSyncDebugEnabled(): boolean {
  if (_enabled !== undefined) return _enabled
  try {
    _enabled = typeof localStorage !== "undefined" && localStorage.getItem(FLAG_KEY) === "1"
  } catch {
    _enabled = false
  }
  return _enabled
}
type SyncDebugCategory = "pipeline" | "reducer" | "dispatch" | "recovery" | "omp"

function log(cat: SyncDebugCategory, ...args: unknown[]): void {
  if (!isSyncDebugEnabled()) return
  const tag = `%c[sync:${cat}]`
  const style = "color: #888"
  console.log(tag, style, ...args)
}

export const syncDebug = {
  pipeline: {
    /** Event coalesced (replaced an earlier event in the queue). */
    coalesced: (eventType: string, coalesceKey: string) =>
      log("pipeline", "coalesced", eventType, coalesceKey),

    /** Flush batch dispatched. */
    flush: (count: number) =>
      log("pipeline", "flush", `${count} events`),
  },

  reducer: {
    /** message.updated skipped because role/finish/completed matched existing. */
    messageUpdatedUnchanged: (sessionID: string, messageID: string, role: string, finish: unknown, completed: unknown) =>
      log("reducer", "message.updated UNCHANGED (skipped)", { sessionID, messageID, role, finish, completed }),

    /** message.part.updated arrived but no parts array exists for this messageID. */
    partUpdatedNoExistingParts: (messageID: string, partID: string, partType: string) =>
      log("reducer", "message.part.updated NO EXISTING PARTS", { messageID, partID, partType }),

    /** message.part.delta arrived but parts array missing — silently dropped. */
    partDeltaNoParts: (messageID: string, partID: string) =>
      log("reducer", "message.part.delta DROPPED (no parts array)", { messageID, partID }),

    /** message.part.delta arrived but partID not found in parts array. */
    partDeltaNotFound: (messageID: string, partID: string) =>
      log("reducer", "message.part.delta DROPPED (partID not found)", { messageID, partID }),

    /** SKIP_PARTS filtered out a part. */
    partSkipped: (messageID: string, partID: string, partType: string) =>
      log("reducer", "message.part.updated SKIPPED (type filtered)", { messageID, partID, partType }),
  },

  dispatch: {
    /** Event dispatched to store but reducer returned false (no state change). */
    eventNoChange: (eventType: string, sessionID?: string, messageID?: string) =>
      log("dispatch", "event → no change", { eventType, sessionID, messageID }),

    /** Event applied to store successfully. */
    eventApplied: (eventType: string, sessionID?: string, messageID?: string) =>
      log("dispatch", "event → applied", { eventType, sessionID, messageID }),
  },

  recovery: {
    /** A scoped session snapshot fetch is starting because live state looked incomplete. */
    materializing: (details: { reason: string; directory: string; sessionID: string; messageID?: string; partID?: string }) =>
      log("recovery", "materializing session", details),
  },

  omp: {
    /** Pipeline did not start: capabilities missing/feature off (wire-only degradation). */
    dormant: (reason: string) =>
      log("omp", "pipeline DORMANT (wire-only)", reason),

    /** Pipeline started after capability negotiation. */
    started: (eventSchema: string) =>
      log("omp", "pipeline started", { eventSchema }),

    /** Unknown omp event type ignored (minor-version addition, spec 05 §5.2.3). */
    unknownEvent: (type: string, id: number) =>
      log("omp", "unknown event type IGNORED", { type, id }),

    /** Event failed its payload schema — consumed without state change. */
    droppedEvent: (type: string, id: number) =>
      log("omp", "event DROPPED (payload shape)", { type, id }),

    /** omp.custom.appended with display:false — no card on any path (T3). */
    customHidden: (wireMessageID: string, customType: string) =>
      log("omp", "custom.appended hidden (display:false)", { wireMessageID, customType }),

    /** Resync control frame: authoritative GET per scope is running. */
    resync: (scopes: string[], lastEventId: number | null) =>
      log("omp", "stream resync", { scopes, lastEventId }),
  },
} as const
