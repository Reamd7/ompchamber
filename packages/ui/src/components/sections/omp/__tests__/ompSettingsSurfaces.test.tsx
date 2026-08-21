/**
 * omp settings surfaces — capability-gating and wiring contracts
 * (spec 06 §5.7 GAP-F3 DefaultsSettings refactor, GAP-F1 engine page,
 * 03 §5.5 C7 approvals area, 01 §5.8 GAP-11 import banner).
 *
 * Covers the degradation matrix plus the repo's established wiring source
 * assertions (issue-2903 precedent): with `settings.v1` off/unresolved the
 * legacy DefaultsSettings trio renders unchanged and the engine page/nav
 * stay hidden; with it on the roles editor (and its GAP-11 banner) renders
 * from an authoritative models snapshot.
 */
import { beforeEach, describe, expect, mock, test } from 'bun:test';
import React from 'react';
import { readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const __dirname = dirname(fileURLToPath(import.meta.url));

let featureFlags: Record<string, boolean> = {};
interface SnapshotLike {
  roles?: Record<string, { provider?: string; id?: string; thinkingLevel?: string; source?: string } | null>;
  roleMeta?: Record<string, { name?: string; hidden?: boolean }>;
}
let rolesSnapshot: SnapshotLike | null = null;

mock.module('@/lib/omp/capabilityGate', () => ({
  primeOmpCapabilityGate: async () => ({}),
  isOmpFeatureEnabled: (key: string) => featureFlags[key] === true,
  isOmpModelRolesEnabled: () => featureFlags['modelRoles.v1'] === true,
  isOmpModesEnabled: () => featureFlags['modes.v1'] === true,
  __resetOmpCapabilityGateForTests: () => undefined,
}));
// ModelSelector pulls rsbuild's import.meta.glob through useProviderLogo;
// stub it so the editor renders under plain `bun test`.
mock.module('@/components/sections/agents/ModelSelector', () => ({
  ModelSelector: (props: { providerId: string; modelId: string; placeholder?: string }) =>
    React.createElement(
      'button',
      { type: 'button', 'data-testid': 'omp-role-model-selector' },
      props.providerId ? `${props.providerId}/${props.modelId}` : (props.placeholder ?? 'No model'),
    ),
}));


const putModelRoleCalls: Array<{ role: string; value: string | null }> = [];

mock.module('@/hooks/useOmpModelRoles', () => ({
  useOmpFeatureFlags: () => ({
    resolved: true,
    modelRoles: featureFlags['modelRoles.v1'] === true,
    modes: false,
  }),
  useOmpModelRoles: () => {
    const snapshot = rolesSnapshot;
    const roles = snapshot?.roles
      ? Object.entries(snapshot.roles).flatMap(([id, entry]) => {
          if (snapshot.roleMeta?.[id]?.hidden === true) return [];
          const record = entry as { provider?: string; id?: string; thinkingLevel?: string; source?: string } | null;
          return [{
            id,
            name: snapshot.roleMeta?.[id]?.name ?? id,
            configured: record != null,
            model: record?.provider && record.id
              ? { provider: record.provider, id: record.id, ...(record.thinkingLevel ? { thinkingLevel: record.thinkingLevel } : {}) }
              : null,
            ...(record?.source ? { source: record.source } : {}),
          }];
        })
      : [];
    return {
      resolved: true,
      modelRolesEnabled: snapshot !== null,
      modesEnabled: false,
      snapshot,
      pending: false,
      roles,
      reload: () => undefined,
    };
  },
  buildRoleSlots: () => [],
  resolveSendAgent: () => undefined,
}));
// Dynamic imports follow the repo's mock.module-then-import test convention
// (packages/ui/src/lib/opencode/client.test.ts): mocks must be installed
// before the modules under test load.

mock.module('@/hooks/useRuntimeAPIs', () => ({
  useRuntimeAPIs: () => ({
    ompSettings: {
      getSettings: async () => ({ ok: false, unavailable: true }),
      putSettings: async () => ({ ok: false, unavailable: true, kind: 'error' }),
      putModelRole: async (options: { role: string; value: string | null }) => {
        putModelRoleCalls.push({ role: options.role, value: options.value });
        return { ok: true, value: options.value };
      },
    },
  }),
  useIsVSCodeRuntime: () => false,
}));

mock.module('@/hooks/useEffectiveDirectory', () => ({
  useEffectiveDirectory: () => '/repo',
}));

const { I18nProvider } = await import('@/lib/i18n');
const { OmpModelRolesEditor } = await import('../OmpModelRolesEditor');

const SNAPSHOT_WITH_LEGACY = {
  schemaVersion: '1.0',
  directory: '/repo',
  roles: {
    default: null,
    smol: { configured: 'prov/fast', provider: 'prov', id: 'fast', source: 'global' },
  },
  roleMeta: { default: { name: 'Default', tag: 'DEFAULT' }, smol: { name: 'Fast', tag: 'SMOL' } },
  cycleOrder: ['default', 'smol'],
  enabledModels: [],
  fallbackChains: {},
  modelRoleStorage: 'global',
  defaultThinkingLevel: 'high',
  legacyDefaults: { defaultModel: 'anthropic/claude-legacy' },
};

const SNAPSHOT_CONFIGURED = {
  ...SNAPSHOT_WITH_LEGACY,
  roles: {
    default: { configured: 'openai/gpt-5', provider: 'openai', id: 'gpt-5', source: 'project' },
    smol: { configured: 'prov/fast', provider: 'prov', id: 'fast', source: 'global' },
  },
  legacyDefaults: { defaultModel: 'anthropic/claude-legacy' },
};

const renderEditor = (): React.ReactElement =>
  React.createElement(
    I18nProvider,
    null,
    React.createElement(OmpModelRolesEditor, { directory: '/repo' }),
  );

// react-dom/server render via the renderer the repo's other component tests use
const { renderToStaticMarkup } = await import('react-dom/server');

describe('OmpModelRolesEditor (GAP-F3 roles editor + GAP-11 banner)', () => {
  beforeEach(() => {
    featureFlags = {};
    rolesSnapshot = null;
    putModelRoleCalls.length = 0;
  });

  test('unconfigured default + legacy value → import banner with explicit action', () => {
    rolesSnapshot = SNAPSHOT_WITH_LEGACY;
    const markup = renderToStaticMarkup(renderEditor());
    expect(markup).toContain('Legacy default model detected');
    expect(markup).toContain('anthropic/claude-legacy');
    expect(markup).toContain('Import');
    // Source badges render for configured roles.
    expect(markup).toContain('Global');
    expect(markup).toContain('Fast');
  });

  test('configured default role → side-by-side comparison only, no import action', () => {
    rolesSnapshot = SNAPSHOT_CONFIGURED;
    const markup = renderToStaticMarkup(renderEditor());
    expect(markup).toContain('anthropic/claude-legacy');
    expect(markup).toContain('openai/gpt-5');
    expect(markup).not.toContain('Legacy default model detected');
    expect(markup).not.toContain('>Import<');
    // Project-sourced assignment carries its badge.
    expect(markup).toContain('Project');
  });

  test('no authoritative snapshot → quiet unavailable note, never a partial roles surface', () => {
    rolesSnapshot = null;
    const markup = renderToStaticMarkup(renderEditor());
    expect(markup).toContain('Model roles are unavailable right now.');
    expect(markup).not.toContain('Legacy default model detected');
  });
});

describe('settings surface wiring follows the settings.v1 gate (source contracts)', () => {
  test('DefaultsSettings swaps the trio for the roles editor only under the capability', () => {
    const source = readFileSync(join(__dirname, '..', '..', 'openchamber', 'DefaultsSettings.tsx'), 'utf8');
    expect(source).toContain("useOmpFeatureEnabled('settings.v1')");
    expect(source).toContain('{ompEngineEnabled ? (');
    expect(source).toContain('<OmpModelRolesEditor directory={ompDirectory} />');
    // The legacy trio (and its dual-channel write path) survives verbatim for
    // the capability-off branch.
    expect(source).toContain("settingsItem=\"sessions.default-model\"");
    expect(source).toContain('handleModelChange');
    expect(source).toContain("runtimeFetch('/api/config/settings'");
  });

  test('Engine page registers in nav, order, mobile whitelist, and search', () => {
    const metadata = readFileSync(join(__dirname, '..', '..', '..', '..', 'lib', 'settings', 'metadata.ts'), 'utf8');
    expect(metadata).toContain("| 'engine'");
    expect(metadata).toContain("slug: 'engine'");
    expect(metadata).toContain("case 'engine':");

    const view = readFileSync(join(__dirname, '..', '..', '..', '..', 'components', 'views', 'SettingsView.tsx'), 'utf8');
    expect(view).toContain("  'engine',");
    expect(view).toContain("case 'engine':");
    expect(view).toContain("return <OmpEngineSettingsPage />;");
    expect(view).toContain("page.slug !== 'engine' || ompEngineSettingsEnabled");

    const mobile = readFileSync(join(__dirname, '..', '..', '..', '..', 'apps', 'MobileApp.tsx'), 'utf8');
    expect(mobile).toContain("  'engine',");

    const search = readFileSync(join(__dirname, '..', '..', '..', '..', 'lib', 'settings', 'search.ts'), 'utf8');
    expect(search).toContain("id: 'engine.model-roles'");
    expect(search).toContain("id: 'engine.approvals'");
  });

  test('Engine page owns approvals + roles keys; C7 deep-link lands on the roles page', () => {
    const page = readFileSync(join(__dirname, '..', 'OmpEngineSettingsPage.tsx'), 'utf8');
    expect(page).toContain("key === 'tools.approvalMode'");
    expect(page).toContain("key === 'tools.approval'");
    expect(page).toContain("key === 'bash.patterns'");
    expect(page).toContain("key.startsWith('ask.')");
    // The approvals section hides itself when the schema exposes no keys.
    expect(page).toContain('approvalEntries.length > 0 &&');
    // Roles-owned keys are never duplicated in the generic tab dump.
    expect(page).toContain('modelRoleStorage: true');
    // Credential entries render masked and never echo values.
    expect(page).toContain("type={credential ? 'password' : 'text'}");
    expect(page).toContain('configured');

    const controls = readFileSync(join(__dirname, '..', '..', '..', 'chat', 'ModelControls.tsx'), 'utf8');
    // The roles deep-link block targets the engine page (the add-provider
    // flow keeps its own providers navigation, so scope the check).
    const openRolesBlock = controls.slice(controls.indexOf('const openRolesSettings'), controls.indexOf('const openRolesSettings') + 400);
    expect(openRolesBlock).toContain("setSettingsPage('engine')");
    expect(openRolesBlock).not.toContain("setSettingsPage('providers')");
  });
});
