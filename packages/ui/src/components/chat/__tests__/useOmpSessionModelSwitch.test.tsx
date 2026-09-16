/**
 * useOmpSessionModelSwitch — persistence and failure contracts.
 *
 * The hook owns the composer picker's server-side model switch. Three
 * contracts matter for the "chip shows a model the session never ran" bug:
 *
 * 1. A successful switch confirms the requested pair — the embedder's
 *    per-session selection cache may only hold server-confirmed values.
 * 2. A failed switch rolls the local selection back to the authoritative
 *    model and confirms THAT, so a rejected pick cannot linger as the
 *    session's stored selection. `unavailable` failures stay silent.
 * 3. A session whose directory cannot be resolved is a loud failure (toast
 *    + rollback), never the old silent local-only no-op; a draft (no
 *    session) and a legacy runtime (feature off) stay silent local picks.
 *
 * Rendered via happy-dom + createRoot/act (Bun provides no DOM). The
 * capability gate stays REAL — mocking its module leaks into
 * ompRoleModeSurfaces' own gate tests — so the runtime APIs registry is
 * seeded and the gate primed per test.
 */
import { afterEach, beforeEach, describe, expect, mock, test } from 'bun:test';
import { Window } from 'happy-dom';
import React, { act } from 'react';
import { createRoot, type Root } from 'react-dom/client';

import type { useOmpSessionModelSwitch as HookType, UseOmpSessionModelSwitchArgs } from '../useOmpSessionModelSwitch';
import type { RuntimeAPIs } from '@/lib/api/types';

type Switch = ReturnType<typeof HookType>['switchSessionModel'];
type SetSessionModelFn = (sessionID: string, model: { providerID: string; modelID: string }, options: { directory: string }) => Promise<{ ok: boolean; model: string; unavailable?: boolean }>;

interface ModelPair {
    providerId: string;
    modelId: string;
}

const setSessionModel = mock<SetSessionModelFn>(async () => ({ ok: true, model: 'apifox/grok-4.6' }));
let setSessionModelCalls = 0;
const confirmed: ModelPair[] = [];
const appliedLocally: ModelPair[] = [];
let toastErrors = 0;

mock.module('sonner', () => ({
    toast: {
        dismiss: () => undefined,
        error: () => {
            toastErrors += 1;
        },
        info: () => undefined,
        success: () => undefined,
    },
}));

const { registerRuntimeAPIs } = await import('@/contexts/runtimeAPIRegistry');
const { __resetOmpCapabilityGateForTests, primeOmpCapabilityGate } = await import('@/lib/omp/capabilityGate');
const { RuntimeAPIContext } = await import('@/contexts/runtimeAPIContext');
const { useOmpSessionModelSwitch } = await import('../useOmpSessionModelSwitch');

interface ProbeHandle {
    switchModel: Switch;
    pending: { provider: string; id: string } | null;
}

const Probe = ({ handle, args }: { handle: ProbeHandle; args: UseOmpSessionModelSwitchArgs }) => {
    const { switchSessionModel, pendingModel } = useOmpSessionModelSwitch(args);
    handle.switchModel = switchSessionModel;
    handle.pending = pendingModel;
    return null;
};

describe('useOmpSessionModelSwitch', () => {
    let windowInstance: Window;
    let root: Root;

    const baseArgs = () => ({
        sessionID: 'ses_1' as string | null,
        directory: '/repo' as string | null,
        authoritativeModel: { provider: 'apifox', id: 'deepseek-flash' } as { provider: string; id: string } | null,
        applyLocalModel: (providerId: string, modelId: string) => {
            appliedLocally.push({ providerId, modelId });
        },
        changeFailedLabel: 'Could not change the model',
        onConfirmedModel: (providerId: string, modelId: string) => {
            confirmed.push({ providerId, modelId });
        },
    });

    const render = async (args: ReturnType<typeof baseArgs>) => {
        const handle: ProbeHandle = { switchModel: () => undefined, pending: null };
        // SAFETY: RuntimeAPIs test fixture narrowing — only ompModels feeds the hook under test.
        const apis = { ompModels: { setSessionModel } } as unknown as RuntimeAPIs;
        await act(async () => {
            root.render(
                <RuntimeAPIContext.Provider value={apis}>
                    <Probe handle={handle} args={args} />
                </RuntimeAPIContext.Provider>,
            );
        });
        return handle;
    };

    const pick = (handle: ProbeHandle) => act(async () => {
        handle.switchModel('apifox', 'grok-4.6');
    });

    beforeEach(async () => {
        windowInstance = new Window();
        Object.assign(globalThis, {
            document: windowInstance.document,
            HTMLElement: windowInstance.HTMLElement,
            Element: windowInstance.Element,
            Node: windowInstance.Node,
            IS_REACT_ACT_ENVIRONMENT: true,
        });
        // SAFETY: happy-dom's Window satisfies React DOM's window reads; the lib types don't unify.
        globalThis.window = windowInstance as unknown as typeof globalThis.window;
        root = createRoot(document.createElement('div'));
        setSessionModel.mockReset();
        setSessionModel.mockImplementation(async () => {
            setSessionModelCalls += 1;
            return { ok: true, model: 'apifox/grok-4.6' };
        });
        confirmed.length = 0;
        appliedLocally.length = 0;
        toastErrors = 0;
        setSessionModelCalls = 0;
        // Real gate, real runtime key: the probe settles modelRoles.v1 so
        // the hook takes the switch path under test.
        // SAFETY: RuntimeAPIs test fixture narrowing — only ompCapabilities participates in the gate probe.
        registerRuntimeAPIs({
            ompCapabilities: {
                getCapabilities: async () => ({ version: 1, eventSchema: '1.0', features: { 'modelRoles.v1': true }, minUiVersion: '0.0.0' }),
            },
        } as unknown as Parameters<typeof registerRuntimeAPIs>[0]);
        __resetOmpCapabilityGateForTests();
        await primeOmpCapabilityGate();
    });

    afterEach(async () => {
        await act(async () => {
            root.unmount();
        });
        // Every happy-dom global assigned in beforeEach must go: a leaked
        // `window` makes real store modules (loaded lazily by the next test
        // file sharing this process) take the browser path and crash on
        // bare `localStorage`.
        const globals = globalThis as Record<string, unknown>;
        for (const key of ['document', 'window', 'HTMLElement', 'Element', 'Node', 'IS_REACT_ACT_ENVIRONMENT']) {
            delete globals[key];
        }
        registerRuntimeAPIs(null);
        __resetOmpCapabilityGateForTests();
    });

    test('a successful switch confirms the requested pair and clears pending', async () => {
        const handle = await render(baseArgs());
        await pick(handle);
        expect(handle.pending).toBeNull();
        expect(confirmed).toEqual([{ providerId: 'apifox', modelId: 'grok-4.6' }]);
        expect(toastErrors).toBe(0);
        expect(appliedLocally).toEqual([]);
    });

    test('a failed switch rolls back, confirms the authoritative model, and toasts', async () => {
        setSessionModel.mockImplementation(async () => {
            setSessionModelCalls += 1;
            return { ok: false, model: '' };
        });
        const handle = await render(baseArgs());
        await pick(handle);
        expect(handle.pending).toBeNull();
        expect(appliedLocally).toEqual([{ providerId: 'apifox', modelId: 'deepseek-flash' }]);
        expect(confirmed).toEqual([{ providerId: 'apifox', modelId: 'deepseek-flash' }]);
        expect(toastErrors).toBe(1);
    });

    test('an unavailable failure rolls back without a toast', async () => {
        setSessionModel.mockImplementation(async () => {
            setSessionModelCalls += 1;
            return { ok: false, model: '', unavailable: true };
        });
        const handle = await render(baseArgs());
        await pick(handle);
        expect(appliedLocally).toEqual([{ providerId: 'apifox', modelId: 'deepseek-flash' }]);
        expect(confirmed).toEqual([{ providerId: 'apifox', modelId: 'deepseek-flash' }]);
        expect(toastErrors).toBe(0);
    });

    test('a failure rolls back to the latest authoritative model, not the mount-time value', async () => {
        setSessionModel.mockImplementation(async () => {
            setSessionModelCalls += 1;
            return { ok: false, model: '' };
        });
        await render(baseArgs());
        // The badge reports a newer model after mount (a prior switch or a
        // warm wire record): settle-time inputs must track the latest props,
        // not the values captured when the hook first rendered.
        const handle = await render({
            ...baseArgs(),
            authoritativeModel: { provider: 'apifox', id: 'kimi-k3' },
        });
        await pick(handle);
        expect(appliedLocally).toEqual([{ providerId: 'apifox', modelId: 'kimi-k3' }]);
        expect(confirmed).toEqual([{ providerId: 'apifox', modelId: 'kimi-k3' }]);
    });

    test('a failed switch with no authoritative model leaves local state untouched and confirms nothing', async () => {
        setSessionModel.mockImplementation(async () => ({ ok: false, model: '' }));
        const handle = await render({ ...baseArgs(), authoritativeModel: null });
        await pick(handle);
        expect(toastErrors).toBe(1);
        expect(appliedLocally).toEqual([]);
        expect(confirmed).toEqual([]);
    });

    test('a session without a resolvable directory fails loudly instead of silently skipping', async () => {
        const args = { ...baseArgs(), directory: null };
        const handle = await render(args);
        await pick(handle);
        expect(setSessionModelCalls).toBe(0);
        expect(toastErrors).toBe(1);
        expect(appliedLocally).toEqual([{ providerId: 'apifox', modelId: 'deepseek-flash' }]);
        expect(confirmed).toEqual([{ providerId: 'apifox', modelId: 'deepseek-flash' }]);
    });

    test('a draft (no session) stays a silent local pick', async () => {
        const args = { ...baseArgs(), sessionID: null };
        const handle = await render(args);
        await pick(handle);
        expect(setSessionModelCalls).toBe(0);
        expect(toastErrors).toBe(0);
        expect(confirmed).toEqual([]);
    });
});
