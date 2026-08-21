// domain-models tests: per-directory keyed Settings topology (06 §5.1 REVISED
// R2 / R6), /omp/models payload shape (01 §5.3), credential sanitization
// (R9), project-scope write limits (R6), legacy defaultModel import
// (R12), and revision/event semantics. Uses the real SDK Settings class in
// throwaway agent directories so cloneForCwd's storage-sharing and
// project-layer reload are exercised against actual persistence.

import { describe, test, expect, afterAll } from 'bun:test';
import { mkdtempSync, mkdirSync, writeFileSync, readFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { Settings, VERSION } from '@oh-my-pi/pi-coding-agent';
import {
  SETTING_TABS,
} from '@oh-my-pi/pi-coding-agent/config/settings';
import {
  createSettingsStore,
  buildModelsPayload,
  buildSettingsPayload,
  applySettingsChanges,
  detectLegacyDefaultModel,
  importLegacyDefaultModel,
  registerModelSettingsRoutes,
} from './domain-models.js';

const cleanupDirs = [];
const makeDir = () => {
  const dir = mkdtempSync(path.join(tmpdir(), 'omp-domain-models-'));
  cleanupDirs.push(dir);
  return dir;
};

/** Write `<dir>/.omp/config.yml` with a modelRoles subtree. */
const writeProjectConfig = (dir, roles) => {
  mkdirSync(path.join(dir, '.omp'), { recursive: true });
  const body = Object.entries(roles)
    .map(([role, value]) => `  ${role}: ${value}`)
    .join('\n');
  writeFileSync(path.join(dir, '.omp', 'config.yml'), `modelRoles:\n${body}\n`);
};

const readProjectConfig = (dir) => {
  try {
    return readFileSync(path.join(dir, '.omp', 'config.yml'), 'utf8');
  } catch {
    return null;
  }
};

const readGlobalConfig = (agentDir) => {
  try {
    return readFileSync(path.join(agentDir, 'config.yml'), 'utf8');
  } catch {
    return null;
  }
};

/** Fresh env: isolated agentDir + boot Settings (loadIsolated — no process
 * singleton pollution) + store. Project layers written before boot load. */
const makeEnv = async ({ projectA = null, projectB = null } = {}) => {
  const agentDir = makeDir();
  const dirA = makeDir();
  const dirB = makeDir();
  if (projectA) writeProjectConfig(dirA, projectA);
  if (projectB) writeProjectConfig(dirB, projectB);
  const boot = await Settings.loadIsolated({ cwd: dirA, agentDir });
  const store = await createSettingsStore(boot);
  return { agentDir, dirA, dirB, boot, store };
};

const disarm = async (env) => {
  await env.store.disposeAll();
  env.boot.cancelPendingSaves();
};

afterAll(async () => {
  for (const dir of cleanupDirs) {
    try {
      rmSync(dir, { recursive: true, force: true, maxRetries: 5 });
    } catch {
      // best-effort teardown on Windows file locks
    }
  }
});

describe('createSettingsStore: per-directory keyed instances (06 §5.1 R2, R6)', () => {
  test('boot directory resolves to the boot instance; clones share storage but reload the project layer', async () => {
    const env = await makeEnv({
      projectA: { default: 'prov/a-model' },
      projectB: { default: 'prov/b-model' },
    });
    const { boot, store, dirA, dirB } = env;
    try {
      expect(await store.settingsFor(dirA)).toBe(boot);
      expect(await store.settingsFor()).toBe(boot);
      expect(boot.getModelRole('default')).toBe('prov/a-model');

      const b = await store.settingsFor(dirB);
      expect(b).not.toBe(boot);
      // Shares storage + configPath with boot (cloneForCwd, settings.ts:607-625).
      expect(b.getStorage()).toBe(boot.getStorage());
      // But consumes dirB's project layer, not boot's.
      expect(b.getProjectModelRole('default')).toBe('prov/b-model');
      expect(b.getCwd()).toBe(path.normalize(dirB));
      // Boot is untouched by the derivation.
      expect(boot.getProjectModelRole('default')).toBe('prov/a-model');

      // Same keyed instance for repeat visits.
      expect(await store.settingsFor(dirB)).toBe(b);
    } finally {
      await disarm(env);
    }
  });

  test('disposeAll drops clones; re-derivation reloads the project layer from disk', async () => {
    const env = await makeEnv({ projectB: { default: 'prov/b1' } });
    const { store, dirB } = env;
    try {
      const first = await store.settingsFor(dirB);
      expect(first.getProjectModelRole('default')).toBe('prov/b1');

      writeProjectConfig(dirB, { default: 'prov/b1', smol: 'prov/b2-smol' });
      await store.disposeAll();
      const second = await store.settingsFor(dirB);
      expect(second).not.toBe(first);
      expect(second.getProjectModelRole('default')).toBe('prov/b1');
      expect(second.getProjectModelRole('smol')).toBe('prov/b2-smol');
    } finally {
      await disarm(env);
    }
  });
});

describe('buildModelsPayload (01 §5.3(1))', () => {
  test('roles snapshot, cycleOrder, enabledModels, fallbackChains, legacyDefaults', async () => {
    const env = await makeEnv();
    const { boot, store } = env;
    try {
      boot.setModelRole('default', 'prov/m1:high');
      boot.setModelRole('smol', 'prov/small');
      boot.set('cycleOrder', ['smol', 'default', 'slow', 'custom1']);
      boot.set('enabledModels', ['prov/m1']);
      boot.set('retry.fallbackChains', { default: ['prov/f1'] });
      boot.set('modelTags', { custom1: { name: 'Custom One' } });
      await boot.flush();

      const legacy = { defaultModel: 'oc/legacy', defaultProvider: 'oc' };
      const payload = buildModelsPayload(boot, { legacyDefaults: legacy });
      expect(payload.schemaVersion).toBe(VERSION);
      expect(payload.directory).toBe(boot.getCwd());
      expect(payload.roles.default).toEqual({
        configured: 'prov/m1:high',
        provider: 'prov',
        id: 'm1',
        thinkingLevel: 'high',
        source: 'global',
      });
      expect(payload.roles.smol).toEqual({
        configured: 'prov/small',
        provider: 'prov',
        id: 'small',
        source: 'global',
      });
      // Unconfigured built-in and custom roles report null.
      expect(payload.roles.vision).toBeNull();
      expect(payload.roles.custom1).toBeNull();
      // Custom role surfaced via modelTags (getKnownRoleIds).
      expect(payload.roleMeta.custom1.name).toBe('Custom One');
      expect(payload.roleMeta.default.tag).toBe('DEFAULT');
      expect(payload.cycleOrder).toEqual(['smol', 'default', 'slow', 'custom1']);
      expect(payload.enabledModels).toEqual(['prov/m1']);
      // Default chain expands to every configured role (retry-fallback-chains).
      expect(payload.fallbackChains.default).toEqual(['prov/f1']);
      expect(payload.fallbackChains.smol).toEqual(['prov/f1']);
      expect(payload.modelRoleStorage).toBe('global');
      expect(payload.defaultThinkingLevel).toBe('high');
      expect(payload.legacyDefaults).toBe(legacy);
    } finally {
      await disarm(env);
    }
  });

  test('models[] projection carries the baked thinking surface (01 §5.4 GAP-06)', async () => {
    const env = await makeEnv();
    const { boot } = env;
    try {
      const models = [
        {
          provider: 'prov',
          id: 'm1',
          name: 'Model One',
          reasoning: true,
          contextWindow: 200000,
          maxTokens: 32000,
          thinking: { efforts: ['low', 'high'], defaultLevel: 'high' },
        },
        { provider: 'prov', id: 'plain' },
      ];
      const payload = buildModelsPayload(boot, { models });
      expect(payload.models).toHaveLength(2);
      expect(payload.models[0]).toEqual({
        provider: 'prov',
        id: 'm1',
        name: 'Model One',
        reasoning: true,
        contextWindow: 200000,
        maxTokens: 32000,
        thinking: { supported: ['low', 'high'], defaultLevel: 'high' },
      });
      // Non-reasoning models expose an empty effort surface, mirroring
      // the SDK's getSupportedEfforts.
      expect(payload.models[1].thinking).toEqual({ supported: [], defaultLevel: null });
      expect(payload.models[1].reasoning).toBe(false);
    } finally {
      await disarm(env);
    }
  });

  test('models omitted when the registry is unavailable (roles-only payload)', async () => {
    const env = await makeEnv();
    const { boot } = env;
    try {
      const payload = buildModelsPayload(boot);
      expect('models' in payload).toBe(false);
    } finally {
      await disarm(env);
    }
  });

  test('no legacyDefaults leaves the field null', async () => {
    const env = await makeEnv();
    try {
      expect(buildModelsPayload(env.boot).legacyDefaults).toBeNull();
    } finally {
      await disarm(env);
    }
  });
});

describe('credential sanitization (R9: GET + PUT echo)', () => {
  test('PUT writes a credential and echoes only { configured: true }', async () => {
    const env = await makeEnv();
    const { store } = env;
    try {
      const result = await applySettingsChanges(store, {
        scope: 'global',
        changes: { 'hindsight.apiToken': 'super-secret-token-xyz' },
      });
      expect(result.status).toBe(200);
      expect(result.body.applied['hindsight.apiToken']).toEqual({ configured: true });
      expect(JSON.stringify(result.body)).not.toContain('super-secret-token-xyz');
    } finally {
      await disarm(env);
    }
  });

  test('GET masks value and default; configured flag reflects state; clearing echoes { configured: false }', async () => {
    const env = await makeEnv();
    const { store, boot } = env;
    try {
      await applySettingsChanges(store, {
        changes: { 'hindsight.apiToken': 'super-secret-token-xyz', 'mnemopi.llmApiKey': 'another-secret-abc' },
      });
      const payload = buildSettingsPayload(boot, { revision: store.getRevision() });
      for (const key of ['hindsight.apiToken', 'mnemopi.llmApiKey']) {
        expect(payload.keys[key].value).toBeNull();
        expect(payload.keys[key].default).toBeNull();
        expect(payload.keys[key].configured).toBe(true);
        expect(payload.keys[key].credential).toBe(true);
        expect(payload.keys[key].writeOnly).toBe(true);
      }
      expect(JSON.stringify(payload)).not.toContain('super-secret-token-xyz');
      expect(JSON.stringify(payload)).not.toContain('another-secret-abc');

      const cleared = await applySettingsChanges(store, {
        changes: { 'hindsight.apiToken': null },
      });
      expect(cleared.status).toBe(200);
      expect(cleared.body.applied['hindsight.apiToken']).toEqual({ configured: false });
      expect(JSON.stringify(cleared.body)).not.toContain('super-secret-token-xyz');
      expect(buildSettingsPayload(boot).keys['hindsight.apiToken'].configured).toBe(false);
    } finally {
      await disarm(env);
    }
  });
});

describe('PUT /omp/settings write routing (06 §5.3, R6)', () => {
  test('project scope accepts only modelRoles.<role> keys', async () => {
    const env = await makeEnv();
    const { store, dirB } = env;
    try {
      const rejected = await applySettingsChanges(store, {
        directory: dirB,
        scope: 'project',
        changes: {
          'compaction.strategy': 'handoff',
          'todo.reminders': true,
        },
      });
      expect(rejected.status).toBe(400);
      expect(rejected.body.error).toBe('validation');
      expect(rejected.body.rejected).toEqual([
        { key: 'compaction.strategy', reason: 'project-scope-model-roles-only' },
        { key: 'todo.reminders', reason: 'project-scope-model-roles-only' },
      ]);
    } finally {
      await disarm(env);
    }
  });

  test('project role write lands in <dir>/.omp/config.yml only; global layer untouched', async () => {
    const env = await makeEnv();
    const { store, dirB, boot } = env;
    try {
      const result = await applySettingsChanges(store, {
        directory: dirB,
        scope: 'project',
        changes: { 'modelRoles.default': 'prov/proj-model' },
      });
      expect(result.status).toBe(200);
      expect(result.body.applied['modelRoles.default']).toBe('prov/proj-model');

      const projectYaml = readProjectConfig(dirB);
      expect(projectYaml).toContain('modelRoles:');
      expect(projectYaml).toContain('default: prov/proj-model');
      // Global config has no modelRoles from a project-scope write.
      expect(boot.getGlobalModelRole('default')).toBeUndefined();
      // The directory's keyed instance (what its sessions consume) sees it.
      const keyed = await store.settingsFor(dirB);
      expect(keyed.getModelRole('default')).toBe('prov/proj-model');
      expect(keyed.getModelRoleSource('default')).toBe('project');

      // Clearing via null removes the role from the project file.
      const cleared = await applySettingsChanges(store, {
        directory: dirB,
        scope: 'project',
        changes: { 'modelRoles.default': null },
      });
      expect(cleared.status).toBe(200);
      const keyedAfter = await store.settingsFor(dirB);
      expect(keyedAfter.getProjectModelRole('default')).toBeUndefined();
    } finally {
      await disarm(env);
    }
  });

  test('global scope always executes on the boot instance regardless of directory', async () => {
    const env = await makeEnv();
    const { store, dirB, boot, agentDir } = env;
    try {
      const before = await store.settingsFor(dirB); // derived before the write
      const result = await applySettingsChanges(store, {
        directory: dirB,
        scope: 'global',
        changes: { autoResume: true },
      });
      expect(result.status).toBe(200);
      expect(boot.get('autoResume')).toBe(true);
      expect(boot.isConfigured('autoResume')).toBe(true);
      // Persisted in the boot/global config, not the project file.
      expect(readGlobalConfig(agentDir)).toContain('autoResume: true');
      expect(readProjectConfig(dirB)).toBeNull();

      // Registered caveat (06 §5.1.7b): an already-derived clone holds a
      // snapshot of the global layer from its derivation time — live clones
      // do not hot-see later global writes; freshly derived ones do.
      expect(before.get('autoResume')).toBe(false);
      await store.disposeAll();
      const after = await store.settingsFor(dirB);
      expect(after.get('autoResume')).toBe(true);
    } finally {
      await disarm(env);
    }
  });
});

describe('PUT validation (06 §5.3.1)', () => {
  test('unknown keys and type violations are rejected without echoing values', async () => {
    const env = await makeEnv();
    const { store } = env;
    try {
      const result = await applySettingsChanges(store, {
        changes: {
          'nope.nope': 'leak-me-unknown',
          'compaction.strategy': 'bogus-strategy',
          'todo.reminders': 'yes',
          modelRoles: { default: 'prov/x' },
        },
      });
      expect(result.status).toBe(400);
      expect(result.body.rejected).toEqual([
        { key: 'nope.nope', reason: 'unknown' },
        { key: 'compaction.strategy', reason: 'invalid-value' },
        { key: 'todo.reminders', reason: 'invalid-type' },
        { key: 'modelRoles', reason: 'record-write-unsupported' },
      ]);
      expect(JSON.stringify(result.body)).not.toContain('leak-me-unknown');
    } finally {
      await disarm(env);
    }
  });

  test('terminal-only keys are not editable over the API', async () => {
    const env = await makeEnv();
    const { store } = env;
    try {
      const result = await applySettingsChanges(store, {
        changes: { 'theme.dark': 'neon' },
      });
      expect(result.status).toBe(400);
      expect(result.body.rejected).toEqual([{ key: 'theme.dark', reason: 'not-editable' }]);
    } finally {
      await disarm(env);
    }
  });

  test('invalid scope / body shapes fail fast', async () => {
    const env = await makeEnv();
    const { store } = env;
    try {
      expect((await applySettingsChanges(store, { scope: 'somewhere' })).body.error).toBe('invalid-scope');
      expect((await applySettingsChanges(store, { changes: 'nope' })).body.error).toBe('invalid-body');
      expect((await applySettingsChanges(store, {})).body.error).toBe('invalid-body');
    } finally {
      await disarm(env);
    }
  });
});

describe('legacy defaultModel migration (01 §5.8, R12)', () => {
  test('detectLegacyDefaultModel is read-only and parse-gated', async () => {
    const env = await makeEnv();
    const { boot, agentDir } = env;
    try {
      const ocPath = path.join(makeDir(), 'settings.json');
      writeFileSync(ocPath, JSON.stringify({ defaultModel: 'oc-prov/oc-model' }));
      expect(detectLegacyDefaultModel({ settingsPath: ocPath })).toEqual({
        defaultModel: 'oc-prov/oc-model',
        defaultProvider: 'oc-prov',
      });

      // Values without "/" are not parseable model selectors → null.
      writeFileSync(ocPath, JSON.stringify({ defaultModel: 'bare-model' }));
      expect(detectLegacyDefaultModel({ settingsPath: ocPath })).toBeNull();
      writeFileSync(ocPath, JSON.stringify({ other: true }));
      expect(detectLegacyDefaultModel({ settingsPath: ocPath })).toBeNull();
      expect(detectLegacyDefaultModel({ settingsPath: path.join(makeDir(), 'missing.json') })).toBeNull();

      // Detection never writes omp config.
      const before = readGlobalConfig(agentDir);
      detectLegacyDefaultModel({ settingsPath: ocPath });
      expect(readGlobalConfig(agentDir)).toBe(before);
      expect(boot.getModelRole('default')).toBeUndefined();
    } finally {
      await disarm(env);
    }
  });

  test('importLegacyDefaultModel writes only when unset and never overwrites', async () => {
    const env = await makeEnv();
    const { boot } = env;
    try {
      expect(await importLegacyDefaultModel(boot, 'bare-no-slash')).toEqual({
        imported: false,
        reason: 'invalid-selector',
      });

      const imported = await importLegacyDefaultModel(boot, 'anthropic/claude-legacy');
      expect(imported.imported).toBe(true);
      expect(imported.role).toBe('default');
      expect(imported.scope).toBe('global');
      expect(imported.audit.originalValue).toBe('anthropic/claude-legacy');
      expect(imported.audit.importedRole).toBe('default');
      expect(boot.getModelRole('default')).toBe('anthropic/claude-legacy');

      const again = await importLegacyDefaultModel(boot, 'prov/other');
      expect(again).toEqual({
        imported: false,
        reason: 'role-already-configured',
        existing: 'anthropic/claude-legacy',
      });
      expect(boot.getModelRole('default')).toBe('anthropic/claude-legacy');
    } finally {
      await disarm(env);
    }
  });

  test('import honors modelRoleStorage=project on the passed keyed instance', async () => {
    const env = await makeEnv();
    const { store, dirB, boot } = env;
    try {
      const keyed = await store.settingsFor(dirB);
      keyed.override('modelRoleStorage', 'project');
      const imported = await importLegacyDefaultModel(keyed, 'prov/project-legacy');
      expect(imported.imported).toBe(true);
      expect(imported.scope).toBe('project');
      expect(keyed.getProjectModelRole('default')).toBe('prov/project-legacy');
      expect(readProjectConfig(dirB)).toContain('default: prov/project-legacy');
      // Global layer still untouched.
      expect(boot.getGlobalModelRole('default')).toBeUndefined();
    } finally {
      await disarm(env);
    }
  });
});

describe('revision + omp.settings.updated events (06 §5.3.5/§5.4)', () => {
  test('revision is monotonic; failures do not bump; payload carries keys/origin only', async () => {
    const env = await makeEnv();
    const { store } = env;
    const published = [];
    const publish = (type, payload, eventScope) => published.push({ type, payload, eventScope });
    try {
      const first = await applySettingsChanges(store, {
        changes: { 'todo.reminders': true },
        ...{},
      }, { publish });
      expect(first.body.revision).toBe(1);
      const second = await applySettingsChanges(store, {
        changes: { 'modelRoles.default': 'prov/x' },
      }, { publish });
      expect(second.body.revision).toBe(2);
      const third = await applySettingsChanges(store, {
        changes: { 'hindsight.apiToken': 'tok' },
      }, { publish });
      expect(third.body.revision).toBe(3);
      expect(store.getRevision()).toBe(3);

      // Failed validation does not bump or publish.
      const failed = await applySettingsChanges(store, {
        changes: { 'nope.nope': 1 },
      }, { publish });
      expect(failed.status).toBe(400);
      expect(store.getRevision()).toBe(3);
      expect(published.length).toBe(3);

      const last = published.at(-1);
      expect(last.type).toBe('omp.settings.updated');
      expect(last.payload).toEqual({
        revision: 3,
        keys: ['hindsight.apiToken'],
        origin: 'web',
      });
      expect(last.eventScope.directory).toBe(store.bootDirectory);
      expect(last.eventScope.durable).toBe(true);
      // Event payloads never carry credential values.
      expect(JSON.stringify(published)).not.toContain('tok');
      for (let i = 1; i < published.length; i += 1) {
        expect(published[i].payload.revision).toBeGreaterThan(published[i - 1].payload.revision);
      }
    } finally {
      await disarm(env);
    }
  });

  test('empty changes is an idempotent no-op: no bump, no event', async () => {
    const env = await makeEnv();
    const { store } = env;
    const published = [];
    try {
      const result = await applySettingsChanges(store, { changes: {} }, {
        publish: (t, p, s) => published.push({ t, p, s }),
      });
      expect(result.status).toBe(200);
      expect(result.body).toEqual({ revision: 0, applied: {}, persisted: true, quarantined: null });
      expect(store.getRevision()).toBe(0);
      expect(published).toHaveLength(0);
    } finally {
      await disarm(env);
    }
  });
});

describe('buildSettingsPayload (06 §5.2)', () => {
  test('schema-driven shape: tabs, defs, defaults, modelRoles record view', async () => {
    const env = await makeEnv();
    const { boot, store } = env;
    try {
      const payload = buildSettingsPayload(boot, { revision: 7 });
      expect(payload.schemaVersion).toBe(VERSION);
      expect(payload.revision).toBe(7);
      expect(payload.directory).toBe(boot.getCwd());
      expect(payload.agentDir).toBe(boot.getAgentDir());
      expect(payload.globalConfigPath).toBe(path.join(boot.getAgentDir(), 'config.yml'));
      expect(payload.projectConfigPath).toBe(path.join(boot.getCwd(), '.omp', 'config.yml'));

      expect(payload.tabs).toHaveLength(SETTING_TABS.length);
      expect(payload.tabs[0]).toEqual({
        id: 'appearance',
        label: 'Appearance',
        groups: expect.any(Array),
      });

      const compaction = payload.keys['compaction.strategy'];
      expect(compaction.type).toBe('enum');
      expect(compaction.values).toContain('snapcompact');
      expect(compaction.value).toBe('snapcompact');
      expect(compaction.configured).toBe(false);
      expect(compaction.scope).toBe('global');
      expect(compaction.editable).toBe(true);

      // Terminal-only surfaces stay readable but not editable (06 §5.6).
      expect(payload.keys['theme.dark'].editable).toBe(false);
      expect(payload.keys['theme.dark'].excluded).toBe('terminal-only');
      const imageProtocolKey = Object.values(payload.keys).find(
        (entry) => entry.ui?.condition === 'hasImageProtocol',
      );
      expect(imageProtocolKey.excluded).toBe('terminal-capability');

      // modelRoles record view with per-role source.
      expect(payload.keys.modelRoles.type).toBe('record');
      expect(payload.keys.modelRoles.modelRoleStorage).toBe('global');
      expect(payload.keys.modelRoles.roles.default).toEqual({
        value: null,
        source: 'default',
        editable: true,
      });

      // Key filter narrows the response.
      const filtered = buildSettingsPayload(boot, { keys: ['cycleOrder'] });
      expect(Object.keys(filtered.keys)).toEqual(['cycleOrder']);
      expect(filtered.keys.cycleOrder.value).toEqual(['smol', 'default', 'slow']);
    } finally {
      await disarm(env);
    }
  });

  test('condition-false keys report hidden and stay in the payload', async () => {
    const env = await makeEnv();
    const { boot } = env;
    try {
      const payload = buildSettingsPayload(boot);
      // Keys conditioned on advisor.enabled (default false) are hidden but stay.
      const advisorKey = Object.values(payload.keys).find(
        (entry) => entry.ui?.condition === 'advisorEnabled',
      );
      expect(advisorKey.hidden).toBe(true);
      expect(advisorKey.editable).toBe(false);
      // Unknown/unevaluated conditions fall back to visible (conservative).
      expect(payload.keys['compaction.strategy'].hidden).toBeUndefined();
    } finally {
      await disarm(env);
    }
  });
});

describe('route mounting', () => {
  test('GET /omp/models, GET/PUT /omp/settings wire to the store', async () => {
    const env = await makeEnv();
    const { store, dirB } = env;
    const ocPath = path.join(makeDir(), 'settings.json');
    writeFileSync(ocPath, JSON.stringify({ defaultModel: 'oc/legacy-model' }));
    const published = [];
    const routes = [];
    registerModelSettingsRoutes(
      (method, pattern, handler) => routes.push({ method, pattern, handler }),
      { store, publish: (t, p, s) => published.push({ t, p, s }), legacySettingsPath: ocPath },
    );
    expect(routes.map((r) => `${r.method} ${r.pattern}`)).toEqual([
      'GET /omp/models',
      'GET /omp/settings',
      'PUT /omp/settings',
    ]);
    const byRoute = (method, pattern) => routes.find((r) => r.method === method && r.pattern === pattern).handler;
    try {
      const modelsResponse = await byRoute('GET', '/omp/models')(
        new Request(`http://host/omp/models?directory=${encodeURIComponent(dirB)}`),
      );
      const models = await modelsResponse.json();
      expect(modelsResponse.status).toBe(200);
      expect(models.roles.default).toBeNull();
      expect(models.legacyDefaults).toEqual({ defaultModel: 'oc/legacy-model', defaultProvider: 'oc' });

      const putResponse = await byRoute('PUT', '/omp/settings')(
        new Request('http://host/omp/settings', {
          method: 'PUT',
          headers: { 'content-type': 'application/json' },
          body: JSON.stringify({ directory: dirB, scope: 'project', changes: { 'modelRoles.default': 'prov/routed' } }),
        }),
      );
      const putBody = await putResponse.json();
      expect(putResponse.status).toBe(200);
      expect(putBody.applied['modelRoles.default']).toBe('prov/routed');

      const settingsResponse = await byRoute('GET', '/omp/settings')(
        new Request(`http://host/omp/settings?directory=${encodeURIComponent(dirB)}&keys=modelRoles`),
      );
      const settings = await settingsResponse.json();
      expect(settings.keys.modelRoles.roles.default.value).toBe('prov/routed');
      expect(settings.keys.modelRoles.roles.default.source).toBe('project');
      expect(published.at(-1).t).toBe('omp.settings.updated');
    } finally {
      await disarm(env);
    }
  });
});
