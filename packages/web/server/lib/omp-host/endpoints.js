// OpenCode-compatible endpoint implementations for the omp host.
//
// Every route here exists because OpenChamber's vendored wire client calls it
// (see packages/ui/src/lib/opencode/wire). Features with no omp equivalent
// respond with stable, explicit errors instead of pretending success.

import fs from 'node:fs';
import path from 'node:path';
import { execFile } from 'node:child_process';
import { promisify } from 'node:util';
import { BUILTIN_TOOLS } from '@oh-my-pi/pi-coding-agent';
import { normalizeDirectoryKey } from './registry.js';

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
      // omp keeps its own model/provider config (~/.omp/agent/config.yml); the
      // wire config exposes the model default when one resolves.
      ...(engine.availableModels()[0]
        ? {
            model: `${engine.availableModels()[0].provider}/${engine.availableModels()[0].id}`,
          }
        : {}),
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
  route('POST', '/global/upgrade', async () => unsupported('The omp host upgrades with the OpenChamber application, not through the API.'));

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
    const limitParam = ctx.url.searchParams.get('limit');
    const messages = await engine.getMessages({
      sessionID: ctx.params.sessionID,
      directory,
      limit: limitParam ? Number(limitParam) : undefined,
    });
    return messages ? json(messages) : notFound('session not found');
  });
  route('POST', '/session/{sessionID}/message', async (request, ctx) => {
    // Synchronous prompt variant (only consumer: gitApi small-model requests).
    const body = await readJsonBody(request);
    const directory = body.directory ?? projectDirectory(ctx);
    const wire = await engine.prompt({
      sessionID: ctx.params.sessionID,
      directory,
      text: body.prompt?.text ?? '',
      model: body.model,
      agent: body.agent,
      images: body.prompt?.files
        ?.filter((f) => typeof f?.data === 'string')
        .map((f) => ({ data: f.data, mimeType: f.mime })),
    });
    if (!wire) return notFound('session not found');
    const messages = await engine.getMessages({ sessionID: ctx.params.sessionID, directory });
    return json(messages ?? [wire]);
  });
  route('POST', '/session/{sessionID}/prompt_async', async (request, ctx) => {
    const body = await readJsonBody(request);
    const directory = body.directory ?? projectDirectory(ctx);
    const wire = await engine.prompt({
      sessionID: ctx.params.sessionID,
      directory,
      text: body.prompt?.text ?? '',
      model: body.model,
      agent: body.agent,
      images: body.prompt?.files
        ?.filter((f) => typeof f?.data === 'string')
        .map((f) => ({ data: f.data, mimeType: f.mime })),
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
