/**
 * useOmpSessionModelSwitch — write-path contracts (spec 01 GAP-02/GAP-04).
 *
 * Under the omp model-roles capability the picker must switch the session's
 * model through POST /api/omp/sessions/{id}/model (the /switch equivalent).
 * These tests pin the gate, the request arguments, the stale-completion
 * guard, and failure rollback to the authoritative badge model.
 */
import { act } from 'react';
import { createRoot, type Root } from 'react-dom/client';
import { beforeEach, describe, expect, mock, test } from 'bun:test';

let gateEnabled = false;
const toastErrorCalls: string[] = [];

mock.module('@/lib/omp/capabilityGate', () => ({
    isOmpFeatureEnabled: () => gateEnabled,
    isOmpModelRolesEnabled: () => gateEnabled,
    isOmpModesEnabled: () => false,
    isOmpPersonasEnabled: () => false,
    isOmpAgentDefinitionsEnabled: () => false,
    primeOmpCapabilityGate: async () => ({ capabilities: null }),
    __resetOmpCapabilityGateForTests: () => undefined,
}));

mock.module('sonner', () => ({
    toast: {
        error: (message: string) => {
            toastErrorCalls.push(message);
        },
    },
}));

type SetSessionModelCall = {
    sessionID: string;
    model: { providerID: string; modelID: string };
    options: { directory: string; thinkingLevel?: string };
};

type SetSessionModelResult =
    | { ok: true; model: string }
    | { ok: false; unavailable: boolean; error?: string };

const deferredResult = () => {
    let resolve!: (value: SetSessionModelResult) => void;
    const promise = new Promise<SetSessionModelResult>((next) => {
        resolve = next;
    });
    return { promise, resolve };
};

const calls: SetSessionModelCall[] = [];
const pendingResults: Array<{ promise: Promise<SetSessionModelResult>; resolve: (value: SetSessionModelResult) => void }> = [];
const localApplies: Array<{ providerId: string; modelId: string }> = [];

const ompModels = {
    setSessionModel: mock((sessionID: string, model: { providerID: string; modelID: string }, options: { directory: string; thinkingLevel?: string }) => {
        calls.push({ sessionID, model, options });
        const deferred = deferredResult();
        pendingResults.push(deferred);
        return deferred.promise;
    }),
};

const { useOmpSessionModelSwitch } = await import('@/components/chat/useOmpSessionModelSwitch');
const { RuntimeAPIContext } = await import('@/contexts/runtimeAPIContext');

const installMinimalDom = () => {
    const descriptors = new Map<string, PropertyDescriptor | undefined>();
    const setGlobal = <T,>(name: string, value: T) => {
        descriptors.set(name, Object.getOwnPropertyDescriptor(globalThis, name));
        Object.defineProperty(globalThis, name, { configurable: true, writable: true, value });
    };
    class ElementStub {}
    interface FakeContainer {
        nodeType: number;
        tagName: string;
        nodeName: string;
        namespaceURI: string;
        ownerDocument: FakeDocument;
        addEventListener: () => void;
        removeEventListener: () => void;
    }
    interface FakeDocument {
        nodeType: number;
        defaultView: unknown;
        activeElement: null;
        addEventListener: () => void;
        removeEventListener: () => void;
        documentElement?: FakeContainer;
        body?: FakeContainer;
    }
    const documentStub: FakeDocument = {
        nodeType: 9,
        defaultView: globalThis,
        activeElement: null,
        addEventListener: () => undefined,
        removeEventListener: () => undefined,
    };
    const container: FakeContainer = {
        nodeType: 1,
        tagName: 'DIV',
        nodeName: 'DIV',
        namespaceURI: 'http://www.w3.org/1999/xhtml',
        ownerDocument: documentStub,
        addEventListener: () => undefined,
        removeEventListener: () => undefined,
    };
    documentStub.documentElement = container;
    documentStub.body = container;
    setGlobal('document', documentStub);
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

interface HarnessOptions {
    sessionID: string | null;
    directory: string | null;
    authoritativeModel: { provider: string; id: string } | null;
}
let currentSwitch: ((providerId: string, modelId: string, thinkingLevel?: string) => void) | null = null;
const renderHarness = (root: Root, options: HarnessOptions) => {
    const Harness = () => {
        const hook = useOmpSessionModelSwitch({
            sessionID: options.sessionID,
            directory: options.directory,
            authoritativeModel: options.authoritativeModel,
            applyLocalModel: (providerId: string, modelId: string) => {
                localApplies.push({ providerId, modelId });
            },
            changeFailedLabel: 'switch failed label',
        });
        currentSwitch = hook.switchSessionModel;
        return null;
    };
    root.render(
        // SAFETY: the harness registers only the ompModels slice the hook
        // reads; the remaining RuntimeAPIs members are never touched here.
        <RuntimeAPIContext.Provider value={{ ompModels } as never}>
            <Harness />
        </RuntimeAPIContext.Provider>,
    );
};

const captureSwitch = () => {
    if (!currentSwitch) {
        throw new Error('switchSessionModel was not captured from the harness render');
    }
    return currentSwitch;
};

beforeEach(() => {
    gateEnabled = false;
    toastErrorCalls.length = 0;
    calls.length = 0;
    pendingResults.length = 0;
    localApplies.length = 0;
});

describe('useOmpSessionModelSwitch', () => {
    test('switches the session model server-side when the omp gate is on', async () => {
        const dom = installMinimalDom();
        const root: Root = createRoot(dom.container);
        gateEnabled = true;
        await act(async () => {
            renderHarness(root, {
                sessionID: 'ses_1',
                directory: '/repo',
                authoritativeModel: { provider: 'prov', id: 'main' },
            });
        });
        const switchModel = captureSwitch();

        await act(async () => {
            switchModel('prov', 'next');
        });
        expect(calls).toEqual([{
            sessionID: 'ses_1',
            model: { providerID: 'prov', modelID: 'next' },
            options: { directory: '/repo' },
        }]);

        // An explicit thinking level rides the switch; without one the
        // option stays absent (never 'inherit' — undefined is the engine's
        // keep-current contract).
        await act(async () => {
            switchModel('prov', 'other', 'xhigh');
        });
        expect(calls[1]).toEqual({
            sessionID: 'ses_1',
            model: { providerID: 'prov', modelID: 'other' },
            options: { directory: '/repo', thinkingLevel: 'xhigh' },
        });
        await act(async () => {
            pendingResults[1].resolve({ ok: true, model: 'prov/other' });
        });

        await act(async () => {
            pendingResults[0].resolve({ ok: true, model: 'prov/next' });
        });
        expect(toastErrorCalls).toEqual([]);
        expect(localApplies).toEqual([]);
        await act(async () => root.unmount());
        dom.restore();
    });

    test('does not write when the gate is off, the session is absent, or the directory is unknown', async () => {
        const dom = installMinimalDom();
        const root: Root = createRoot(dom.container);
        await act(async () => {
            renderHarness(root, {
                sessionID: 'ses_1',
                directory: '/repo',
                authoritativeModel: { provider: 'prov', id: 'main' },
            });
        });
        const switchModel = captureSwitch();

        // Gate off: legacy runtimes keep the prompt-time model path.
        await act(async () => {
            switchModel('prov', 'next');
        });
        expect(calls).toEqual([]);

        gateEnabled = true;
        const remount = (options: { sessionID: string | null; directory: string | null }) =>
            renderHarness(root, { ...options, authoritativeModel: { provider: 'prov', id: 'main' } });
        await act(async () => {
            remount({ sessionID: null, directory: '/repo' });
        });
        await act(async () => {
            captureSwitch()('prov', 'next');
        });
        await act(async () => {
            remount({ sessionID: 'ses_1', directory: null });
        });
        await act(async () => {
            captureSwitch()('prov', 'next');
        });
        expect(calls).toEqual([]);
        await act(async () => root.unmount());
        dom.restore();
    });

    test('a failed switch toasts and rolls the local selection back to the authoritative model', async () => {
        const dom = installMinimalDom();
        const root: Root = createRoot(dom.container);
        gateEnabled = true;
        await act(async () => {
            renderHarness(root, {
                sessionID: 'ses_1',
                directory: '/repo',
                authoritativeModel: { provider: 'prov', id: 'main' },
            });
        });
        const switchModel = captureSwitch();

        await act(async () => {
            switchModel('prov', 'next');
        });
        await act(async () => {
            pendingResults[0].resolve({ ok: false, unavailable: false, error: 'no api key' });
        });
        expect(toastErrorCalls).toEqual(['switch failed label']);
        expect(localApplies).toEqual([{ providerId: 'prov', modelId: 'main' }]);
        await act(async () => root.unmount());
        dom.restore();
    });

    test('an unavailable endpoint stays silent but still rolls the optimistic selection back', async () => {
        const dom = installMinimalDom();
        const root: Root = createRoot(dom.container);
        gateEnabled = true;
        await act(async () => {
            renderHarness(root, {
                sessionID: 'ses_1',
                directory: '/repo',
                authoritativeModel: { provider: 'prov', id: 'main' },
            });
        });
        const switchModel = captureSwitch();

        await act(async () => {
            switchModel('prov', 'next');
        });
        await act(async () => {
            pendingResults[0].resolve({ ok: false, unavailable: true });
        });
        expect(toastErrorCalls).toEqual([]);
        expect(localApplies).toEqual([{ providerId: 'prov', modelId: 'main' }]);
        await act(async () => root.unmount());
        dom.restore();
    });

    test('a stale completion never toasts or rolls back after a newer switch started', async () => {
        const dom = installMinimalDom();
        const root: Root = createRoot(dom.container);
        gateEnabled = true;
        await act(async () => {
            renderHarness(root, {
                sessionID: 'ses_1',
                directory: '/repo',
                authoritativeModel: { provider: 'prov', id: 'main' },
            });
        });
        const switchModel = captureSwitch();

        await act(async () => {
            switchModel('prov', 'b');
        });
        await act(async () => {
            switchModel('prov', 'c');
        });
        // The older switch (to b) settles after the newer one (to c) started.
        await act(async () => {
            pendingResults[0].resolve({ ok: false, unavailable: false });
        });
        expect(toastErrorCalls).toEqual([]);
        expect(localApplies).toEqual([]);
        // The newer switch still owns its own outcome.
        await act(async () => {
            pendingResults[1].resolve({ ok: true, model: 'prov/c' });
        });
        expect(toastErrorCalls).toEqual([]);
        expect(localApplies).toEqual([]);
        await act(async () => root.unmount());
        dom.restore();
    });
});
