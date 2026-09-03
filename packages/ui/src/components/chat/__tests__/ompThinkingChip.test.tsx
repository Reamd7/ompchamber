/**
 * ModelControls — omp thinking chip presence (GAP-06 regression pin).
 *
 * The thinking chip must render whenever the picker can name a target model:
 * the server badge, the wire session-model seed, or — while neither has
 * arrived (cold-open, event gap) — the displayed config-store model. A state
 * where the model chip shows a model but the thinking chip vanished is the
 * regression this file pins. The draft (no session) path is covered too.
 */
import { beforeEach, describe, expect, mock, test } from 'bun:test';
import { renderToStaticMarkup } from 'react-dom/server';

const DIRECTORY = '/repo';
const SESSION = 'ses_1';

const ROLES_STATE = {
    resolved: true,
    modelRoles: true,
    modelRolesEnabled: true,
    personasEnabled: false,
    snapshot: null,
    roles: [],
    pending: false,
    reload: () => undefined,
};

interface FakeConfigState {
    providers: Array<{ id: string; name: string; models: Array<{ id: string; name?: string }> }>;
    currentProviderId: string;
    currentModelId: string;
    currentVariant: string | undefined;
    currentVariantSelection: { override: string | null | undefined; inherited: string | undefined };
    currentAgentName: string | undefined;
    agents: never[];
    modelsMetadata: Map<string, unknown>;
    settingsDefaultVariant: string | undefined;
    settingsDefaultAgent: string | undefined;
    getVisibleAgents: () => never[];
    getCurrentProvider: () => { id: string; name: string; models: Array<{ id: string }> };
    getCurrentAgent: () => undefined;
    getCurrentModelVariants: () => never[];
    getModelMetadata: () => undefined;
    setProvider: (providerId: string) => void;
    setModel: (modelId: string) => void;
    setCurrentVariant: (variant: string | undefined) => void;
    setCurrentVariantOverride: (variant: string | undefined) => void;
    setAgent: (agentName: string) => void;
    setSelectedProvider: (providerId: string | null) => void;
    addRecentModel: (providerId: string, modelId: string) => void;
    addRecentAgent: (agentName: string) => void;
    addRecentEffort: (providerId: string, modelId: string, variant: string | undefined) => void;
    applyDefaultModelAgentSelection: () => void;
    applyOpenCodeConfigDefaults: () => void;
}

const configState: FakeConfigState = {
    providers: [{ id: 'axonhub', name: 'axonhub', models: [{ id: 'glm-5.3', name: 'GLM-5.3' }] }],
    currentProviderId: 'axonhub',
    currentModelId: 'glm-5.3',
    currentVariant: undefined,
    currentAgentName: undefined,
    agents: [],
    modelsMetadata: new Map(),
    currentVariantSelection: { override: undefined, inherited: undefined },
    settingsDefaultVariant: undefined,
    settingsDefaultAgent: undefined,
    getVisibleAgents: () => [],
    getCurrentProvider: () => ({ id: 'axonhub', name: 'axonhub', models: [{ id: 'glm-5.3' }] }),
    getCurrentAgent: () => undefined,
    getCurrentModelVariants: () => [],
    getModelMetadata: () => undefined,
    setProvider: () => undefined,
    setModel: () => undefined,
    setCurrentVariant: () => undefined,
    setCurrentVariantOverride: () => undefined,
    setAgent: () => undefined,
    setSelectedProvider: () => undefined,
    addRecentModel: () => undefined,
    addRecentAgent: () => undefined,
    addRecentEffort: () => undefined,
    applyDefaultModelAgentSelection: () => undefined,
    applyOpenCodeConfigDefaults: () => undefined,
};

mock.module('@/stores/useConfigStore', () => ({
    useConfigStore: Object.assign(
        <T,>(selector: (state: FakeConfigState) => T): T => selector(configState),
        { getState: () => configState, setState: (patch: Partial<FakeConfigState>) => Object.assign(configState, patch) },
    ),
}));

let fakeSessionId: string | null = SESSION;
let fakeBadge: { provider: string; id: string; thinkingLevel?: string; updatedAt: number } | null = null;
let fakeWireModel: { providerID: string; id: string } | null = null;
let fakeThinking: { thinkingLevel?: string; configured?: string; resolved?: string; updatedAt: number } | null = null;

mock.module('@/hooks/useOmpModelRoles', () => ({ useOmpModelRoles: () => ({ ...ROLES_STATE, snapshot: SNAPSHOT() }) }));
mock.module('@/hooks/useOpenCodeReadiness', () => ({ useOpenCodeReadiness: () => ({ isReady: true, isUnavailable: false }) }));

interface FakeSessionUiState {
    currentSessionId: string | null;
    currentSessionDirectory: string;
    getDirectoryForSession: () => string;
    worktreeMetadata: Map<string, unknown>;
}

mock.module('@/sync/session-ui-store', () => ({
    useSessionUIStore: Object.assign(
        <T,>(selector: (state: FakeSessionUiState) => T): T => selector({
            currentSessionId: fakeSessionId,
            currentSessionDirectory: DIRECTORY,
            getDirectoryForSession: () => DIRECTORY,
            worktreeMetadata: new Map(),
        }),
        { getState: () => ({ worktreeMetadata: new Map() }) },
    ),
}));

mock.module('@/sync/useOmpSessionStore', () => ({
    OMP_COMPACTION_LOADER_TTL_MS: 1,
    OMP_RETRY_LOADER_TTL_MS: 1,
    OMP_TTSR_TTL_MS: 1,
    OMP_AWAITING_ASYNC_TTL_MS: 1,
    sweepOmpVolatile: () => false,
    useOmpSessionStore: Object.assign(() => ({}), { getState: () => ({ directories: {}, runtimeKey: 'rt-test' }) }),
    getOmpDirectoryState: () => null,
    isOmpCompactionActive: () => false,
    useOmpThinkingState: () => fakeThinking,
    useOmpRetrySupersession: () => false,
    useOmpRetryNote: () => undefined,
    useOmpSessionLoaders: () => null,
    useOmpCustomDetails: () => null,
    useOmpSessionModelBadge: () => fakeBadge,
    useOmpFallbackState: () => null,
    useOmpAwaitingAsync: () => false,
    useOmpModeState: () => null,
    useOmpGoalState: () => null,
    useOmpPlanReview: () => null,
    useOmpTelemetry: () => null,
    useOmpSettingsRevision: () => 0,
}));

mock.module('@/sync/sync-context', () => ({
    SyncProvider: (props: { children: unknown }) => props.children,
    setActiveSession: () => undefined,
    setExternallyViewedSession: () => undefined,
    buildSessionMessageRecordsSnapshot: () => ({}),
    useAllLiveSessions: () => [],
    useAllSessionStatuses: () => ({}),
    useChildStoreManager: () => null,
    useDirectoryStore: () => null,
    useDirectorySync: () => null,
    useEnsureSessionMessages: () => undefined,
    useGlobalSessionStatus: () => null,
    useSession: () => null,
    useSessionDirectory: () => null,
    useSessionMessageRecords: () => [],
    useSessionMessages: () => [],
    useSessionMessagesResolved: () => true,
    useSessionParts: () => [],
    useSessionPermissions: () => [],
    useSessionQuestionCount: () => 0,
    useSessionQuestions: () => [],
    useSessionRenderable: () => false,
    useSessionStatus: () => null,
    useSessionWireModel: () => fakeWireModel,
    useSessions: () => [],
    useUserMessageHistory: () => [],
    getSyncParts: () => [],
    getDirectoryState: () => null,
}));
mock.module('@/sync/use-sync', () => ({
    useSync: () => ({ isLoading: () => false, ensureSessionRenderable: () => undefined }),
}));
mock.module('@/hooks/useProviderLogo', () => ({
    preloadProviderLogos: () => undefined,
    useProviderLogo: () => ({ src: null, onError: () => undefined, hasLogo: false }),
}));
mock.module('@/lib/runtime-switch', () => ({
    getRuntimeKey: () => 'rt-test',
    getRuntimeApiBaseUrl: () => '',
    initializeRuntimeEndpoint: () => undefined,
    subscribeRuntimeEndpointChanged: () => () => undefined,
    subscribeRuntimeEndpointWillChange: () => () => undefined,
    switchRuntimeEndpoint: () => undefined,
}));

function SNAPSHOT() {
    return {
        schemaVersion: '1.0',
        directory: DIRECTORY,
        models: [
            { provider: 'axonhub', id: 'glm-5.3', reasoning: true, thinking: { supported: ['high', 'xhigh'], defaultLevel: 'xhigh' } },
        ],
        roles: {
            default: { configured: 'axonhub/glm-5.3:xhigh', provider: 'axonhub', id: 'glm-5.3', thinkingLevel: 'xhigh', source: 'global' },
        },
        roleMeta: { default: { tag: 'DEFAULT', name: 'Default' } },
        cycleOrder: ['default'],
        enabledModels: [],
        fallbackChains: {},
        modelRoleStorage: 'global',
        defaultThinkingLevel: 'xhigh',
        legacyDefaults: null,
    };
}

const { ModelControls } = await import('@/components/chat/ModelControls');
const { RuntimeAPIContext } = await import('@/contexts/runtimeAPIContext');
const { I18nProvider } = await import('@/lib/i18n');
const { useConfigStore } = await import('@/stores/useConfigStore');
void useConfigStore;

const fakeRuntimeAPIs = {
    runtime: { isVSCode: false },
    ompCapabilities: { getCapabilities: async () => ({ version: 1, eventSchema: '1.0', features: {}, minUiVersion: '0.0.0' }) },
    ompModels: { getModels: async () => ({ ok: true, data: SNAPSHOT() }) },
    ompModes: { getMode: async () => ({ ok: true, data: null }), setMode: async () => ({ ok: true }) },
    ompSettings: { putModelRole: async () => ({ ok: true }) },
};

const renderControls = (): string => renderToStaticMarkup(
    // SAFETY: the harness registers only the slices ModelControls reads.
    <RuntimeAPIContext.Provider value={fakeRuntimeAPIs as never}>
        <I18nProvider>
            <ModelControls />
        </I18nProvider>
    </RuntimeAPIContext.Provider>,
);

beforeEach(() => {
    fakeSessionId = SESSION;
    fakeBadge = null;
    fakeWireModel = null;
    fakeThinking = null;
    useConfigStore.setState({
        currentProviderId: 'axonhub',
        currentModelId: 'glm-5.3',
        currentVariant: undefined,
    });
});

describe('ModelControls omp thinking chip presence', () => {
    test('renders in-session from the server badge', () => {
        fakeBadge = { provider: 'axonhub', id: 'glm-5.3', updatedAt: 1 };
        expect(renderControls()).toContain('model-controls__variant-trigger');
    });

    test('renders in-session from the wire seed when the badge has not fired', () => {
        fakeWireModel = { providerID: 'axonhub', id: 'glm-5.3' };
        expect(renderControls()).toContain('model-controls__variant-trigger');
    });

    test('renders in-session from the displayed model when neither server answer arrived', () => {
        // The regression this pins: the model chip shows GLM-5.3 while the
        // thinking chip vanished because the badge/wire seed were missing.
        expect(renderControls()).toContain('model-controls__variant-trigger');
    });

    test('renders on a draft (no session) from the picked model', () => {
        fakeSessionId = null;
        expect(renderControls()).toContain('model-controls__variant-trigger');
    });
});
