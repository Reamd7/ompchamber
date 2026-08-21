// OpenCode-compatible endpoint implementations for the omp host.
//
// Every route here exists because OpenChamber's vendored wire client calls it
// (see packages/ui/src/lib/opencode/wire). Features with no omp equivalent
// respond with stable, explicit errors instead of pretending success.

import fs from 'node:fs';
import path from 'node:path';
import { execFile } from 'node:child_process';
import { promisify } from 'node:util';
import { BUILTIN_TOOLS, getAgentDir } from '@oh-my-pi/pi-coding-agent';
import { normalizeDirectoryKey } from './registry.js';
import { buildCapabilities, featureUnavailable, ompFeatures } from './omp-parity.js';
import { registerModelSettingsRoutes, buildModelsPayload } from './domain-models.js';
import { registerModesDomainRoutes } from './domain-modes.js';
import { registerCommandsDomainRoutes } from './domain-commands.js';
import { registerChromeDomainRoutes } from './domain-chrome.js';


/**
 * Resolve the wire config's default-model pointer from modelRoles.default
 * through the keyed Settings instance (spec 01 §5.3/GAP-03). Falls back to
 * omitting the key when no role default resolves — never pins the
 * alphabetically-first provider model.
 */
export const defaultModelPointer = async (engine) => {
  try {
    const store = await engine.settingsStoreReady();
    if (!store) return {};
    const settings = await store.settingsFor(process.cwd());
    const roleDefault = buildModelsPayload(settings).roles?.default;
    if (roleDefault?.provider && roleDefault?.id) {
      return { model: `${roleDefault.provider}/${roleDefault.id}` };
    }
  } catch {
    // Settings unavailable: omit the pointer rather than pin a wrong model.
  }
  return {};
};
const execFileAsync = promisify(execFile);

const json = (data, init) => Response.json(data, init);

const notFound = (message) =>
  json({ name: 'UnknownError', data: { message } }, { status: 404 });

const badRequest = (message) =>
  json({ name: 'UnknownError', data: { message } }, { status: 400 });

const unsupported = (message) =>
  json({ name: 'UnknownError', data: { message } }, { status: 501 });

const readJsonBody = async (request) => {
  try {
    return await request.json();
  } catch {
    return {};
  }
};

/**
 * Parse a prompt request body into the engine's prompt inputs.
 *
 * The wire contract sends `{ parts: [text|file|agent|subtask], messageID }`
 * (see SessionPromptAsyncData in the vendored types). The legacy
 * `{ prompt: { text, files } }` shape is still accepted for the synchronous
 * `/message` consumer. Dropping `parts` here persisted every user message
 * with an empty part list, which the UI hides after a reload.
 */
export const promptPayloadFromWire = (body) => {
  const messageID = typeof body?.messageID === 'string' && body.messageID ? body.messageID : undefined;
  const parts = Array.isArray(body?.parts) ? body.parts : [];
  if (parts.length === 0) {
    return {
      text: body?.prompt?.text ?? '',
      images: (body?.prompt?.files ?? [])
        .filter((file) => typeof file?.data === 'string')
        .map((file) => ({ data: file.data, mimeType: file.mime })),
      messageID,
    };
  }
  const texts = [];
  const images = [];
  for (const part of parts) {
    if (part.type === 'text' && typeof part.text === 'string' && part.text.length > 0) {
      texts.push(part.text);
    } else if (part.type === 'file' && typeof part.url === 'string' && part.url.startsWith('data:')) {
      const comma = part.url.indexOf(',');
      const meta = part.url.slice(5, comma === -1 ? part.url.length : comma);
      const payload = part.url.slice(comma === -1 ? part.url.length : comma + 1);
      const isBase64 = /;base64$/i.test(meta);
      images.push({
        data: isBase64 ? payload : Buffer.from(payload, 'utf8').toString('base64'),
        mimeType: meta.split(';')[0] || part.mime || 'application/octet-stream',
      });
    } else if (part.type === 'agent' && typeof part.name === 'string' && part.name) {
      const mention = part.source?.value ?? `@${part.name}`;
      if (!texts.some((text) => text.includes(mention))) texts.push(mention);
    } else if (part.type === 'subtask' && typeof part.prompt === 'string' && part.prompt) {
      texts.push(part.prompt);
    }
  }
  return { text: texts.join('\n\n'), images, messageID };
};

const directoryFromRequest = ({ url, headers }) => {
  const fromQuery = url.searchParams.get('directory') ?? url.searchParams.get('location[directory]');
  const fromHeader = headers.get('x-opencode-directory');
  const raw = fromQuery ?? (fromHeader ? decodeURIComponent(fromHeader) : null);
  return raw ? normalizeDirectoryKey(raw) : null;
};

const git = async (cwd, ...args) => {
  try {
    const { stdout } = await execFileAsync('git', ['-C', cwd, ...args], { windowsHide: true });
    return stdout.trim();
  } catch {
    return null;
  }
};

/** @returns {import('node:http').IncomingMessage & { write, flushHeaders }} */
const sseResponseInit = () => ({
  status: 200,
  headers: {
    'content-type': 'text/event-stream',
    'cache-control': 'no-cache',
    connection: 'keep-alive',
    'x-accel-buffering': 'no',
  },
});

/**
 * Register every consumed route.
 * @param {(method: string, pattern: string, handler: Function) => void} route
 */
export const registerEndpoints = (route, engine, { version }) => {
  const providersPayload = async () => {
    await engine.ready();
    const models = engine.availableModels();
    const byProvider = new Map();
    for (const model of models) {
      const list = byProvider.get(model.provider) ?? [];
      list.push(model);
      byProvider.set(model.provider, list);
    }
    const providers = [...byProvider.entries()].map(([id, list]) => ({
      id,
      name: id,
      env: [],
      models: list.map((model) => ({
        id: model.id,
        name: model.name ?? model.id,
        ...(model.reasoning ? { reasoning: true } : {}),
        ...(model.contextWindow ? { limit: { context: model.contextWindow, output: model.maxTokens ?? 0 } } : {}),
      })),
    }));
    return providers;
  };

  const configPayload = async () => {
    await engine.ready();
    return {
      version,
      // omp keeps its own model/provider config (~/.omp/agent/config.yml);
      // the default-model pointer resolves modelRoles.default through the
      // keyed Settings instance (spec 01 §5.3/GAP-03) instead of pinning
      // whichever provider model sorts first.
      ...(await defaultModelPointer(engine)),
      agents: [...engine.customAgents.values()],
      provider: await providersPayload(),
      // OpenCode-specific keys with no omp equivalent are absent; PATCH /config
      // stores custom agents only.
    };
  };

  const projectDirectory = (requestContext) =>
    directoryFromRequest(requestContext) ?? process.cwd();

  // ---- global ----
  route('GET', '/global/health', async () => json({ healthy: true, version, uptime: process.uptime() }));
  route('POST', '/global/dispose', async () => {
    setTimeout(() => process.exit(0), 0);
    return json({});
  });
  route('POST', '/instance/dispose', async () => {
    setTimeout(() => process.exit(0), 0);
    return json({});
  });
  route('GET', '/global/config', async () => json(await configPayload()));
  route('PATCH', '/global/config', async () => json(await configPayload()));

  // ---- sessions ----
  route('GET', '/session', async (request, ctx) => {
    const directory = projectDirectory(ctx);
    const sessions = await engine.listSessions({ directory });
    return json(sessions);
  });
  route('POST', '/session', async (request, ctx) => {
    const body = await readJsonBody(request);
    const directory = body.directory ?? projectDirectory(ctx);
    const session = await engine.createSession({
      directory,
      title: body.title,
      parentID: body.parentID,
      ...(body.parentID ? {} : {}),
    });
    return json(session);
  });
  route('GET', '/session/status', async (request, ctx) => {
    const directory = projectDirectory(ctx);
    return json(await engine.getSessionStatuses({ directory }));
  });
  route('GET', '/session/{sessionID}', async (request, ctx) => {
    const directory = projectDirectory(ctx);
    const session = await engine.getSession({ sessionID: ctx.params.sessionID, directory });
    return session ? json(session) : notFound('session not found');
  });
  route('PATCH', '/session/{sessionID}', async (request, ctx) => {
    const body = await readJsonBody(request);
    const directory = body.directory ?? projectDirectory(ctx);
    const session = await engine.updateSession({
      sessionID: ctx.params.sessionID,
      directory,
      title: body.title,
      metadata: body.metadata,
      timeArchived: body.time?.archived,
    });
    return json(session);
  });
  route('DELETE', '/session/{sessionID}', async (request, ctx) => {
    const url = ctx.url;
    const directory = directoryFromRequest(ctx) ?? url.searchParams.get('directory') ?? process.cwd();
    await engine.deleteSession({ sessionID: ctx.params.sessionID, directory });
    return json({});
  });
  route('GET', '/session/{sessionID}/children', async (request, ctx) => {
    const directory = projectDirectory(ctx);
    const all = await engine.listSessions({ directory });
    return json(all.filter((s) => s.parentID === ctx.params.sessionID));
  });
  route('GET', '/session/{sessionID}/todo', async (request, ctx) => {
    const directory = projectDirectory(ctx);
    return json(await engine.getTodos({ sessionID: ctx.params.sessionID, directory }));
  });
  route('GET', '/session/{sessionID}/message', async (request, ctx) => {
    const directory = projectDirectory(ctx);
    const limitParam = Number(ctx.url.searchParams.get('limit'));
    const before = ctx.url.searchParams.get('before') ?? undefined;
    const page = await engine.getMessagesPage({
      sessionID: ctx.params.sessionID,
      directory,
      limit: Number.isFinite(limitParam) ? limitParam : undefined,
      before,
    });
    if (!page) return notFound('session not found');
    return page.cursor
      ? json(page.messages, { headers: { 'x-next-cursor': page.cursor } })
      : json(page.messages);
  });
  route('POST', '/session/{sessionID}/message', async (request, ctx) => {
    // Synchronous prompt variant (only consumer: gitApi small-model requests).
    const body = await readJsonBody(request);
    const directory = body.directory ?? projectDirectory(ctx);
    const payload = promptPayloadFromWire(body);
    const wire = await engine.prompt({
      sessionID: ctx.params.sessionID,
      directory,
      text: payload.text,
      model: body.model,
      agent: body.agent,
      images: payload.images,
      messageID: payload.messageID,
    });
    if (!wire) return notFound('session not found');
    const messages = await engine.getMessages({ sessionID: ctx.params.sessionID, directory });
    return json(messages ?? [wire]);
  });
  route('POST', '/session/{sessionID}/prompt_async', async (request, ctx) => {
    const body = await readJsonBody(request);
    const directory = body.directory ?? projectDirectory(ctx);
    const payload = promptPayloadFromWire(body);
    const wire = await engine.prompt({
      sessionID: ctx.params.sessionID,
      directory,
      text: payload.text,
      model: body.model,
      agent: body.agent,
      images: payload.images,
      messageID: payload.messageID,
      delivery: body.delivery,
    });
    if (!wire) return notFound('session not found');
    return json(wire.info);
  });
  route('POST', '/session/{sessionID}/command', async (request, ctx) => {
    const body = await readJsonBody(request);
    const directory = body.directory ?? projectDirectory(ctx);
    // Slash-command execution: forward the command text as a prompt; omp
    // expands its own slash commands when the session is materialized.
    const wire = await engine.prompt({
      sessionID: ctx.params.sessionID,
      directory,
      text: body.command ?? '',
      model: body.model,
      agent: body.agent,
    });
    if (!wire) return notFound('session not found');
    return json(wire.info);
  });
  route('POST', '/session/{sessionID}/abort', async (request, ctx) => {
    const body = await readJsonBody(request);
    const directory = body.directory ?? projectDirectory(ctx);
    await engine.abort({ sessionID: ctx.params.sessionID, directory });
    return json({});
  });
  route('POST', '/session/{sessionID}/shell', async (request, ctx) => {
    return unsupported('Interactive session shells are not exposed by the omp engine.');
  });
  route('POST', '/session/{sessionID}/revert', async (request, ctx) => {
    const body = await readJsonBody(request);
    const directory = body.directory ?? projectDirectory(ctx);
    const session = await engine.revert({
      sessionID: ctx.params.sessionID,
      directory,
      messageID: body.messageID,
    });
    return session ? json(session) : notFound('session not found');
  });
  route('POST', '/session/{sessionID}/unrevert', async (request, ctx) => {
    const body = await readJsonBody(request);
    const directory = body.directory ?? projectDirectory(ctx);
    const session = await engine.unrevert({ sessionID: ctx.params.sessionID, directory });
    return session ? json(session) : notFound('session not found');
  });
  route('POST', '/session/{sessionID}/summarize', async (request, ctx) => {
    const body = await readJsonBody(request);
    const directory = body.directory ?? projectDirectory(ctx);
    await engine.summarize({ sessionID: ctx.params.sessionID, directory });
    const session = await engine.getSession({ sessionID: ctx.params.sessionID, directory });
    return json(session ?? {});
  });
  route('POST', '/session/{sessionID}/fork', async (request, ctx) => {
    const body = await readJsonBody(request);
    const directory = body.directory ?? projectDirectory(ctx);
    const session = await engine.fork({ sessionID: ctx.params.sessionID, directory });
    return session ? json(session) : notFound('session not found');
  });
  route('POST', '/session/{sessionID}/share', async () =>
    unsupported('Sharing sessions publicly is an OpenCode cloud feature with no omp equivalent.'),
  );
  route('DELETE', '/session/{sessionID}/share', async () =>
    unsupported('Sharing sessions publicly is an OpenCode cloud feature with no omp equivalent.'),
  );
  route('GET', '/session/{sessionID}/diff', async () => json([]));

  // ---- permissions / questions ----
  // The omp engine runs tools with its own approval policy; OpenChamber's
  // permission/question protocol has no live producer yet, so these answer
  // authoritatively empty until the approval bridge lands.
  route('GET', '/permission', async () => json([]));
  route('POST', '/permission/{requestID}/reply', async () => json({}));
  route('POST', '/api/session/{sessionID}/permission', async () => json({ id: '', effect: 'deny' }));
  route('GET', '/api/session/{sessionID}/permission/{requestID}', async () => notFound('no pending permission'));
  route('GET', '/question', async () => json([]));
  route('POST', '/question/{requestID}/reply', async () => json({}));
  route('POST', '/question/{requestID}/reject', async () => json({}));

  // ---- config / app / commands / tools ----
  route('GET', '/config', async () => json(await configPayload()));
  route('PATCH', '/config', async (request) => {
    const body = await readJsonBody(request);
    if (Array.isArray(body.agents)) {
      for (const agent of body.agents) {
        if (agent && typeof agent.name === 'string') engine.upsertCustomAgent(agent);
      }
    }
    return json(await configPayload());
  });
  route('GET', '/config/providers', async () => {
    const providers = await providersPayload();
    return json({ providers, default: providers[0]?.id ?? '' });
  });
  route('GET', '/agent', async () => {
    await engine.ready();
    const builtin = [
      { name: 'build', description: 'General purpose coding agent', mode: 'primary', builtIn: true },
      { name: 'plan', description: 'Planning agent (read-only analysis before execution)', mode: 'primary', builtIn: true },
    ];
    const custom = [...engine.customAgents.values()].map((agent) => ({
      name: agent.name,
      description: agent.description ?? agent.name,
      mode: 'subagent',
      builtIn: false,
      prompt: agent.prompt,
      ...(Array.isArray(agent.tools) ? { tools: agent.tools } : {}),
    }));
    return json([...builtin, ...custom]);
  });
  route('GET', '/skill', async (request, ctx) => {
    const directory = projectDirectory(ctx);
    try {
      const { discoverSkills } = await import('@oh-my-pi/pi-coding-agent');
      const { skills } = await discoverSkills(directory);
      return json(
        (skills ?? []).map((skill) => ({
          name: skill.name,
          description: skill.description ?? '',
          path: skill.path ?? '',
        })),
      );
    } catch {
      return json([]);
    }
  });
  route('GET', '/command', async () => json([]));
  route('GET', '/experimental/tool/ids', async () => json(BUILTIN_TOOLS ?? []));
  route('GET', '/lsp', async () => json({ servers: {} }));
  route('GET', '/formatter', async () => json({}));

  // ---- providers / auth ----
  route('GET', '/provider', async () => json(await providersPayload()));
  route('GET', '/provider/auth', async () => {
    const providers = await providersPayload();
    const models = engine.availableModels();
    const authenticated = new Set(models.map((model) => model.provider));
    const withAuth = providers.map((provider) => ({
      ...provider,
      auth: authenticated.has(provider.id),
    }));
    return json({ providers: withAuth.filter((p) => p.auth), agent: '' });
  });
  route('POST', '/provider/{providerID}/oauth/authorize', async () =>
    unsupported('Provider OAuth flows run through the omp CLI login, not the host API.'),
  );
  route('POST', '/provider/{providerID}/oauth/callback', async () =>
    unsupported('Provider OAuth flows run through the omp CLI login, not the host API.'),
  );
  route('PUT', '/auth/{providerID}', async () =>
    unsupported('API keys are managed by the omp engine credential store; run `omp` login flows.'),
  );
  route('DELETE', '/auth/{providerID}', async () =>
    unsupported('API keys are managed by the omp engine credential store; run `omp` logout.'),
  );

  // ---- mcp ----
  route('GET', '/mcp', async () => json({ servers: {} }));
  route('POST', '/mcp/{name}/connect', async () => json({}));
  route('POST', '/mcp/{name}/disconnect', async () => json({}));
  route('POST', '/mcp/{name}/auth', async () => unsupported('MCP OAuth is not bridged through the host yet.'));
  route('POST', '/mcp/{name}/auth/authenticate', async () => unsupported('MCP OAuth is not bridged through the host yet.'));
  route('POST', '/mcp/{name}/auth/callback', async () => unsupported('MCP OAuth is not bridged through the host yet.'));
  route('DELETE', '/mcp/{name}/auth', async () => json({}));

  // ---- path / project / vcs / file / find ----
  route('GET', '/path', async (request, ctx) => {
    const directory = projectDirectory(ctx);
    const gitRoot = await git(directory, 'rev-parse', '--show-toplevel');
    const branch = gitRoot ? await git(directory, 'branch', '--show-current') : null;
    return json({
      cwd: directory,
      git: gitRoot
        ? {
            branch: branch || 'HEAD',
            detached: !branch,
            created: Date.now(),
          }
        : undefined,
    });
  });
  route('GET', '/project', async (request, ctx) => {
    const directory = projectDirectory(ctx);
    return json([
      {
        id: engine.projectIdFor(directory),
        directory,
        name: path.basename(directory) || directory,
        worktree: false,
      },
    ]);
  });
  route('GET', '/project/current', async (request, ctx) => {
    const directory = projectDirectory(ctx);
    return json({
      id: engine.projectIdFor(directory),
      directory,
      name: path.basename(directory) || directory,
      worktree: false,
    });
  });
  route('GET', '/vcs', async (request, ctx) => {
    const directory = projectDirectory(ctx);
    const branch = await git(directory, 'branch', '--show-current');
    return json(branch ? { branch } : {});
  });
  route('GET', '/file', async (request, ctx) => {
    const directory = projectDirectory(ctx);
    try {
      const entries = fs.readdirSync(directory, { withFileTypes: true });
      return json(
        entries
          .filter((entry) => !entry.name.startsWith('.'))
          .map((entry) => ({
            name: entry.name,
            path: path.join(directory, entry.name).replaceAll('\\', '/'),
            type: entry.isDirectory() ? 'directory' : 'file',
            absolute: path.join(directory, entry.name),
          })),
      );
    } catch {
      return json([]);
    }
  });
  route('GET', '/file/content', async (request, ctx) => {
    const filePath = ctx.url.searchParams.get('path');
    if (!filePath) return badRequest('path is required');
    try {
      const content = fs.readFileSync(filePath, 'utf8');
      return json({ type: 'file', content });
    } catch {
      return notFound('file not found');
    }
  });
  route('GET', '/file/status', async () => json({ entries: {} }));
  route('GET', '/find/file', async (request, ctx) => {
    const query = (ctx.url.searchParams.get('pattern') ?? ctx.url.searchParams.get('query') ?? '').toLowerCase();
    const directory = projectDirectory(ctx);
    if (!query) return json({ hits: [], total: 0, processing: false });
    const hits = [];
    const walk = (dir, depth) => {
      if (depth > 6 || hits.length >= 50) return;
      let entries;
      try {
        entries = fs.readdirSync(dir, { withFileTypes: true });
      } catch {
        return;
      }
      for (const entry of entries) {
        if (entry.name.startsWith('.') || entry.name === 'node_modules') continue;
        const full = path.join(dir, entry.name);
        if (entry.name.toLowerCase().includes(query)) {
          hits.push({
            path: full.replaceAll('\\', '/'),
            name: entry.name,
            type: entry.isDirectory() ? 'directory' : 'file',
            score: 1,
          });
          if (hits.length >= 50) return;
        }
        if (entry.isDirectory()) walk(full, depth + 1);
      }
    };
    walk(directory, 0);
    return json({ hits, total: hits.length, processing: false });
  });

  // ---- experimental ----
  route('GET', '/experimental/session', async (request, ctx) => {
    const archived = ctx.url.searchParams.get('archived');
    const limit = Number(ctx.url.searchParams.get('limit') ?? 500);
    const byDirectory = await engine.listAllSessions({ archived: archived === 'false' ? false : undefined });
    const all = [...byDirectory.values()].flat();
    all.sort((a, b) => (b.time?.updated ?? 0) - (a.time?.updated ?? 0));
    const page = Number.isFinite(limit) && limit > 0 ? all.slice(0, limit) : all;
    return json(page);
  });
  route('POST', '/experimental/control-plane/move-session', async (request) => {
    const body = await readJsonBody(request);
    const sessionID = body.sessionID;
    const destination = body.destination?.directory;
    if (!sessionID || !destination) return badRequest('sessionID and destination.directory are required');
    const moved = await engine.moveSession({ sessionID, destination });
    return json(moved ?? {});
  });
  // ---- domain modules (specs 01/02/03/04/06; public /api/omp/*) ----
  const ompPublish = (type, payload, scope) => engine.ompBus.publish(type, payload, scope);
  registerModelSettingsRoutes(route, {
    store: {
      settingsFor: async (directory) => {
        const store = await engine.settingsStoreReady();
        return store.settingsFor(directory);
      },
      getRevision: () => engine.settingsStore?.getRevision?.() ?? 0,
      bumpRevision: () => engine.settingsStore?.bumpRevision?.() ?? 0,
      chainWrites: (targetKey, task) => {
        const store = engine.settingsStore;
        return store ? store.chainWrites(targetKey, task) : Promise.resolve();
      },
      get boot() {
        return engine.settingsStore?.boot;
      },
      get bootDirectory() {
        return engine.settingsStore?.bootDirectory;
      },
    },
    publish: ompPublish,
    listModels: async () => {
      await engine.ready();
      return engine.availableModels();
    },
  });
  engine.dialogs.mount(route);
  registerModesDomainRoutes(route, engine.modesDomain, { features: ompFeatures() });
  engine.uriDomain.mount(route);
  registerCommandsDomainRoutes(route, { features: ompFeatures(), liveCommandsFor: (directory) => engine.liveCommandsFor(directory) });
  registerChromeDomainRoutes(route, { chrome: engine.chrome, features: ompFeatures() });

  // ---- omp parity foundation (spec docs/omp-parity; public paths
  // /api/omp/* — the web proxy strips the /api prefix, master D6-R3/R4) ----
  // Profile-scoped omp agent dir (spec 07 §5.13). The web server cannot
  // import the SDK (it runs under Node; the SDK is Bun/TS-only), so this is
  // the authoritative resolution point.
  route('GET', '/agent-dir', async () => json({ agentDir: getAgentDir() }));
  route('GET', '/omp/capabilities', async () => json(buildCapabilities()));

  route('GET', '/omp/sessions/{id}/custom-messages', async (request, ctx) => {
    const directory = directoryFromRequest({ url: new URL(request.url), headers: request.headers });
    if (!directory) return badRequest('directory is required');
    const messages = await engine.getCustomMessages({
      sessionID: ctx.params.id,
      directory,
    });
    return messages ? json(messages) : notFound('session not found');
  });

  route('GET', '/omp/sessions/{id}/telemetry', async (request, ctx) => {
    const directory = directoryFromRequest({ url: new URL(request.url), headers: request.headers });
    if (!directory) return badRequest('directory is required');
    const telemetry = await engine.getTelemetry({ sessionID: ctx.params.id, directory });
    return telemetry ? json(telemetry) : notFound('session not found');
  });

  route('GET', '/omp/sessions/{id}/entries', async (request, ctx) => {
    const url = new URL(request.url);
    const directory = directoryFromRequest({ url, headers: request.headers });
    if (!directory) return badRequest('directory is required');
    const kinds = url.searchParams.get('kinds') ?? '';
    const entries = await engine.getEntries({
      sessionID: ctx.params.id,
      directory,
      kinds,
    });
    return entries ? json(entries) : notFound('session not found');
  });

  route('POST', '/omp/sessions/{id}/model', async (request, ctx) => {
    if (ompFeatures()['modelRoles.v1'] !== true) {
      return featureUnavailable('modelRoles.v1');
    }
    const directory = directoryFromRequest({ url: new URL(request.url), headers: request.headers });
    if (!directory) return badRequest('directory is required');
    const body = await request.json().catch(() => ({}));
    const result = await engine.setSessionModel({
      sessionID: ctx.params.id,
      directory,
      model: body?.model && typeof body.model === 'object' ? body.model : null,
      ...(typeof body?.thinkingLevel === 'string' && body.thinkingLevel.length > 0
        ? { thinkingLevel: body.thinkingLevel }
        : {}),
    });
    if (!result.ok) return badRequest(result.error);
    return json(result);
  });

  route('GET', '/omp/events', async (request) => {
    // Single omp-native event channel (05 §5.2.1, master R1). Same frame
    // format as the wire SSE; Last-Event-ID resumes durable entries only,
    // with an omp.stream.resync control frame first when the ring can't
    // bridge the gap (断流不是空状态, master D2).
    const url = new URL(request.url);
    const rawDirectory = url.searchParams.get('directory');
    const directory = rawDirectory ? normalizeDirectoryKey(rawDirectory) : null;
    const lastEventId = Number(request.headers.get('last-event-id') ?? 0) || 0;
    let closed = false;
    const stream = new ReadableStream({
      start(controller) {
        const encoder = new TextEncoder();
        const send = (text) => {
          if (closed) return;
          try {
            controller.enqueue(encoder.encode(text));
          } catch {
            closed = true;
          }
        };
        send(':ok\n\n');
        const bus = engine.ompBus;
        const state = bus.replayState(lastEventId);
        if (state.status !== 'ok') {
          const resyncId = bus.nextEventId;
          const envelope = {
            id: resyncId,
            type: 'omp.stream.resync',
            directory: directory ?? '',
            schemaVersion: bus.schemaVersion,
            createdAt: Date.now(),
            payload: {
              scope: ['sessions', 'modes', 'model', 'dialogs', 'chrome', 'settings', 'agents', 'jobs', 'queue', 'tree', 'transcript'],
              lastEventId,
            },
          };
          send(`event: omp.stream.resync\ndata: ${JSON.stringify(envelope)}\n\n`);
        }
        const unsubscribe = bus.subscribeSince(
          lastEventId,
          (entry) => {
            send(`id: ${entry.eventId}\nevent: ${entry.envelope.type}\ndata: ${JSON.stringify(entry.envelope)}\n\n`);
          },
          directory ? { directory } : {},
        );
        const heartbeat = setInterval(() => send(':heartbeat\n\n'), 15000);
        request.signal.addEventListener('abort', () => {
          closed = true;
          clearInterval(heartbeat);
          unsubscribe();
          try {
            controller.close();
          } catch {
            // Already closed.
          }
        });
      },
    });
    return new Response(stream, sseResponseInit());
  });

  // ---- SSE ----
  const sseHandler = (request, { global }) => {
    const directory = global ? null : directoryFromRequest({ url: new URL(request.url), headers: request.headers });
    const lastEventId = Number(request.headers.get('last-event-id') ?? 0) || 0;
    let closed = false;
    const stream = new ReadableStream({
      start(controller) {
        const encoder = new TextEncoder();
        const send = (text) => {
          if (closed) return;
          try {
            controller.enqueue(encoder.encode(text));
          } catch {
            closed = true;
          }
        };
        send(':ok\n\n');
        const unsubscribe = engine.bus.subscribeSince(
          lastEventId,
          (entry) => {
            send(`id: ${entry.eventId}\nevent: ${entry.envelope.type}\ndata: ${JSON.stringify(entry.envelope)}\n\n`);
          },
          directory ? { directory } : {},
        );
        const heartbeat = setInterval(() => send(':heartbeat\n\n'), 15000);
        request.signal.addEventListener('abort', () => {
          closed = true;
          clearInterval(heartbeat);
          unsubscribe();
          try {
            controller.close();
          } catch {
            // Already closed.
          }
        });
      },
    });
    return new Response(stream, sseResponseInit());
  };

  return { sseHandler };
};
