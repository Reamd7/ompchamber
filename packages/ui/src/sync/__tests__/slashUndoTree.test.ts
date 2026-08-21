/**
 * /undo under the omp tree capability (spec 04 §5.4.5 GAP-06): the revert
 * marker lands on the message BEFORE the target user message (leaf parent)
 * and the composer is prefilled with that message's text via the input
 * store. Capability off/unsettled keeps the legacy marker exactly (the user
 * message itself stays the last retained message, no prefill).
 *
 * The store is loaded after the module mocks, mirroring issue-2039.test.ts.
 */
import { beforeEach, describe, expect, mock, test } from "bun:test"

let treeEnabled = false
let syncMessages: Array<{ id: string; role: string }> = []
const partsByMessage = new Map<string, Array<{ type: string; text?: string }>>()
const revertCalls: Array<{ sessionId: string; messageId: string }> = []
const prefillCalls: Array<{ text: string; mode: string }> = []
const toasts: string[] = []



const storage = {
  getItem: () => null,
  setItem: () => undefined,
  removeItem: () => undefined,
  clear: () => undefined,
  key: () => null,
  length: 0,
} as Storage

mock.module("zustand", () => ({
  create: () => (initializer: (
    set: unknown,
    get: unknown,
    _api?: unknown,
  ) => Record<string, unknown>) => {
    let state: Record<string, unknown>
    const get = () => state
    const set = (patch: unknown) => {
      const next = typeof patch === "function" ? (patch as (c: unknown) => unknown)(state) : patch
      state = next && typeof next === "object" ? { ...state, ...(next as Record<string, unknown>) } : state
    }
    state = initializer(set, get, {
      setState: set, getState: get, getInitialState: get, subscribe: () => () => undefined,
    } as never)
    const store = ((selector?: (current: Record<string, unknown>) => unknown) =>
      typeof selector === "function" ? selector(state) : state) as unknown as {
      getState: () => Record<string, unknown>
      setState: (patch: unknown) => void
      subscribe: () => () => void
    }
    store.getState = () => state
    store.setState = set
    store.subscribe = () => () => undefined
    return store
  },
}))

mock.module("@/stores/utils/safeStorage", () => ({
  getDeferredSafeStorage: () => storage,
  createDeferredSafeJSONStorage: () => ({
    getItem: async () => null,
    setItem: async () => undefined,
    removeItem: async () => undefined,
  }),
}))

mock.module("@/lib/opencode/client", () => ({
  opencodeClient: {
    getDirectory: () => null,
    setDirectory: mock(() => undefined),
  },
}))

mock.module("@/stores/permissionStore", () => ({
  usePermissionStore: { getState: () => ({ setSessionAutoAccept: mock(async () => undefined) }) },
}))

mock.module("@/stores/useConfigStore", () => ({
  useConfigStore: { getState: () => ({ currentAgentName: "build", agents: [] }) },
}))

mock.module("@/stores/useProjectsStore", () => ({
  useProjectsStore: { getState: () => ({ projects: [], activeProjectId: null, getActiveProject: () => null }) },
}))

mock.module("@/stores/useDirectoryStore", () => ({
  useDirectoryStore: { getState: () => ({ currentDirectory: null, setDirectory: mock(() => undefined) }) },
}))

mock.module("@/stores/useGlobalSessionsStore", () => ({
  useGlobalSessionsStore: { getState: () => ({ activeSessions: [], archivedSessions: [] }) },
  resolveGlobalSessionDirectory: () => null,
}))

mock.module("@/stores/useSessionFoldersStore", () => ({
  useSessionFoldersStore: { getState: () => ({ addSessionToFolder: mock(() => undefined) }) },
}))

mock.module("@/stores/useCommandsStore", () => ({
  useCommandsStore: { getState: () => ({ commands: [] }) },
}))

mock.module("@/stores/useSkillsStore", () => ({
  useSkillsStore: { getState: () => ({ skills: [] }) },
}))

mock.module("sonner", () => ({
  toast: { error: () => undefined, info: () => undefined, success: (message: string) => toasts.push(message) },
}))

mock.module("@/lib/i18n/store", () => ({
  useI18nStore: { getState: () => ({ dictionary: {} }), setState: () => undefined, subscribe: () => () => undefined },
  formatMessage: (_dictionary: unknown, key: string) => key,
  initializeLocale: () => undefined,
  resetI18nDictionaryCacheForTests: () => undefined,
}))

mock.module("../selection-store", () => ({
  useSelectionStore: { getState: () => ({}) },
}))

mock.module("@/lib/runtime-switch", () => ({
  getRuntimeApiBaseUrl: () => "",
  getRuntimeKey: () => "test-runtime",
  initializeRuntimeEndpoint: () => undefined,
  subscribeRuntimeEndpointChanged: () => () => undefined,
  switchRuntimeEndpoint: () => undefined,
}))

mock.module("@/lib/userSendAnimation", () => ({ markPendingUserSendAnimation: () => undefined }))

mock.module("../sync-context", () => ({ setActiveSession: () => undefined }))

mock.module("../notification-store", () => ({ markSessionViewed: () => undefined }))

mock.module("../session-navigation", () => ({ setSessionOpener: () => undefined }))
mock.module("../session-worktree-store", () => ({
  useSessionWorktreeStore: {
    getState: () => ({ getAttachment: () => undefined, setAttachment: () => undefined, clearAttachment: () => undefined }),
  },
}))

mock.module("../viewport-store", () => ({
  getViewportSessionMemory: () => null,
  viewportSessionKey: (sessionId: string) => sessionId,
  useViewportStore: { getState: () => ({ updateViewportAnchor: mock(() => undefined) }), setState: () => undefined },
}))

mock.module("../input-store", () => ({
  useInputStore: {
    getState: () => ({
      clearAttachedFiles: () => undefined,
      setPendingInputText: (text: string, mode: string) => prefillCalls.push({ text, mode }),
      addRestoredAttachment: () => undefined,
    }),
  },
}))

mock.module("../sync-refs", () => ({
  getDirectoryState: () => null,
  getSyncSessions: () => [{ id: "ses_1" }],
  getSyncMessages: () => syncMessages,
  getSyncParts: (messageId: string) => partsByMessage.get(messageId) ?? [],
  getAllSyncSessions: () => [],
  getSyncSessionDirectory: () => null,
}))

mock.module("@/lib/omp/capabilityGate", () => ({
  isOmpFeatureEnabled: (key: string) => key === "tree.v1" && treeEnabled,
  primeOmpCapabilityGate: async () => ({ capabilities: null }),
}))

mock.module("../session-actions", () => ({
  deleteSession: mock(async () => true),
  deleteSessions: mock(async () => ({ deletedIds: [], failedIds: [] })),
  archiveSession: mock(async () => true),
  archiveSessions: mock(async () => ({ archivedIds: [], failedIds: [] })),
  unarchiveSession: mock(async () => true),
  unarchiveSessions: mock(async () => ({ restoredIds: [], failedIds: [] })),
  updateSessionTitle: mock(async () => undefined),
  optimisticSend: mock(async () => undefined),
  refetchSessionMessages: mock(async () => undefined),
  revertToMessage: mock(async (sessionId: string, messageId: string) => {
    revertCalls.push({ sessionId, messageId })
  }),
  unrevertSession: mock(async () => undefined),
  forkFromMessage: mock(async () => undefined),
  fetchMessagesForSession: mock(async () => undefined),
  getSessionLastAssistantModel: () => null,
  setActionRefs: () => undefined,
  setOptimisticRefs: () => undefined,
  moveSessionToDirectory: mock(async () => undefined),
  waitForConnectionOrThrow: mock(async () => undefined),
  isQuestionRequestNotFoundError: () => undefined,
  createSession: mock(async () => ({ id: "ses_new" })),
  setLinkedIssue: () => undefined,
  setContextObligatoryMessage: () => undefined,
  deleteSessionInDirectory: mock(async () => undefined),
  abortCurrentOperation: mock(async () => undefined),
  respondToPermission: mock(async () => undefined),
  dismissPermission: mock(async () => undefined),
  dismissOpenPermissionsForSession: mock(async () => undefined),
  respondToQuestion: mock(async () => undefined),
  rejectQuestion: mock(async () => undefined),
  dismissOpenQuestionsForSession: mock(async () => undefined),
  patchSessionMetadata: mock(async () => undefined),
  waitForConnection: async () => undefined,
}))

mock.module("@/lib/omp/capabilityGate", () => ({
  // Pure mock (importing the real module first would cache it and defeat
  // mock.module); every consumer key is listed explicitly.
  primeOmpCapabilityGate: async () => ({ capabilities: null }),
  isOmpFeatureEnabled: (key: string) => key === "tree.v1" && treeEnabled,
  isOmpModelRolesEnabled: () => false,
  isOmpModesEnabled: () => false,
  isOmpAgentDefinitionsEnabled: () => false,
  isOmpPersonasEnabled: () => false,
  __resetOmpCapabilityGateForTests: () => undefined,
}))

const seedConversation = () => {
  syncMessages = [
    { id: "u1", role: "user" },
    { id: "a1", role: "assistant" },
    { id: "u2", role: "user" },
    { id: "a2", role: "assistant" },
  ]
  partsByMessage.clear()
  partsByMessage.set("u2", [{ type: "text", text: "Second question" }])
  partsByMessage.set("u1", [{ type: "text", text: "First question" }])
}

describe("handleSlashUndo omp tree semantics (04 §5.4.5 GAP-06)", () => {
  beforeEach(() => {
    treeEnabled = false
    revertCalls.length = 0
    prefillCalls.length = 0
    toasts.length = 0
    seedConversation()
  })

  test("tree.v1 on: marker lands on the preceding message and the composer is prefilled", async () => {
    treeEnabled = true
    const { useSessionUIStore } = await import("../session-ui-store")
    await useSessionUIStore.getState().handleSlashUndo("ses_1")
    expect(revertCalls).toEqual([{ sessionId: "ses_1", messageId: "a1" }])
    expect(prefillCalls).toEqual([{ text: "Second question", mode: "replace" }])
    expect(toasts).toEqual(["chat.revert.toast.undoPrefilled"])
  })

  test("tree.v1 off: legacy marker on the user message itself, no prefill", async () => {
    const { useSessionUIStore } = await import("../session-ui-store")
    await useSessionUIStore.getState().handleSlashUndo("ses_1")
    expect(revertCalls).toEqual([{ sessionId: "ses_1", messageId: "u2" }])
    expect(prefillCalls).toEqual([])
    expect(toasts).toEqual(["chat.revert.toast.undo"])
  })

  test("first message has no leaf parent: legacy fallback even under tree.v1", async () => {
    treeEnabled = true
    syncMessages = [{ id: "u1", role: "user" }]
    const { useSessionUIStore } = await import("../session-ui-store")
    await useSessionUIStore.getState().handleSlashUndo("ses_1")
    expect(revertCalls).toEqual([{ sessionId: "ses_1", messageId: "u1" }])
    expect(prefillCalls).toEqual([])
  })
})
