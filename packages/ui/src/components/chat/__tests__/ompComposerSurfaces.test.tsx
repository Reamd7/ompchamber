/**
 * ModelControls omp surface swap — behavioral render contracts
 * (spec 01 §5.5 GAP-05/06, 02 §5.1 D-B2, 08 §5.0).
 *
 * Replaces the retired source-text wiring assertions (issue-2903 pattern,
 * banned by master §8.3): the composer's capability-gated surfaces are
 * asserted from rendered markup, with the authoritative role/mode state
 * stubbed at the `useOmpModelRoles` seam (that hook's own fetch/gating
 * behavior is covered by ompRoleModeSurfaces.test.tsx).
 *
 * SSR note: zustand v5 selectors read the store's INITIAL state under
 * renderToStaticMarkup, so the session-ui seam is mocked with a store whose
 * initial state carries the seed; the omp per-session state and readiness
 * hooks are stubbed at their modules.
 *
 * Contracts:
 *  - modes.v1  → mode selector replaces the agent chip
 *  - personas.v1 (modes off) → persona selector replaces the agent chip
 *  - modelRoles.v1 → thinking-level slot renders the configured level and
 *    the legacy variant slot stays suppressed
 *  - capability off → legacy composer renders with no omp surface markers
 */
import { beforeEach, describe, expect, mock, test } from 'bun:test';
import { renderToStaticMarkup } from 'react-dom/server';
import { create } from 'zustand';

import type { OmpModelRolesState } from '@/hooks/useOmpModelRoles';

// ---------------------------------------------------------------------------
// Module mocks — installed before importing anything from the component
// graph (static imports would bind the real modules before these run).
// ---------------------------------------------------------------------------

const DIRECTORY = '/repo';
const SESSION = 'ses_1';

const ROLES_OFF: OmpModelRolesState = {
    resolved: true,
    modelRolesEnabled: false,
    modesEnabled: false,
    personasEnabled: false,
    snapshot: null,
    roles: [],
    pending: false,
    reload: () => undefined,
};

let fakeRoles: OmpModelRolesState = ROLES_OFF;

// The authoritative omp state seam. The real hook resolves asynchronously
// (probe + fetch), which never settles inside a static render; stubbing it
// keeps these tests about the component's render contract, not the hook's
// fetch lifecycle (covered separately).
mock.module('@/hooks/useOmpModelRoles', () => ({
    useOmpModelRoles: () => fakeRoles,
}));

// Readiness gates every picker trigger on "Loading…"; the config store it
// reads cannot be seeded under SSR initial-state semantics, so pin the hook.
mock.module('@/hooks/useOpenCodeReadiness', () => ({
    useOpenCodeReadiness: () => ({
        isReady: true,
        isLoading: false,
        isUnavailable: false,
        connectionPhase: 'connected',
    }),
}));

// Session identity seam: a real (tiny) zustand store whose INITIAL state
// carries the current session — SSR selectors read initial state.
mock.module('@/sync/session-ui-store', () => ({
    useSessionUIStore: create<Record<string, unknown>>(() => ({
        currentSessionId: SESSION,
        currentSessionDirectory: DIRECTORY,
        newSessionDraft: { open: false },
        worktreeMetadata: new Map(),
        getDirectoryForSession: (sessionId: string) =>
            sessionId === SESSION ? DIRECTORY : null,
    })),
    expandSlashCommandGoalObjective: (text: string) => text,
    routeMessage: () => undefined,
    getRememberedSessionDirectory: () => null,
    acquireFirstTurnDialogLease: async () => false,
    materializeOpenDraftSession: async () => null,
}));

// Per-session omp state: thinking level + session model badge come from the
// seeded values; the remaining selectors degrade to their empty answers.
mock.module('@/sync/useOmpSessionStore', () => ({
    OMP_COMPACTION_LOADER_TTL_MS: 1,
    OMP_RETRY_LOADER_TTL_MS: 1,
    OMP_TTSR_TTL_MS: 1,
    OMP_AWAITING_ASYNC_TTL_MS: 1,
    sweepOmpVolatile: () => false,
    useOmpSessionStore: create<Record<string, unknown>>(() => ({
        directories: {},
        runtimeKey: 'rt-test',
    })),
    getOmpDirectoryState: () => null,
    isOmpCompactionActive: () => false,
    useOmpThinkingState: () => ({ configured: 'max', thinkingLevel: 'high', updatedAt: 1 }),
    useOmpRetrySupersession: () => false,
    useOmpRetryNote: () => undefined,
    useOmpSessionLoaders: () => null,
    useOmpCustomDetails: () => null,
    useOmpSessionModelBadge: () => ({ provider: 'prov', id: 'main', thinkingLevel: 'high', updatedAt: 1 }),
    useOmpFallbackState: () => null,
    useOmpAwaitingAsync: () => false,
    useOmpModeState: () => null,
    useOmpGoalState: () => null,
    useOmpPlanReview: () => null,
    useOmpTelemetry: () => null,
}));

// Session sync reads are irrelevant to the surface swap; neutralize the
// context-guarded hooks so the composer renders without a sync provider.
// Every named export consumed across the graph is listed so ESM linking
// succeeds (a partial mock breaks named-import validation).
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
    useSessionWireModel: () => null,
    useSessions: () => [],
    useUserMessageHistory: () => [],
}));

// useSync pulls the whole sync-system context; ModelControls only reads
// `isLoading` / `ensureSessionRenderable` (both inside effects).
mock.module('@/sync/use-sync', () => ({
    useSync: () => ({ isLoading: () => false, ensureSessionRenderable: () => undefined }),
}));

// ModelControls' picker graph pulls rsbuild's import.meta.glob through
// useProviderLogo; stub it so the composer renders under plain `bun test`
// (same treatment as the settings-surface suite gives ModelSelector).
mock.module('@/hooks/useProviderLogo', () => ({
    preloadProviderLogos: () => undefined,
    useProviderLogo: () => ({ src: null, onError: () => undefined, hasLogo: false }),
}));

mock.module('@/lib/runtime-switch', () => ({
    getRuntimeApiBaseUrl: () => 'http://localhost/',
    getRuntimeKey: () => 'rt-test',
    initializeRuntimeEndpoint: () => undefined,
    switchRuntimeEndpoint: () => undefined,
    subscribeRuntimeEndpointWillChange: () => () => undefined,
    subscribeRuntimeEndpointChanged: () => () => undefined,
}));

const { ModelControls } = await import('@/components/chat/ModelControls');
const { RuntimeAPIContext } = await import('@/contexts/runtimeAPIContext');
const { I18nProvider } = await import('@/lib/i18n');

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/** Snapshot whose `prov/main` model carries three concrete thinking levels. */
const SNAPSHOT = {
    schemaVersion: '1.0',
    directory: DIRECTORY,
    models: [{
        provider: 'prov',
        id: 'main',
        name: 'Main',
        reasoning: true,
        contextWindow: 200000,
        maxTokens: 32000,
        thinking: { supported: ['low', 'high', 'max'], defaultLevel: 'high' },
    }],
    roles: {
        default: { configured: 'prov/main:high', provider: 'prov', id: 'main', thinkingLevel: 'high', source: 'global' },
    },
    roleMeta: { default: { tag: 'DEFAULT', name: 'Default' } },
    cycleOrder: ['default'],
    enabledModels: [],
    fallbackChains: {},
    modelRoleStorage: 'global',
    defaultThinkingLevel: 'high',
    legacyDefaults: null,
} as unknown as OmpModelRolesState['snapshot'];

const fakeRuntimeAPIs = {
    runtime: { isVSCode: false },
    ompCapabilities: { getCapabilities: async () => ({ version: 1, eventSchema: '1.0', features: {}, minUiVersion: '0.0.0' }) },
    ompModels: { getModels: async () => ({ ok: true, data: null }) },
    ompModes: { getMode: async () => ({ ok: true, data: null }), setMode: async () => ({ ok: true }) },
    ompSettings: { putModelRole: async () => ({ ok: true }) },
    ompPersonas: { list: async () => ({ ok: true, data: [] }) },
};

const render = (): string =>
    renderToStaticMarkup(
        <RuntimeAPIContext.Provider value={fakeRuntimeAPIs as never}>
            <I18nProvider>
                <ModelControls />
            </I18nProvider>
        </RuntimeAPIContext.Provider>,
    );

beforeEach(() => {
    fakeRoles = ROLES_OFF;
});

// ---------------------------------------------------------------------------
// Surface swap (08 §5.0: capability decides which chip owns the composer)
// ---------------------------------------------------------------------------

describe('ModelControls — capability-gated surface swap', () => {
    test('modes.v1 on → mode selector replaces the agent chip', () => {
        fakeRoles = { ...ROLES_OFF, modelRolesEnabled: true, modesEnabled: true, snapshot: SNAPSHOT };
        const markup = render();
        expect(markup).toContain('data-testid="omp-mode-selector"');
        expect(markup).not.toContain('omp-persona-selector');
        expect(markup).not.toContain('model-controls__agent-label');
    });

    test('personas.v1 on (modes off) → persona selector replaces the agent chip', () => {
        fakeRoles = { ...ROLES_OFF, modelRolesEnabled: true, personasEnabled: true, snapshot: SNAPSHOT };
        const markup = render();
        expect(markup).toContain('data-testid="omp-persona-selector"');
        expect(markup).toContain('model-controls__persona-trigger');
        expect(markup).not.toContain('data-testid="omp-mode-selector"');
        expect(markup).not.toContain('model-controls__agent-label');
    });

    test('model roles on → thinking slot shows the configured level; legacy variant slot suppressed', () => {
        // Personas/modes off keeps the legacy agent chip visible; the variant
        // slot, however, is owned by the thinking level under roles (GAP-06).
        fakeRoles = { ...ROLES_OFF, modelRolesEnabled: true, snapshot: SNAPSHOT };
        const markup = render();
        expect(markup).toContain('model-controls__variant-trigger');
        // activeLevel = thinking.configured ('max') → capitalized label.
        expect(markup).toContain('>Max</span>');
        expect(markup).toContain('model-controls__agent-label');
    });

    test('capability off → legacy composer renders with no omp surface markers', () => {
        fakeRoles = ROLES_OFF;
        const markup = render();
        expect(markup).toContain('model-controls__agent-label');
        expect(markup).not.toContain('omp-persona-selector');
        expect(markup).not.toContain('omp-mode-selector');
        // No variants configured and roles off → no variant/thinking trigger.
        expect(markup).not.toContain('model-controls__variant-trigger');
    });
});
