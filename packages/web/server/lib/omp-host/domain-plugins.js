// OMP plugin management domain.
//
// Settings → Plugins projects omp's canonical package/marketplace registries
// and extension loader. OpenCode's `opencode.json#plugin` is never consulted.

import { execFile } from 'node:child_process';
import crypto from 'node:crypto';
import fs from 'node:fs';
import path from 'node:path';
import { promisify } from 'node:util';
import { Settings } from '@oh-my-pi/pi-coding-agent/config/settings';
import { getAgentDir } from '@oh-my-pi/pi-coding-agent';
import { classifyInstallTarget } from '@oh-my-pi/pi-coding-agent/cli/classify-install-target';
import {
  clearPluginRootsAndCaches,
  getExtensionNameFromPath,
  resolveActiveProjectRegistryPath,
  resolveOrDefaultProjectRegistryPath,
} from '@oh-my-pi/pi-coding-agent/discovery/helpers';
import { discoverExtensionPaths } from '@oh-my-pi/pi-coding-agent/extensibility/extensions';
import {
  getEnabledPlugins,
  getPluginSettings,
  parseSettingValue,
  PluginManager,
  resolvePluginManifestEntries,
  validateSetting,
} from '@oh-my-pi/pi-coding-agent/extensibility/plugins';
import {
  MarketplaceManager,
  getInstalledPluginsRegistryPath,
  getMarketplacesCacheDir,
  getMarketplacesRegistryPath,
  getPluginsCacheDir,
} from '@oh-my-pi/pi-coding-agent/extensibility/plugins/marketplace';
import { featureUnavailable, ompFeatures } from './omp-parity.js';

const json = (data, init) => Response.json(data, init);
const badRequest = (message) => json({ error: message }, { status: 400 });
const notFound = (message) => json({ error: message }, { status: 404 });
const failed = (message) => json({ error: message }, { status: 500 });
const EXTENSION_FILE_PATTERN = /^[a-z0-9][a-z0-9-_.]*\.(js|ts|mjs|cjs)$/i;
const extensionFilesById = new Map();

const restartDeferred = (message) => ({
  ok: true,
  requiresRestart: true,
  restartDeferred: true,
  message,
});

const encodePluginId = (kind, scope, name) =>
  Buffer.from(`${kind}:${scope}:${name}`).toString('base64url');

const decodePluginId = (id) => {
  try {
    const decoded = Buffer.from(id, 'base64url').toString('utf8');
    const first = decoded.indexOf(':');
    const second = decoded.indexOf(':', first + 1);
    if (first <= 0 || second <= first + 1) return null;
    const kind = decoded.slice(0, first);
    const scope = decoded.slice(first + 1, second);
    const name = decoded.slice(second + 1);
    if (!['npm', 'marketplace'].includes(kind)) return null;
    if (!['user', 'project'].includes(scope) || !name) return null;
    return { kind, scope, name };
  } catch {
    return null;
  }
};

const canonicalPath = (filePath) => {
  try {
    return fs.realpathSync(filePath);
  } catch {
    return path.resolve(filePath);
  }
};

const isWithin = (filePath, rootPath) => {
  const relative = path.relative(rootPath, filePath);
  return relative === '' || (!relative.startsWith('..') && !path.isAbsolute(relative));
};

const extensionId = (filePath) =>
  `ext_${crypto.createHash('sha256').update(canonicalPath(filePath)).digest('base64url').slice(0, 20)}`;

const rememberExtension = (filePath, metadata) => {
  const id = extensionId(filePath);
  extensionFilesById.set(id, { path: canonicalPath(filePath), ...metadata });
  return id;
};

const manifestFeatures = (manifest, enabledFeatures) => {
  const enabled = enabledFeatures === null
    ? new Set(Object.entries(manifest?.features ?? {})
      .filter(([, value]) => value?.default !== false)
      .map(([name]) => name))
    : new Set(enabledFeatures ?? []);
  return Object.entries(manifest?.features ?? {}).map(([name, value]) => ({
    name,
    enabled: enabled.has(name),
    ...(value?.description ? { description: value.description } : {}),
  }));
};

/** omp TUI parity: every plugin mutation clears the process-global discovery
 * caches (plugin roots + enabled plugins), else the next list projects a
 * freshly installed plugin's manifest entries as not loaded. */
const invalidatePluginCaches = async (directory) => {
  try {
    const projectRegistryPath = await resolveOrDefaultProjectRegistryPath(directory);
    clearPluginRootsAndCaches(projectRegistryPath ? [projectRegistryPath] : undefined);
  } catch {
    clearPluginRootsAndCaches();
  }
};

const manifestSettings = (manifest, values) => Object.fromEntries(
  Object.entries(manifest?.settings ?? {}).map(([key, schema]) => {
    const value = values?.[key];
    return [key, {
      type: schema?.type,
      description: schema?.description,
      secret: schema?.secret === true,
      configured: value !== undefined,
      ...(schema?.secret === true || value === undefined ? {} : { value }),
      ...(schema?.default === undefined ? {} : { default: schema.default }),
      ...(Array.isArray(schema?.values) ? { values: schema.values } : {}),
      ...(typeof schema?.min === 'number' ? { min: schema.min } : {}),
      ...(typeof schema?.max === 'number' ? { max: schema.max } : {}),
      ...(typeof schema?.step === 'number' ? { step: schema.step } : {}),
    }];
  }),
);

const marketplaceManagerFor = async (directory) => new MarketplaceManager({
  marketplacesRegistryPath: getMarketplacesRegistryPath(),
  installedRegistryPath: getInstalledPluginsRegistryPath(),
  projectInstalledRegistryPath: await resolveOrDefaultProjectRegistryPath(directory),
  marketplacesCacheDir: getMarketplacesCacheDir(),
  pluginsCacheDir: getPluginsCacheDir(),
});

const projectOverridesPath = (directory) =>
  path.join(directory, '.omp', 'plugin-overrides.json');

const readProjectOverrides = (directory) => {
  const overridesPath = projectOverridesPath(directory);
  try {
    return JSON.parse(fs.readFileSync(overridesPath, 'utf8'));
  } catch {
    return {};
  }
};

const writeProjectOverrides = (directory, overrides) => {
  const overridesPath = projectOverridesPath(directory);
  fs.mkdirSync(path.dirname(overridesPath), { recursive: true });
  const tempPath = `${overridesPath}.tmp-${process.pid}-${Date.now()}`;
  fs.writeFileSync(tempPath, JSON.stringify(overrides, null, 2), 'utf8');
  fs.renameSync(tempPath, overridesPath);
};

/** Toggle / feature / setting mutation through .omp/plugin-overrides.json —
 * the same file omp TUI manages. Cache invalidation picks it up on the next
 * discovery pass, so project-scoped package plugins become fully editable. */
const applyProjectOverride = (directory, pluginName, mutation) => {
  const overrides = readProjectOverrides(directory);
  if (mutation.enabled === false) {
    overrides.disabled = [...new Set([...(overrides.disabled ?? []), pluginName])];
  } else if (mutation.enabled === true) {
    overrides.disabled = (overrides.disabled ?? []).filter((name) => name !== pluginName);
  }
  if (Array.isArray(mutation.enabledFeatures)) {
    overrides.features = { ...(overrides.features ?? {}), [pluginName]: mutation.enabledFeatures };
  }
  if (mutation.setting) {
    const settings = { ...(overrides.settings ?? {}) };
    const pluginSettings = { ...(settings[pluginName] ?? {}) };
    if (mutation.setting.remove === true) delete pluginSettings[mutation.setting.key];
    else pluginSettings[mutation.setting.key] = mutation.setting.value;
    settings[pluginName] = pluginSettings;
    overrides.settings = settings;
  }
  writeProjectOverrides(directory, overrides);
};

const execFileAsync = promisify(execFile);

/**
 * Platform file-manager reveal command for an absolute target. macOS selects
 * the file (open -R) or opens the directory; Windows selects via
 * `explorer /select,`; other platforms open the parent directory only.
 * Pure builder — unit-testable without touching a real file manager.
 */
export const revealCommand = (platform, targetPath) => {
  if (platform === 'darwin') return { command: 'open', args: ['-R', targetPath] };
  if (platform === 'win32') {
    return fs.existsSync(targetPath) && fs.statSync(targetPath).isFile()
      ? { command: 'explorer', args: [`/select,${targetPath}`] }
      : { command: 'explorer', args: [targetPath] };
  }
  return { command: 'xdg-open', args: [fs.existsSync(targetPath) ? path.dirname(targetPath) : targetPath] };
};

const revealInFileManager = (targetPath) => {
  const { command, args } = revealCommand(process.platform, targetPath);
  return execFileAsync(command, args, { windowsHide: true }).then(() => true);
};

/** Resolve a package plugin id to its on-disk root via omp registries. */
const pluginPathForId = async (target, directory) => {
  if (target.kind === 'npm') {
    const plugin = (await new PluginManager(directory).list()).find((item) => item.name === target.name);
    return plugin?.path ?? null;
  }
  const summary = (await marketplaceManagerFor(directory).listInstalledPlugins())
    .find((item) => item.id === target.name);
  const entry = summary?.entries?.find((item) => item.enabled !== false) ?? summary?.entries?.[0];
  return entry?.installPath ?? null;
};

const nativeExtensionDirectories = async (directory) => {
  const directories = [{ scope: 'user', path: path.join(getAgentDir(), 'extensions') }];
  const projectRegistryPath = await resolveActiveProjectRegistryPath(directory);
  if (projectRegistryPath) {
    const projectOmpDir = path.dirname(path.dirname(projectRegistryPath));
    directories.push({ scope: 'project', path: path.join(projectOmpDir, 'extensions') });
  }
  return directories;
};

const settingsExtensionPaths = async (directory) => {
  try {
    const settings = await Settings.loadReadOnly({ cwd: directory, agentDir: getAgentDir() });
    return {
      configured: settings.get('extensions') ?? [],
      disabled: settings.get('disabledExtensions') ?? [],
    };
  } catch {
    return { configured: [], disabled: [] };
  }
};

const projectExtension = ({ filePath, scope, source, editable, pluginId, pluginName, declaredEntry, loaded = true }) => {
  const name = getExtensionNameFromPath(filePath);
  const id = loaded
    ? rememberExtension(filePath, { editable, scope, source, pluginId, pluginName })
    : `missing_${crypto.createHash('sha256').update(`${pluginId}:${declaredEntry}`).digest('base64url').slice(0, 20)}`;
  return {
    id,
    kind: 'extension',
    scope,
    name,
    source,
    editable,
    loaded,
    ...(pluginId ? { pluginId } : {}),
    ...(pluginName ? { pluginName } : {}),
    ...(declaredEntry ? { declaredEntry } : {}),
  };
};

const projectPlugin = (plugin, {
  kind = 'npm',
  name = plugin.name,
  scope = plugin.scope ?? 'user',
  settingValues = {},
} = {}) => {
  const userPackage = kind === 'npm' && scope === 'user';
  const npmPackage = kind === 'npm';
  return {
    id: encodePluginId(kind, scope, name),
    kind,
    scope,
    name,
    version: plugin.version || 'unknown',
    enabled: plugin.enabled !== false,
    editable: npmPackage,
    permissions: {
      toggle: npmPackage,
      features: npmPackage,
      settings: npmPackage,
      uninstall: userPackage,
    },
    ...(plugin.manifest?.description ? { description: plugin.manifest.description } : {}),
    features: manifestFeatures(plugin.manifest, plugin.enabledFeatures),
    settings: manifestSettings(plugin.manifest, settingValues),
    extensionEntries: [],
  };
};

const listPlugins = async (directory) => {
  extensionFilesById.clear();
  const manager = new PluginManager(directory);
  const extensionSettings = await settingsExtensionPaths(directory);
  const [managedPlugins, enabledPlugins, marketplaceManager, discoveredPaths, nativeDirs] = await Promise.all([
    manager.list(),
    getEnabledPlugins(directory),
    marketplaceManagerFor(directory),
    discoverExtensionPaths(extensionSettings.configured, directory, extensionSettings.disabled),
    nativeExtensionDirectories(directory),
  ]);
  const marketplacePlugins = await marketplaceManager.listInstalledPlugins();

  const rawPlugins = new Map();
  for (const plugin of managedPlugins) rawPlugins.set(`user:${plugin.name}`, { ...plugin, scope: 'user', cwd: directory });
  for (const plugin of enabledPlugins) rawPlugins.set(`${plugin.scope}:${plugin.name}`, { ...plugin, cwd: directory });

  const plugins = [];
  const pluginRawById = new Map();
  for (const plugin of rawPlugins.values()) {
    const projected = projectPlugin(plugin, { settingValues: await getPluginSettings(plugin.name, directory) });
    plugins.push(projected);
    pluginRawById.set(projected.id, plugin);
  }

  const marketplaceRawById = new Map();
  for (const summary of marketplacePlugins) {
    const entry = summary.entries?.find((item) => item.enabled !== false) ?? summary.entries?.[0];
    if (!entry) continue;
    const packageJsonPath = path.join(entry.installPath, 'package.json');
    let packageJson;
    try {
      packageJson = JSON.parse(fs.readFileSync(packageJsonPath, 'utf8'));
    } catch {
      packageJson = null;
    }
    const manifest = packageJson?.omp ?? packageJson?.pi;
    const projected = {
      id: encodePluginId('marketplace', summary.scope, summary.id),
      kind: 'marketplace',
      scope: summary.scope,
      name: summary.id,
      version: entry.version || packageJson?.version || 'unknown',
      enabled: entry.enabled !== false,
      editable: true,
      permissions: {
        toggle: true,
        features: false,
        settings: false,
        uninstall: true,
      },
      ...(manifest?.description ? { description: manifest.description } : {}),
      features: manifestFeatures(manifest, null),
      settings: manifestSettings(manifest, {}),
      extensionEntries: [],
    };
    plugins.push(projected);
    marketplaceRawById.set(projected.id, {
      name: summary.id,
      scope: summary.scope,
      version: projected.version,
      enabled: projected.enabled,
      enabledFeatures: null,
      manifest,
      path: entry.installPath,
      cwd: directory,
    });
  }

  const discovered = new Map(discoveredPaths.map((item) => [canonicalPath(item), item]));
  const extensions = [];
  const extensionIds = new Set();

  for (const plugin of plugins) {
    const raw = pluginRawById.get(plugin.id) ?? marketplaceRawById.get(plugin.id);
    if (!raw) continue;
    for (const entry of resolvePluginManifestEntries(raw, 'extensions')) {
      const resolved = entry.resolvedPath ? canonicalPath(entry.resolvedPath) : null;
      const record = projectExtension({
        filePath: resolved ?? path.join(raw.path, entry.entry),
        scope: plugin.scope,
        source: 'plugin-manifest',
        editable: false,
        pluginId: plugin.id,
        pluginName: plugin.name,
        declaredEntry: entry.entry,
        loaded: resolved ? discovered.has(resolved) : false,
      });
      plugin.extensionEntries.push(record.id);
      if (!extensionIds.has(record.id)) {
        extensionIds.add(record.id);
        extensions.push(record);
      }
    }
  }

  const configuredRoots = extensionSettings.configured.map((item) => canonicalPath(path.isAbsolute(item) ? item : path.resolve(directory, item)));
  for (const discoveredPath of discovered.keys()) {
    if (extensionIds.has(extensionId(discoveredPath))) continue;
    const native = nativeDirs.find((entry) => isWithin(discoveredPath, canonicalPath(entry.path)));
    const configured = configuredRoots.some((root) => isWithin(discoveredPath, root));
    const scope = native?.scope ?? (isWithin(discoveredPath, canonicalPath(directory)) ? 'project' : 'user');
    const record = projectExtension({
      filePath: discoveredPath,
      scope,
      source: native ? 'native' : configured ? 'configured' : 'discovered',
      editable: Boolean(native && path.dirname(discoveredPath) === canonicalPath(native.path)),
    });
    extensionIds.add(record.id);
    extensions.push(record);
  }

  return { plugins, extensions };
};

export const registerPluginsDomainRoutes = (
  route,
  { features = ompFeatures(), list = listPlugins, snapshots = null, reloadSessions = null } = {},
) => {
  route('GET', '/omp/plugins', async (request) => {
    if (features?.['plugins.v1'] !== true) return featureUnavailable('plugins.v1');
    const url = new URL(request.url);
    const directory = url.searchParams.get('directory') ?? process.cwd();
    try {
      return json(await list(directory));
    } catch (error) {
      console.warn('[omp-host] failed to list plugins:', error?.message ?? error);
      return failed('Failed to list omp plugins');
    }
  });

  route('POST', '/omp/plugins', async (request) => {
    if (features?.['plugins.v1'] !== true) return featureUnavailable('plugins.v1');
    const body = await request.json().catch(() => ({}));
    const spec = typeof body?.spec === 'string' ? body.spec.trim() : '';
    const directory = typeof body?.directory === 'string' && body.directory ? body.directory : process.cwd();
    if (!spec) return badRequest('spec is required');
    try {
      const manager = new PluginManager(directory);
      const marketplaceManager = await marketplaceManagerFor(directory);
      const marketplaces = await marketplaceManager.listMarketplaces();
      const target = classifyInstallTarget(spec, new Set(marketplaces.map((entry) => entry.name)));
      const scope = body?.scope === 'project' ? 'project' : 'user';
      // omp semantics: only marketplace installs support project scope. npm
      // packages and local paths always install to the user plugins root.
      // Reject the mismatch instead of silently downgrading scope.
      if (scope === 'project' && target.type !== 'marketplace') {
        return badRequest('project scope is only supported for marketplace installs (name@marketplace)');
      }
      if (target.type === 'marketplace') {
        await marketplaceManager.installPlugin(target.name, target.marketplace, { scope });
      } else if (target.type === 'local') {
        const localPath = target.path === '~' || target.path.startsWith('~/') || target.path.startsWith('~\\')
          ? path.join(os.homedir(), target.path.slice(2))
          : target.path;
        await manager.link(localPath);
      } else {
        await manager.install(target.spec);
      }
      await invalidatePluginCaches(directory);
      return json(restartDeferred('OMP plugin installed. Restart the omp engine to apply it.'));
    } catch (error) {
      console.warn('[omp-host] failed to install plugin:', error?.message ?? error);
      return failed('Failed to install omp plugin');
    }
  });

  route('GET', '/omp/plugins/extensions/{id}', async (request, ctx) => {
    if (features?.['plugins.v1'] !== true) return featureUnavailable('plugins.v1');
    const target = extensionFilesById.get(ctx.params.id);
    if (!target) return notFound('extension not found');
    try {
      return json({
        fileName: path.basename(target.path),
        scope: target.scope,
        content: fs.readFileSync(target.path, 'utf8'),
        editable: target.editable === true,
        source: target.source,
      });
    } catch {
      return failed('Failed to read omp extension');
    }
  });

  route('POST', '/omp/plugins/extensions', async (request) => {
    if (features?.['plugins.v1'] !== true) return featureUnavailable('plugins.v1');
    const body = await request.json().catch(() => ({}));
    const directory = typeof body?.directory === 'string' && body.directory ? body.directory : process.cwd();
    const scope = body?.scope === 'project' ? 'project' : 'user';
    const fileName = typeof body?.fileName === 'string' ? body.fileName.trim() : '';
    if (!EXTENSION_FILE_PATTERN.test(fileName)) return badRequest('invalid extension file name');
    const dirs = await nativeExtensionDirectories(directory);
    const targetDir = dirs.find((entry) => entry.scope === scope);
    if (!targetDir) return badRequest('project extension scope unavailable');
    const targetPath = path.join(targetDir.path, fileName);
    if (fs.existsSync(targetPath)) return json({ error: 'extension already exists' }, { status: 409 });
    fs.mkdirSync(targetDir.path, { recursive: true });
    fs.writeFileSync(targetPath, typeof body?.content === 'string' ? body.content : '', 'utf8');
    rememberExtension(targetPath, { editable: true, scope, source: 'native' });
    return json(restartDeferred('OMP extension created. Restart the omp engine to apply it.'));
  });

  route('DELETE', '/omp/plugins/extensions/{id}', async (_request, ctx) => {
    if (features?.['plugins.v1'] !== true) return featureUnavailable('plugins.v1');
    const target = extensionFilesById.get(ctx.params.id);
    if (!target) return notFound('extension not found');
    if (target.editable !== true) return badRequest('extension entry is read-only');
    fs.unlinkSync(target.path);
    extensionFilesById.delete(ctx.params.id);
    return json(restartDeferred('OMP extension removed. Restart the omp engine to apply it.'));
  });

  route('PATCH', '/omp/plugins/{id}', async (request, ctx) => {
    if (features?.['plugins.v1'] !== true) return featureUnavailable('plugins.v1');
    const target = decodePluginId(ctx.params.id);
    if (!target) return badRequest('invalid plugin id');
    const body = await request.json().catch(() => ({}));
    const directory = typeof body?.directory === 'string' && body.directory ? body.directory : process.cwd();
    try {
      if (target.kind === 'marketplace') {
        if (typeof body?.enabled !== 'boolean') return badRequest('enabled must be boolean');
        const manager = await marketplaceManagerFor(directory);
        await manager.setPluginEnabled(target.name, body.enabled, target.scope);
      } else if (target.scope === 'project') {
        // Project-scoped package plugins mutate through .omp/plugin-overrides.json
        // (the same file omp TUI manages) instead of the global lockfile.
        applyProjectOverride(directory, target.name, body);
      } else {
        const manager = new PluginManager(directory);
        if (typeof body?.enabled === 'boolean') await manager.setEnabled(target.name, body.enabled);
        if (Array.isArray(body?.enabledFeatures)) await manager.setEnabledFeatures(target.name, body.enabledFeatures);
        if (body?.setting && typeof body.setting === 'object' && typeof body.setting.key === 'string') {
          if (body.setting.remove === true) {
            await manager.deletePluginSetting(target.name, body.setting.key);
          } else {
            const plugin = (await manager.list()).find((item) => item.name === target.name);
            const schema = plugin?.manifest?.settings?.[body.setting.key];
            if (!schema) return badRequest('unknown plugin setting');
            const value = typeof body.setting.value === 'string'
              ? parseSettingValue(body.setting.value, schema)
              : body.setting.value;
            const validation = validateSetting(value, schema);
            if (!validation.valid) return badRequest(validation.error ?? 'invalid plugin setting');
            await manager.setPluginSetting(target.name, body.setting.key, value);
          }
        }
      }
      await invalidatePluginCaches(directory);
      return json(restartDeferred('OMP plugin state updated. Restart the omp engine to apply it.'));
    } catch (error) {
      if (/not found|does not exist/i.test(error?.message ?? '')) return notFound('plugin not found');
      console.warn('[omp-host] failed to update plugin:', error?.message ?? error);
      return failed('Failed to update omp plugin');
    }
  });

  route('DELETE', '/omp/plugins/{id}', async (request, ctx) => {
    if (features?.['plugins.v1'] !== true) return featureUnavailable('plugins.v1');
    const target = decodePluginId(ctx.params.id);
    if (!target) return badRequest('invalid plugin id');
    const url = new URL(request.url);
    const directory = url.searchParams.get('directory') ?? process.cwd();
    try {
      if (target.kind === 'marketplace') {
        const manager = await marketplaceManagerFor(directory);
        await manager.uninstallPlugin(target.name, target.scope);
      } else {
        if (target.scope !== 'user') return badRequest('project package plugins are read-only');
        await new PluginManager(directory).uninstall(target.name);
      }
      await invalidatePluginCaches(directory);
      return json(restartDeferred('OMP plugin removed. Restart the omp engine to apply it.'));
    } catch (error) {
      if (/not found|does not exist/i.test(error?.message ?? '')) return notFound('plugin not found');
      console.warn('[omp-host] failed to remove plugin:', error?.message ?? error);
      return failed('Failed to remove omp plugin');
    }
  });

  route('POST', '/omp/plugins/{id}/reveal', async (request, ctx) => {
    if (features?.['plugins.v1'] !== true) return featureUnavailable('plugins.v1');
    const target = decodePluginId(ctx.params.id);
    if (!target) return badRequest('invalid plugin id');
    const url = new URL(request.url);
    const directory = url.searchParams.get('directory') ?? process.cwd();
    try {
      const targetPath = await pluginPathForId(target, directory);
      if (!targetPath || !fs.existsSync(targetPath)) return notFound('plugin path not found');
      await revealInFileManager(targetPath);
      return json({ ok: true });
    } catch (error) {
      console.warn('[omp-host] failed to reveal plugin:', error?.message ?? error);
      return failed('Failed to reveal omp plugin');
    }
  });

  route('POST', '/omp/plugins/extensions/{id}/reveal', async (_request, ctx) => {
    if (features?.['plugins.v1'] !== true) return featureUnavailable('plugins.v1');
    const target = extensionFilesById.get(ctx.params.id);
    if (!target) return notFound('extension not found');
    if (!fs.existsSync(target.path)) return notFound('extension path not found');
    try {
      await revealInFileManager(target.path);
      return json({ ok: true });
    } catch (error) {
      console.warn('[omp-host] failed to reveal extension:', error?.message ?? error);
      return failed('Failed to reveal omp extension');
    }
  });
  route('GET', '/omp/plugins/applied', async (request) => {
    if (features?.['plugins.v1'] !== true) return featureUnavailable('plugins.v1');
    const url = new URL(request.url);
    const directory = url.searchParams.get('directory');
    if (!snapshots) return badRequest('applied snapshots unavailable');
    try {
      // Populate the extension id map first so snapshot paths can join the
      // same projected ids the Settings list already renders.
      await list(directory ?? process.cwd());
      const byPath = new Map([...extensionFilesById.entries()].map(([id, meta]) => [meta.path, id]));
      const sessions = snapshots()
        .filter((snapshot) => !directory || snapshot.directory === directory)
        .map((snapshot) => ({
          sessionId: snapshot.sessionId,
          directory: snapshot.directory,
          appliedAt: snapshot.appliedAt,
          extensionIds: snapshot.extensionPaths
            .map((item) => byPath.get(item))
            .filter(Boolean),
          pluginNames: snapshot.pluginNames,
        }));
      return json({ sessions });
    } catch (error) {
      console.warn('[omp-host] failed to project applied plugins:', error?.message ?? error);
      return failed('Failed to project applied plugins');
    }
  });

  route('POST', '/omp/plugins/reload', async (request) => {
    if (features?.['plugins.v1'] !== true) return featureUnavailable('plugins.v1');
    const url = new URL(request.url);
    const directory = url.searchParams.get('directory') ?? process.cwd();
    if (typeof reloadSessions !== 'function') return badRequest('reload unavailable');
    try {
      const { sessionsRefreshed } = await reloadSessions(directory, url.searchParams.get('sessionId'));
      return json({ ok: true, sessionsRefreshed });
    } catch (error) {
      console.warn('[omp-host] failed to reload plugins:', error?.message ?? error);
      return failed('Failed to reload omp plugins');
    }
  });
};

export { decodePluginId, encodePluginId, listPlugins, projectExtension, projectPlugin };
