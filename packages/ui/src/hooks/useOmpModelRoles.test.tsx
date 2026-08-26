/**
 * useOmpModelRoles — live refresh contracts.
 *
 * Role assignments change through the engine settings page, this picker's
 * own "Set as role" action, and the omp CLI; each write broadcasts
 * omp.settings.updated and the reducer stores the directory's settings
 * revision. The composer's models snapshot must follow that revision
 * instead of staying stale until a remount, and a failed refresh must not
 * blank valid role chips.
 */
import { act } from 'react';
import { createRoot, type Root } from 'react-dom/client';
import { beforeEach, describe, expect, mock, test } from 'bun:test';

import type { OmpModelsSnapshot } from '@/lib/api/omp';
import type { OmpModelRolesState } from '@/hooks/useOmpModelRoles';

mock.module('@/lib/omp/capabilityGate', () => ({
    isOmpFeatureEnabled: () => true,
    isOmpModelRolesEnabled: () => true,
    isOmpModesEnabled: () => true,
    isOmpPersonasEnabled: () => false,
    isOmpAgentDefinitionsEnabled: () => false,
    primeOmpCapabilityGate: async () => ({ capabilities: { version: 1, eventSchema: '1.0', features: { 'modelRoles.v1': true, 'modes.v1': true }, minUiVersion: '0.0.0' } }),
    __resetOmpCapabilityGateForTests: () => undefined,
}));

mock.module('@/lib/runtime-switch', () => ({
    getRuntimeKey: () => 'test-rt',
    getRuntimeApiBaseUrl: () => '',
    subscribeRuntimeEndpointWillChange: () => () => undefined,
    initializeRuntimeEndpoint: () => undefined,
    subscribeRuntimeEndpointChanged: () => () => undefined,
    switchRuntimeEndpoint: () => undefined,
}));

const getModelsCalls: string[] = [];
let nextSnapshot: OmpModelsSnapshot | null = null;
let nextOk = true;

// Stable identity across renders — the hook's fetch effect keys on ompModels.
const ompModelsMock = {
    getModels: mock(async (options: { directory: string }) => {
        getModelsCalls.push(options.directory);
        return nextOk && nextSnapshot
            ? { ok: true, data: nextSnapshot }
            : { ok: false, unavailable: false };
    }),
};

mock.module('@/hooks/useRuntimeAPIs', () => ({
    useRuntimeAPIs: () => ({ ompModels: ompModelsMock }),
    useIsVSCodeRuntime: () => false,
}));

const { useOmpModelRoles } = await import('@/hooks/useOmpModelRoles');
const { useOmpSessionStore } = await import('@/sync/useOmpSessionStore');

// SAFETY: fixture matches ModelsSnapshotSchema's shape (see
const snapshotWith = (roles: OmpModelsSnapshot['roles']): OmpModelsSnapshot => ({
    schemaVersion: '1.0',
    directory: '/repo',
    models: [
        { provider: 'prov', id: 'main', reasoning: true, thinking: { supported: ['high', 'xhigh'], defaultLevel: null } },
        { provider: 'prov', id: 'fast', reasoning: false, thinking: { supported: [], defaultLevel: null } },
        { provider: 'prov', id: 'tiny', reasoning: false, thinking: { supported: [], defaultLevel: null } },
    ],
    roles,
    roleMeta: {
        default: { tag: 'DEFAULT', name: 'Default' },
        smol: { tag: 'SMOL', name: 'Fast' },
    },
    cycleOrder: ['smol', 'default'],
    enabledModels: [],
    fallbackChains: {},
    modelRoleStorage: 'global',
    defaultThinkingLevel: 'high',
    legacyDefaults: null,
} as OmpModelsSnapshot);

const DIRECTORY = '/repo';
const RUNTIME = 'test-rt';
const revisionEnvelope = (id: number, revision: number) => ({
    id,
    type: 'omp.settings.updated',
    directory: DIRECTORY,
    createdAt: Date.now(),
    payload: { revision, keys: ['modelRoles.roles.smol'], origin: 'web' },
});

const installMinimalDom = () => {
    const descriptors = new Map<string, PropertyDescriptor | undefined>();
    const setGlobal = <T,>(name: string, value: T) => {
        descriptors.set(name, Object.getOwnPropertyDescriptor(globalThis, name));
        Object.defineProperty(globalThis, name, { configurable: true, writable: true, value });
    };
    class ElementStub {}
    const documentStub: {
        nodeType: number;
        defaultView: typeof globalThis;
        activeElement: null;
        addEventListener: () => void;
        removeEventListener: () => void;
        documentElement?: unknown;
        body?: unknown;
    } = {
        nodeType: 9,
        defaultView: globalThis,
        activeElement: null,
        addEventListener: () => undefined,
        removeEventListener: () => undefined,
    };
    const container = {
        nodeType: 1,
        tagName: 'DIV',
        nodeName: 'DIV',
        namespaceURI: 'http://www.w3.org/1999/xhtml',
        ownerDocument: documentStub,
        addEventListener: () => undefined,
        removeEventListener: () => undefined,
        dispatchEvent: () => true,
    };
    documentStub.documentElement = container;
    documentStub.body = container;
    setGlobal('window', globalThis);
    setGlobal('location', { search: '', protocol: 'http:', hostname: 'localhost' });
    setGlobal('Element', ElementStub);
    setGlobal('HTMLElement', ElementStub);
    setGlobal('HTMLIFrameElement', ElementStub);
    setGlobal('IS_REACT_ACT_ENVIRONMENT', true);
    setGlobal('requestAnimationFrame', (callback: FrameRequestCallback) => setTimeout(() => callback(Date.now()), 0));
    setGlobal('cancelAnimationFrame', clearTimeout);
    // SAFETY: the fake container implements the node surface createRoot
    // touches; `as never` marks that single hand-written stand-in hop.
    const element: Element = container as never;
    return {
        container: element,
        restore: () => {
            for (const [name, descriptor] of descriptors) {
                if (descriptor) Object.defineProperty(globalThis, name, descriptor);
                else Reflect.deleteProperty(globalThis, name);
            }
        },
    };
};

let latest: OmpModelRolesState | null = null;

const Harness = ({ directory }: { directory: string | null }) => {
    latest = useOmpModelRoles(directory);
    return null;
};

beforeEach(() => {
    getModelsCalls.length = 0;
    nextOk = true;
    nextSnapshot = snapshotWith({
        default: { configured: 'prov/main:high', provider: 'prov', id: 'main', thinkingLevel: 'high', source: 'global' },
        smol: { configured: 'prov/fast', provider: 'prov', id: 'fast', source: 'global' },
    });
    latest = null;
    useOmpSessionStore.getState().clearAll(RUNTIME);
});

describe('useOmpModelRoles live refresh', () => {
    test('a settings revision jump refetches the snapshot and updates role slots', async () => {
        const dom = installMinimalDom();
        const root: Root = createRoot(dom.container);
        await act(async () => {
            root.render(<Harness directory={DIRECTORY} />);
        });
        expect(getModelsCalls).toEqual([DIRECTORY]);
        expect(latest?.roles.find((slot) => slot.id === 'smol')?.model).toEqual({ provider: 'prov', id: 'fast' });

        // The settings page (or this picker's "Set as role") wrote a new
        // smol assignment; the engine broadcast the revision.
        nextSnapshot = snapshotWith({
            default: { configured: 'prov/main:high', provider: 'prov', id: 'main', thinkingLevel: 'high', source: 'global' },
            smol: { configured: 'prov/tiny', provider: 'prov', id: 'tiny', source: 'global' },
        });
        await act(async () => {
            useOmpSessionStore.getState().applyEvent(RUNTIME, DIRECTORY, revisionEnvelope(1, 1));
        });
        expect(getModelsCalls).toEqual([DIRECTORY, DIRECTORY]);
        expect(latest?.roles.find((slot) => slot.id === 'smol')?.model).toEqual({ provider: 'prov', id: 'tiny' });

        await act(async () => root.unmount());
        dom.restore();
    });

    test('a failed refresh keeps the previous role slots instead of blanking them', async () => {
        const dom = installMinimalDom();
        const root: Root = createRoot(dom.container);
        await act(async () => {
            root.render(<Harness directory={DIRECTORY} />);
        });
        expect(latest?.modelRolesEnabled).toBe(true);

        nextOk = false;
        await act(async () => {
            useOmpSessionStore.getState().applyEvent(RUNTIME, DIRECTORY, revisionEnvelope(2, 1));
        });
        expect(getModelsCalls).toEqual([DIRECTORY, DIRECTORY]);
        // The stale-but-authoritative snapshot stays; no legacy-picker flicker.
        expect(latest?.modelRolesEnabled).toBe(true);
        expect(latest?.roles.find((slot) => slot.id === 'smol')?.model).toEqual({ provider: 'prov', id: 'fast' });

        await act(async () => root.unmount());
        dom.restore();
    });

    test('a replayed revision (no jump) does not refetch, and a scope change refetches fresh', async () => {
        const dom = installMinimalDom();
        const root: Root = createRoot(dom.container);
        await act(async () => {
            root.render(<Harness directory={DIRECTORY} />);
        });
        await act(async () => {
            useOmpSessionStore.getState().applyEvent(RUNTIME, DIRECTORY, revisionEnvelope(3, 5));
        });
        expect(getModelsCalls).toEqual([DIRECTORY, DIRECTORY]);
        // Same revision again — the reducer no-ops, the selector value is
        // unchanged, no third fetch.
        await act(async () => {
            useOmpSessionStore.getState().applyEvent(RUNTIME, DIRECTORY, revisionEnvelope(4, 5));
        });
        expect(getModelsCalls).toEqual([DIRECTORY, DIRECTORY]);

        await act(async () => {
            root.render(<Harness directory="/other" />);
        });
        expect(getModelsCalls).toEqual([DIRECTORY, DIRECTORY, '/other']);

        await act(async () => root.unmount());
        dom.restore();
    });
});
