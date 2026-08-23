/**
 * ScheduledTaskEditorDialog persona picker — behavioral render contracts
 * (spec 02 §5.1 D-B2, 08 §5.1 GAP-02).
 *
 * Same seam-stub pattern as ompComposerSurfaces.test.tsx: the capability
 * gate (useOmpModelRoles) and persona list (useOmpPersonas) are stubbed at
 * their modules, so these tests assert the dialog's render contract —
 * which picker owns the field, and how the degraded list state renders —
 * not the hooks' fetch lifecycle (covered by ompRoleModeSurfaces).
 *
 * Contracts:
 *  - personas.v1 + authoritative list → persona Select replaces the legacy
 *    AgentSelector; "Standard" is the default entry
 *  - personas.v1 + degraded list (null) → disabled unavailable trigger,
 *    never a fake "Standard only" universe
 *  - personas.v1 off → legacy AgentSelector renders unchanged
 */
import { beforeEach, describe, expect, mock, test } from 'bun:test';
import { renderToStaticMarkup } from 'react-dom/server';
import type * as React from 'react';

import type { OmpModelRolesState } from '@/hooks/useOmpModelRoles';
import type { OmpPersonasState } from '@/hooks/useOmpPersonas';
import type { ScheduledTask } from '@/lib/scheduledTasksApi';

const DIRECTORY = '/repo';

let fakeRoles: OmpModelRolesState;
let fakePersonas: OmpPersonasState;

mock.module('@/hooks/useOmpModelRoles', () => ({
    useOmpModelRoles: () => fakeRoles,
}));

mock.module('@/hooks/useOmpPersonas', () => ({
    useOmpPersonas: () => fakePersonas,
}));

// Model-default seeding is exercised interactively; keep the async seam
// inert so a static render never depends on it settling.
mock.module('@/lib/omp-defaults', () => ({
    resolveOmpDefaults: async () => ({
        model: null,
        modelRolesEnabled: false,
        personasEnabled: false,
    }),
}));

// base-ui's Dialog never mounts its popup under a static render; stub the
// chrome as passthrough so the field contracts render (the dialog shell's
// own behavior is not under test here).
mock.module('@/components/ui/dialog', () => ({
    Dialog: (props: { children?: React.ReactNode }) => props.children ?? null,
    DialogContent: (props: { children?: React.ReactNode }) => props.children ?? null,
    DialogTitle: (props: { children?: React.ReactNode }) => props.children ?? null,
    DialogDescription: (props: { children?: React.ReactNode }) => props.children ?? null,
}));

// The dialog's ModelSelector pulls rsbuild's import.meta.glob through
// useProviderLogo; stub it so the dialog renders under plain `bun test`.
mock.module('@/hooks/useProviderLogo', () => ({
    useProviderLogo: () => null,
}));

// The legacy branch's AgentSelector gates on runtime readiness.
mock.module('@/hooks/useOpenCodeReadiness', () => ({
    useOpenCodeReadiness: () => ({
        isReady: true,
        isLoading: false,
        isUnavailable: false,
        connectionPhase: 'connected',
    }),
}));

const { ScheduledTaskEditorDialog } = await import('@/components/session/ScheduledTaskEditorDialog');
const { I18nProvider } = await import('@/lib/i18n');

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

const PERSONA_LIST: OmpPersonasState['personas'] = [
    { name: 'reviewer', systemPrompt: 'You review code.', tools: ['read'] },
];

const render = (task: ScheduledTask | null): string =>
    renderToStaticMarkup(
        <I18nProvider>
            <ScheduledTaskEditorDialog
                open
                task={task}
                projectDirectory={DIRECTORY}
                onOpenChange={() => undefined}
                onSave={async () => undefined}
            />
        </I18nProvider>,
    );

beforeEach(() => {
    fakeRoles = ROLES_OFF;
    fakePersonas = { resolved: false, personas: null };
});

describe('ScheduledTaskEditorDialog — persona picker (personas.v1)', () => {
    test('authoritative list → persona Select replaces the legacy AgentSelector', () => {
        fakeRoles = { ...ROLES_OFF, personasEnabled: true };
        fakePersonas = { resolved: true, personas: PERSONA_LIST };
        const markup = render(null);
        expect(markup).toContain('aria-label="Session persona"');
        // Standard entry is the picker's default option.
        expect(markup).toContain('Standard');
        // Legacy picker (robot-2 icon) is gone under personas.v1.
        expect(markup).not.toContain('robot-2');
    });

    test('degraded list → disabled unavailable trigger, no fake Standard-only universe', () => {
        fakeRoles = { ...ROLES_OFF, personasEnabled: true };
        fakePersonas = { resolved: true, personas: null };
        const markup = render(null);
        expect(markup).toContain('aria-label="Session persona"');
        expect(markup).toContain('Unavailable');
        expect(markup).toContain('disabled');
        expect(markup).not.toContain('robot-2');
    });

    test('capability off → legacy AgentSelector renders unchanged', () => {
        const markup = render(null);
        expect(markup).not.toContain('aria-label="Session persona"');
        expect(markup).toContain('robot-2');
    });
});
