/**
 * materializeOpenDraftSession — omp draft model application (spec 01
 * GAP-02/GAP-04).
 *
 * Under the omp model-roles capability prompts are model-free, so the model
 * picked before a new chat starts must be applied to the freshly created
 * session server-side (POST /api/omp/sessions/{id}/model) before the first
 * turn routes. These tests pin the gate, the request arguments, and the
 * failure degradation (send never blocked).
 */
import { beforeEach, describe, expect, mock, test } from "bun:test"

const storage = new Map<string, string>()
const createSessionCalls: Array<{ title?: string; directory: string | null; parentID: string | null }> = []
let createdSessionDirectory: string | undefined

let ompGateEnabled = false
const setSessionModelCalls: Array<{
  sessionID: string
  model: { providerID: string; modelID: string }
  options: { directory: string }
}> = []
let setSessionModelResult: { ok: true; model: string } | { ok: false; unavailable: boolean } = { ok: true, model: "prov/next" }

const deferredStorage: Storage = {
  getItem: (key: string) => storage.get(key) ?? null,
  setItem: (key: string, value: string) => {
    storage.set(key, value)
  },
  removeItem: (key: string) => {
    storage.delete(key)
  },
  clear: () => {
    storage.clear()
  },
  key: (index: number) => Array.from(storage.keys())[index] ?? null,
  get length() {
    return storage.size
  },
}

mock.module("@/stores/utils/safeStorage", () => ({
  getDeferredSafeStorage: () => deferredStorage,
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
  usePermissionStore: {
    getState: () => ({
      setSessionAutoAccept: mock(async () => undefined),
    }),
  },
}))

mock.module("@/stores/useConfigStore", () => ({
  useConfigStore: {
    getState: () => ({
      currentAgentName: "agent-default",
      agents: [],
      activateDirectory: mock(async () => undefined),
      applyDefaultModelAgentSelection: mock(() => undefined),
    }),
  },
}))

mock.module("@/stores/useProjectsStore", () => ({
  useProjectsStore: {
    getState: () => ({
      projects: [],
      activeProjectId: null,
      getActiveProject: () => null,
    }),
  },
}))

mock.module("@/stores/useDirectoryStore", () => ({
  useDirectoryStore: {
    getState: () => ({
      currentDirectory: null,
      setDirectory: mock(() => undefined),
    }),
  },
}))

mock.module("@/stores/useGlobalSessionsStore", () => ({
  useGlobalSessionsStore: {
    getState: () => ({
      activeSessions: [],
      archivedSessions: [],
    }),
  },
  resolveGlobalSessionDirectory: () => null,
}))

mock.module("@/stores/useSessionFoldersStore", () => ({
  useSessionFoldersStore: {
    getState: () => ({
      addSessionToFolder: mock(() => undefined),
    }),
  },
}))

mock.module("@/stores/useCommandsStore", () => ({
  useCommandsStore: {
    getState: () => ({
      commands: [],
    }),
  },
}))

mock.module("@/stores/useSkillsStore", () => ({
  useSkillsStore: {
    getState: () => ({
      skills: [],
    }),
  },
}))

mock.module("@/components/ui", () => ({
  toast: {
    error: () => undefined,
    info: () => undefined,
    success: () => undefined,
  },
}))

mock.module("../selection-store", () => ({
  useSelectionStore: {
    getState: () => ({
      saveSessionModelSelection: () => undefined,
      saveSessionAgentSelection: () => undefined,
      saveAgentModelForSession: () => undefined,
      saveAgentModelVariantForSession: () => undefined,
      getSessionAgentSelection: () => null,
      getSessionModelSelection: () => null,
      getAgentModelForSession: () => null,
      getAgentModelVariantForSession: () => undefined,
    }),
  },
}))

mock.module("@/lib/runtime-switch", () => ({
  getRuntimeApiBaseUrl: () => "",
  getRuntimeKey: () => "test-runtime",
  initializeRuntimeEndpoint: () => undefined,
  subscribeRuntimeEndpointChanged: () => () => undefined,
  switchRuntimeEndpoint: () => undefined,
}))

mock.module("@/lib/userSendAnimation", () => ({
  markPendingUserSendAnimation: () => undefined,
}))

mock.module("../sync-context", () => ({
  setActiveSession: () => undefined,
}))

mock.module("../notification-store", () => ({
  markSessionViewed: () => undefined,
}))

mock.module("../session-navigation", () => ({
  setSessionOpener: () => undefined,
}))

mock.module("../session-worktree-contract", () => ({
  getAttachedSessionDirectory: () => null,
}))

mock.module("../session-worktree-store", () => ({
  useSessionWorktreeStore: {
    getState: () => ({
      getAttachment: () => undefined,
      setAttachment: () => undefined,
      clearAttachment: () => undefined,
    }),
  },
}))

mock.module("../viewport-store", () => ({
  getViewportSessionMemory: () => null,
  viewportSessionKey: (sessionId: string) => sessionId,
  useViewportStore: {
    getState: () => ({
      updateViewportAnchor: mock(() => undefined),
    }),
    setState: () => undefined,
  },
}))

mock.module("../input-store", () => ({
  useInputStore: {
    getState: () => ({
      clearAttachedFiles: () => undefined,
      setPendingInputText: () => undefined,
      addRestoredAttachment: () => undefined,
    }),
  },
}))

mock.module("../sync-refs", () => ({
  getDirectoryState: () => null,
  getSyncSessions: () => [],
  getSyncMessages: () => [],
  getSyncParts: () => [],
  getAllSyncSessions: () => [],
  getSyncSessionDirectory: () => null,
}))

mock.module("../session-actions", () => ({
  createSession: mock(async (title: string | undefined, directory: string | null, parentID: string | null) => {
    createSessionCalls.push({ title, directory, parentID })
    return { id: "ses_omp_draft", directory: createdSessionDirectory ?? directory }
  }),
  deleteSession: mock(async () => true),
  deleteSessions: mock(async () => ({ deletedIds: [], failedIds: [] })),
  archiveSession: mock(async () => true),
  archiveSessions: mock(async () => ({ archivedIds: [], restoredIds: [] })),
  unarchiveSession: mock(async () => true),
  unarchiveSessions: mock(async () => ({ restoredIds: [], failedIds: [] })),
  updateSessionTitle: mock(async () => undefined),
  optimisticSend: mock(async () => undefined),
  refetchSessionMessages: mock(async () => undefined),
  revertToMessage: mock(async () => undefined),
  unrevertSession: mock(async () => undefined),
  forkFromMessage: mock(async () => undefined),
  forkSession: mock(async () => undefined),
  fetchMessagesForSession: mock(async () => undefined),
  getSessionLastAssistantModel: () => null,
  patchSessionMetadata: mock(async () => undefined),
  abortCurrentOperation: mock(async () => undefined),
}))

mock.module("@/lib/omp/capabilityGate", () => ({
  isOmpFeatureEnabled: () => ompGateEnabled,
  isOmpModelRolesEnabled: () => ompGateEnabled,
  isOmpModesEnabled: () => false,
  isOmpPersonasEnabled: () => false,
  isOmpAgentDefinitionsEnabled: () => false,
  primeOmpCapabilityGate: async () => ({ capabilities: null }),
  __resetOmpCapabilityGateForTests: () => undefined,
}))

mock.module("@/contexts/runtimeAPIRegistry", () => ({
  getRegisteredRuntimeAPIs: () => ({
    ompModels: {
      setSessionModel: mock(async (
        sessionID: string,
        model: { providerID: string; modelID: string },
        options: { directory: string },
      ) => {
        setSessionModelCalls.push({ sessionID, model, options })
        return setSessionModelResult
      }),
    },
  }),
}))

const { applyOmpSessionModelToFreshSession, materializeOpenDraftSession, useSessionUIStore } = await import("../session-ui-store")

const openDraft = (directory: string | null) => {
  // SAFETY: partial NewSessionDraftState — materializeOpenDraftSession only
  // reads the open/directoryOverride/bootstrapPending fields set here.
  useSessionUIStore.setState({ newSessionDraft: { open: true, directoryOverride: directory } } as never)
}

beforeEach(() => {
  ompGateEnabled = false
  setSessionModelCalls.length = 0
  createSessionCalls.length = 0
  createdSessionDirectory = undefined
  setSessionModelResult = { ok: true, model: "prov/next" }
  // SAFETY: partial SessionUIState reset — only the fields materialize and
  // the assertions read are restored between tests.
  useSessionUIStore.setState({
    currentSessionId: null,
    currentSessionDirectory: null,
    newSessionDraft: { open: false },
  } as never)
})

describe("materializeOpenDraftSession omp model application", () => {
  test("does not write when the gate is off or no model was picked", async () => {
    openDraft("/repo")
    await materializeOpenDraftSession({ providerID: "prov", modelID: "next" })
    expect(setSessionModelCalls).toEqual([])

    ompGateEnabled = true
    await materializeOpenDraftSession({ agent: "build" })
    expect(setSessionModelCalls).toEqual([])
  })

  test("applies the picked model to the fresh session when the omp gate is on", async () => {
    ompGateEnabled = true
    openDraft("/repo")

    const result = await materializeOpenDraftSession({ providerID: "prov", modelID: "next", variant: "xhigh" })

    expect(result?.sessionId).toBe("ses_omp_draft")
    // Regression pin: the new session must become the current one (an
    // editing accident once dropped this line while adding the switch).
    expect(useSessionUIStore.getState().currentSessionId).toBe("ses_omp_draft")
    expect(setSessionModelCalls).toEqual([{
      sessionID: "ses_omp_draft",
      model: { providerID: "prov", modelID: "next" },
      options: { directory: "/repo", thinkingLevel: "xhigh" },
    }])
  })

  test("a failed switch degrades to the engine default without blocking the draft", async () => {
    ompGateEnabled = true
    setSessionModelResult = { ok: false, unavailable: false }
    openDraft("/repo")

    const result = await materializeOpenDraftSession({ providerID: "prov", modelID: "next" })

    expect(result?.sessionId).toBe("ses_omp_draft")
    expect(setSessionModelCalls).toHaveLength(1)
  })

  test("applyOmpSessionModelToFreshSession skips a directory-less session and writes the scoped one", async () => {
    ompGateEnabled = true

    // No directory to scope the write — the fork/multirun callers rely on
    // this being a no-op rather than an unscoped request.
    await applyOmpSessionModelToFreshSession("ses_x", null, "prov", "next")
    expect(setSessionModelCalls).toEqual([])

    await applyOmpSessionModelToFreshSession("ses_x", "/worktree", "prov", "next")
    expect(setSessionModelCalls).toEqual([{
      sessionID: "ses_x",
      model: { providerID: "prov", modelID: "next" },
      options: { directory: "/worktree" },
    }])
  })
})
