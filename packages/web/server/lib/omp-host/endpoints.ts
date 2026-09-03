// OpenCode-compatible endpoint implementations for the omp host.
//
// Every route here exists because OMPChamber's vendored wire client calls it
// (see packages/ui/src/lib/opencode/wire). Features with no omp equivalent
// respond with stable, explicit errors instead of pretending success.

import fs from 'node:fs';
import path from 'node:path';
import { execFile } from 'node:child_process';
import { promisify } from 'node:util';
import { BUILTIN_TOOLS, getAgentDir } from '@oh-my-pi/pi-coding-agent';
import { normalizeDirectoryKey } from './registry.ts';
import type { SessionMetadataValue } from './registry.ts';
import { buildCapabilities, featureUnavailable, ompFeatures } from './omp-parity.ts';
import { registerModelSettingsRoutes, buildModelsPayload } from './domain-models.ts';
import { ModeDomainError, registerModesDomainRoutes } from './domain-modes.ts';
import { registerCommandsDomainRoutes } from './domain-commands.ts';
import { registerChromeDomainRoutes } from './domain-chrome.ts';
import { registerPluginsDomainRoutes } from './domain-plugins.ts';
import { registerProvidersDomainRoutes } from './domain-providers.ts';
import type { Settings, Skill } from '@oh-my-pi/pi-coding-agent';
import type { OmpHostEngine } from './engine.ts';
import type { PublishFn, SettingsStore } from './domain-models.ts';
import type { ProjectedMessage } from './projection.ts';


/**
 * Engine face defaultModelPointer consumes: just the lazy settings store
 * (structural — the parity tests inject a one-method stub).
 */
export interface DefaultModelPointerEngine {
  settingsStoreReady: () => Promise<{ settingsFor: (directory?: string) => Promise<Settings> } | null>;
}

/** Wire default-model pointer: `{ model: 'provider/id' }` or `{}`. */
export interface DefaultModelPointer {
  model?: string;
}

/**
 * Resolve the wire config's default-model pointer from modelRoles.default
 * through the keyed Settings instance (spec 01 §5.3/GAP-03). Falls back to
 * omitting the key when no role default resolves — never pins the
 * alphabetically-first provider model.
 */
export const defaultModelPointer = async (engine: DefaultModelPointerEngine): Promise<DefaultModelPointer> => {
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
/** What `Response.json` itself accepts — this helper only forwards to it. */
type ResponseJsonData = Parameters<typeof Response.json>[0];

const json = (data: ResponseJsonData, init?: ResponseInit): Response => Response.json(data, init);

const execFileAsync = promisify(execFile);

// Domain errors from the prompt path (e.g. persona-not-found 404, 02 §5.1
// R2-M3) answer with their own status; anything else stays a 500 in host.js.
const domainErrorsToResponse = async <T>(run: () => Promise<T>): Promise<T | Response> => {
  try {
    return await run();
  } catch (error) {
    if (error instanceof ModeDomainError) return json(error.body, { status: error.status });
    throw error;
  }
};
const notFound = (message: string): Response =>
  json({ name: 'UnknownError', data: { message } }, { status: 404 });

const badRequest = (message: string): Response =>
  json({ name: 'UnknownError', data: { message } }, { status: 400 });

const unsupported = (message: string): Response =>
  json({ name: 'UnknownError', data: { message } }, { status: 501 });

// Wire JSON body parse (same parse-time assertion convention as the domain
// modules — domain-modes.ts readJsonBody): the call-site type parameter is
// the contract; every field is runtime-validated by the handler.
const readJsonBody = async <T extends object>(request: Request): Promise<T> => {
  try {
    // SAFETY: parse-time assertion convention — the call-site type parameter
    // is the contract; every field is runtime-validated by the handler
    // before use.
    return (await request.json()) as T;
  } catch {
    // SAFETY: a malformed body answers as `{}`, which satisfies the
    // `T extends object` bound; every handler treats missing fields as
    // absent.
    return {} as T;
  }
};

/**
 * Wire agent-mention source span (the generated AgentPart.source shape):
 * the mention text plus the character offsets it occupies.
 */
export interface WirePromptPartSource {
  value?: string;
  start?: number;
  end?: number;
}

/**
 * One wire prompt part (SessionPromptAsyncData superset: text/file/agent/
 * subtask variants, plus the span fields that ride along with agent
 * mentions). Fields are runtime-validated per variant before use.
 */
export interface WirePromptPart {
  type?: unknown;
  text?: unknown;
  url?: unknown;
  name?: unknown;
  mime?: string;
  prompt?: unknown;
  source?: WirePromptPartSource | null;
}

/**
 * Prompt-endpoint JSON body: the parts-based SessionPromptAsyncData shape
 * plus the legacy `{ prompt: { text, files } }` form.
 */
export interface WirePromptBody {
  messageID?: unknown;
  parts?: WirePromptPart[] | null;
  prompt?: { text?: string; files?: { data?: unknown; mime?: string }[] } | null;
}

/** Base64 image input the engine's prompt consumes. */
export interface PromptImage {
  data: string;
  mimeType: string | undefined;
}

/** Prompt inputs parsed from a wire body (promptPayloadFromWire's return). */
export interface PromptPayload {
  text: string;
  images: PromptImage[];
  messageID: string | undefined;
}

/**
 * Parse a prompt request body into the engine's prompt inputs.
 *
 * The wire contract sends `{ parts: [text|file|agent|subtask], messageID }`
 * (see SessionPromptAsyncData in the vendored types). The legacy
 * `{ prompt: { text, files } }` shape is still accepted for the synchronous
 * `/message` consumer. Dropping `parts` here persisted every user message
 * with an empty part list, which the UI hides after a reload.
 */
export const promptPayloadFromWire = (body: WirePromptBody): PromptPayload => {
  const messageID = typeof body?.messageID === 'string' && body.messageID ? body.messageID : undefined;
  const parts = Array.isArray(body?.parts) ? body.parts : [];
  if (parts.length === 0) {
    return {
      text: body?.prompt?.text ?? '',
      images: (body?.prompt?.files ?? [])
        .filter((file): file is { data: string; mime?: string } => typeof file?.data === 'string')
        .map((file) => ({ data: file.data, mimeType: file.mime })),
      messageID,
    };
  }
  const texts: string[] = [];
  const images: PromptImage[] = [];
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

const directoryFromRequest = ({ url, headers }: { url?: URL; headers?: Headers }): string | null => {
  const fromQuery = url?.searchParams.get('directory') ?? url?.searchParams.get('location[directory]');
  const fromHeader = headers?.get('x-opencode-directory');
  const raw = fromQuery ?? (fromHeader ? decodeURIComponent(fromHeader) : null);
  return raw ? normalizeDirectoryKey(raw) : null;
};

const git = async (cwd: string, ...args: string[]): Promise<string | null> => {
  try {
    const { stdout } = await execFileAsync('git', ['-C', cwd, ...args], { windowsHide: true });
    return stdout.trim();
  } catch {
    return null;
  }
};

/** SSE response init: streaming headers Bun keeps open past its idle cap. */
const sseResponseInit = (): ResponseInit => ({
  status: 200,
  headers: {
    'content-type': 'text/event-stream',
    'cache-control': 'no-cache',
    connection: 'keep-alive',
    'x-accel-buffering': 'no',
  },
});

/**
 * Per-request route context host.ts dispatches into every registered
 * handler (host.ts fetch: handler(request, { params, url, headers, engine })).
 */
export interface RouteContext {
  params: Record<string, string>;
  url: URL;
  headers: Headers;
  engine: OmpHostEngine;
}

/**
 * omp-host route handler (Basic auth is enforced by host.ts outside these).
 * `Promise<void>` covers the legacy `/session/{id}/command` handler, which
 * resolves without a Response exactly as it did pre-rename; host.ts answers
 * those requests at the Bun.serve boundary.
 */
export type RouteHandler = (
  request: Request,
  ctx: RouteContext,
) => Response | Promise<Response | void>;

/** `route(method, pattern, handler)` registration callback (host.ts). */
export type RouteMount = (method: string, pattern: string, handler: RouteHandler) => void;

/** SSE upgrade handler host.ts routes /event and /global/event into. */
export type SseHandler = (request: Request, options: { global: boolean }) => Response;

/** Context host.ts supplies to registerEndpoints. */
export interface EndpointOptions {
  /** Host version echoed by /global/config and /global/health. */
  version: string;
  /** Host module directory (host.ts __dirname; asset resolution root).
   * Optional: registerEndpoints itself consumes only `version`. */
  dirname?: string;
}

/** JSON body of POST /session (session create). */
export interface SessionInitBody {
  directory?: string;
  title?: string;
  parentID?: string;
}
/**
 * Session metadata values: arbitrary JSON the wire client round-trips
 * opaquely (the host stores and returns them verbatim, never reading
 * individual entries).
 */
export type { SessionMetadataValue };

/** JSON body of PATCH /session/{sessionID}. */
export interface SessionUpdateBody {
  directory?: string;
  title?: string;
  metadata?: Record<string, SessionMetadataValue>;
  time?: { archived?: number };
}

/** JSON body of the prompt-family routes (message, prompt_async, command). */
export interface SessionPromptBody extends WirePromptBody {
  directory?: string;
  model?: { providerID?: string; modelID?: string };
  agent?: string;
  delivery?: string;
  command?: string;
}

/** JSON body of POST /session/{sessionID}/revert. */
export interface SessionRevertBody {
  directory?: string;
  messageID?: string;
}

/** JSON body of POST /experimental/control-plane/move-session. */
export interface MoveSessionBody {
  sessionID?: string;
  destination?: { directory?: string };
}

/** JSON body of the directory-scoped session POST routes (unrevert, summarize, fork). */
export interface SessionDirectoryBody {
  directory?: string;
  /** Fork boundary (wire message id): present bounds the fork at that
   * message (TUI /branch); absent forks the whole transcript (TUI /fork). */
  messageID?: string;
}

/** JSON body of POST /omp/sessions/{id}/model (spec 01 GAP-02/04). */
export interface SessionModelBody {
  model?: unknown;
  thinkingLevel?: string;
}


/** Wire app/skills row (AppSkillsResponses 200 element, types.gen.d.ts). */
export interface WireSkillRow {
  name: string;
  description: string;
  location: string;
  content: string;
}

/**
 * Strip a leading YAML frontmatter block (`---\n...\n---\n`) from a SKILL.md
 * file. Mirrors the SDK's own display-layer semantics (extensibility/skills.ts
 * buildSkillPromptMessage strips the same block; discovery/helpers.ts fills
 * `content` with the parseFrontmatter body). A file without frontmatter is
 * returned verbatim.
 */
const stripSkillFrontmatter = (content: string): string =>
  content.replace(/^---\r?\n[\s\S]*?\r?\n---\r?\n/, '');

/**
 * Project discovered SDK skills onto the wire app/skills rows the vendored
 * client consumes: `location` is the SKILL.md path (the wire field name —
 * the SDK calls it `filePath`) and `content` is the frontmatter-stripped
 * body. An unreadable SKILL.md degrades to empty content rather than
 * dropping the row: discovery just scanned it, so one vanished file must
 * not hide the surviving skills behind the route-wide `[]` fallback.
 */
export const wireSkillRows = async (skills: readonly Pick<Skill, 'name' | 'description' | 'filePath'>[]): Promise<WireSkillRow[]> =>
  Promise.all(
    skills.map(async (skill) => ({
      name: skill.name,
      description: skill.description ?? '',
      location: skill.filePath,
      content: await fs.promises.readFile(skill.filePath, 'utf8').then(
        stripSkillFrontmatter,
        () => '',
      ),
    })),
  );

/**
 * Register every consumed route.
 * @param {(method: string, pattern: string, handler: Function) => void} route
 */
export const registerEndpoints = (route: RouteMount, engine: OmpHostEngine, { version }: EndpointOptions) => {
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
      // SAFETY: wire env list is empty in the omp payload (no env passthrough yet).
      env: [] as string[],
      models: list.map((model: { id: string; name?: string; reasoning?: boolean; contextWindow?: number; maxTokens?: number }) => ({
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
      // Agent definitions are the omp discovery chain (02 §5.2); the legacy
      // wire payload carries no custom-agent list anymore.
      // SAFETY: wire payload carries no custom-agent list anymore (comment above).
      agents: [] as never[],
      // OpenCode-specific keys with no omp equivalent are absent.
    };
  };

  const projectDirectory = (requestContext: { url?: URL }) =>
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
    const body = await readJsonBody<SessionInitBody>(request);
    const directory = body.directory ?? projectDirectory(ctx);
    const session = await engine.createSession({
      directory,
      title: body.title,
      parentID: body.parentID,
      ...(body.parentID ? {} : {}),
      // createSession destructures the full wire shape; absent fields are
      // explicitly undefined (identical to missing keys at runtime).
      agent: undefined,
      model: undefined,
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
    const body = await readJsonBody<SessionUpdateBody>(request);
    const directory = body.directory ?? projectDirectory(ctx);
    const session = await engine.updateSession({
      sessionID: ctx.params.sessionID,
      directory,
      title: body.title,
      metadata: body.metadata,
      timeArchived: body.time?.archived,
    });
    return session ? json(session) : notFound('session not found');
  });
  route('DELETE', '/session/{sessionID}', async (request, ctx) => {
    const url = ctx.url;
    const directory = directoryFromRequest(ctx) ?? url.searchParams.get('directory') ?? process.cwd();
    await engine.deleteSession({ sessionID: ctx.params.sessionID, directory });
    return json(true);
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
    const body = await readJsonBody<SessionPromptBody>(request);
    const directory = body.directory ?? projectDirectory(ctx);
    const payload = promptPayloadFromWire(body);
    const wire = await domainErrorsToResponse(() => engine.prompt({
      sessionID: ctx.params.sessionID,
      directory,
      text: payload.text,
      model: body.model,
      agent: body.agent,
      images: payload.images,
      messageID: payload.messageID,
      delivery: undefined,
    }));
    if (!wire) return notFound('session not found');
    const messages = await engine.getMessages({ sessionID: ctx.params.sessionID, directory });
    return json(messages ?? [wire]);
  });
  route('POST', '/session/{sessionID}/prompt_async', async (request, ctx) => {
    const body = await readJsonBody<SessionPromptBody>(request);
    const directory = body.directory ?? projectDirectory(ctx);
    const payload = promptPayloadFromWire(body);
    // engine.prompt's return is circularly inferred (it recurses for the
    // persona switch), so generic inference degrades to unknown — pin the
    // documented wire contract instead.
    const wire = await domainErrorsToResponse((): Promise<ProjectedMessage | null> => engine.prompt({
      sessionID: ctx.params.sessionID,
      directory,
      text: payload.text,
      model: body.model,
      agent: body.agent,
      images: payload.images,
      messageID: payload.messageID,
      delivery: body.delivery,
    }));
    if (!wire) return notFound('session not found');
    // Domain-error answers arrive as the Response itself; `info` narrows to
    // the payload branch (identical value to the historical untyped read).
    return json('info' in wire ? wire.info : undefined);
  });
  route('POST', '/session/{sessionID}/command', async (request, ctx) => {
    const body = await readJsonBody<SessionPromptBody>(request);
    const directory = body.directory ?? projectDirectory(ctx);
    // Slash-command execution: forward the command text as a prompt; omp
    // expands its own slash commands when the session is materialized.
    const wire = await domainErrorsToResponse(() => engine.prompt({
      sessionID: ctx.params.sessionID,
      directory,
      text: body.command ?? '',
      model: body.model,
      agent: body.agent,
      images: undefined,
      delivery: undefined,
      messageID: undefined,
    }));
  });
  route('POST', '/session/{sessionID}/abort', async (request, ctx) => {
    // Wire stop control (UI abortCurrentOperation, Esc shortcut, mobile
    // pill): 200 boolean per the vendored contract. `directory` rides the
    // query string / directory header; engine.abort resolves the live
    // session by ID (sessions are keyed by sessionID, not directory).
    const directory = directoryFromRequest(ctx);
    return json(await engine.abort({ sessionID: ctx.params.sessionID, directory: directory ?? undefined }));
  });
  route('POST', '/session/{sessionID}/shell', async (request, ctx) => {
    return unsupported('Interactive session shells are not exposed by the omp engine.');
  });
  route('POST', '/session/{sessionID}/revert', async (request, ctx) => {
    const body = await readJsonBody<SessionRevertBody>(request);
    if (!body?.messageID) return badRequest('messageID is required');
    const directory = body.directory ?? projectDirectory(ctx);
    const session = await engine.revert({
      sessionID: ctx.params.sessionID,
      directory,
      messageID: body.messageID,
    });
    return session ? json(session) : notFound('session not found');
  });
  route('POST', '/session/{sessionID}/unrevert', async (request, ctx) => {
    const body = await readJsonBody<SessionDirectoryBody>(request);
    const directory = body.directory ?? projectDirectory(ctx);
    const session = await engine.unrevert({ sessionID: ctx.params.sessionID, directory });
    return session ? json(session) : notFound('session not found');
  });
  route('POST', '/session/{sessionID}/summarize', async (request, ctx) => {
    const body = await readJsonBody<SessionDirectoryBody>(request);
    const directory = body.directory ?? projectDirectory(ctx);
    await engine.summarize({ sessionID: ctx.params.sessionID, directory });
    const session = await engine.getSession({ sessionID: ctx.params.sessionID, directory });
    return json(session ?? {});
  });
  route('POST', '/session/{sessionID}/fork', async (request, ctx) => {
    const body = await readJsonBody<SessionDirectoryBody>(request);
    const directory = body.directory ?? projectDirectory(ctx);
    // messageID (wire contract) bounds the fork at that message — TUI /branch
    // semantics. Absent → whole-transcript fork (TUI /fork semantics).
    const session = await engine.fork({
      sessionID: ctx.params.sessionID,
      directory,
      ...(typeof body.messageID === 'string' && body.messageID ? { messageID: body.messageID } : {}),
    });
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
  // The omp engine runs tools with its own approval policy; OMPChamber's
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
  route('PATCH', '/config', async () => {
    // The agents write branch is gone (02 §5.8): agent definitions are the
    // omp discovery chain, managed via /api/omp/agent-definitions. The
    // legacy PATCH surface keeps answering the config payload unchanged.
    return json(await configPayload());
  });
  route('GET', '/config/providers', async () => {
    const providers = await providersPayload();
    return json({ providers, default: providers[0]?.id ?? '' });
  });
  route('GET', '/agent', async () => {
    await engine.ready();
    // Legacy wire face (02 §5.1 D-B1): the manufactured build/plan shells
    // keep old clients rendering; worker definitions live behind
    // /api/omp/agent-definitions and personas behind /api/omp/personas.
    return json([
      { name: 'build', description: 'General purpose coding agent', mode: 'primary', builtIn: true },
      { name: 'plan', description: 'Planning agent (read-only analysis before execution)', mode: 'primary', builtIn: true },
    ]);
  });
  route('GET', '/skill', async (request, ctx) => {
    const directory = projectDirectory(ctx);
    try {
      // Dynamic import on purpose: a failed SDK load must degrade this one
      // request to `[]` (the catch below), not break host startup.
      const { discoverSkills } = await import('@oh-my-pi/pi-coding-agent');
      const { skills } = await discoverSkills(directory);
      return json(await wireSkillRows(skills ?? []));
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
    const hits: Array<{ type: string; name: string; path: string; score: number }> = [];
    const walk = (dir: string, depth: number) => {
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
    const directory = ctx.url.searchParams.get('directory');
    const limit = Number(ctx.url.searchParams.get('limit') ?? 500);
    const byDirectory = await engine.listAllSessions({ archived: archived === 'false' ? false : undefined });
    let all = [...byDirectory.values()].flat();
    if (directory) {
      // Scoped callers (directory bootstrap, per-directory refresh) expect only
      // the sessions the named directory owns — the contract external OpenCode
      // runtimes honor and the proxy forwards. Answering with every
      // directory's sessions seeded foreign records into every directory child
      // store, poisoning containment-based session-directory resolution
      // (mis-addressed archive incident).
      const directoryKey = normalizeDirectoryKey(directory);
      all = all.filter((session) => normalizeDirectoryKey(session.directory ?? '') === directoryKey);
    }
    all.sort((a, b) => (b.time?.updated ?? 0) - (a.time?.updated ?? 0));
    const page = Number.isFinite(limit) && limit > 0 ? all.slice(0, limit) : all;
    return json(page);
  });
  route('POST', '/experimental/control-plane/move-session', async (request) => {
    const body = await readJsonBody<MoveSessionBody>(request);
    const sessionID = body.sessionID;
    const destination = body.destination?.directory;
    if (!sessionID || !destination) return badRequest('sessionID and destination.directory are required');
    const moved = await engine.moveSession({ sessionID, destination });
    return json(moved ?? {});
  });
  // ---- domain modules (specs 01/02/03/04/06; public /api/omp/*) ----
  const ompPublish: PublishFn = (type, payload, scope) => engine.ompBus.publish(type, payload, scope);
  registerModelSettingsRoutes(route, {
    store: {
      settingsFor: async (directory) => {
        // SAFETY: a null store is the boot-degrade path; settings requests
        // then fail loudly (500) rather than masquerading as empty success.
        const store = (await engine.settingsStoreReady()) as SettingsStore;
        return store.settingsFor(directory);
      },
      getRevision: () => engine.settingsStore?.getRevision?.() ?? 0,
      bumpRevision: () => engine.settingsStore?.bumpRevision?.() ?? 0,
      chainWrites: (targetKey, task) => {
        const store = engine.settingsStore;
        return store ? store.chainWrites(targetKey, task) : Promise.resolve();
      },
      invalidateDerived: async () => {
        // SAFETY: same boot-degrade stance as settingsFor above.
        const store = (await engine.settingsStoreReady()) as SettingsStore;
        await store.invalidateDerived();
      },
      get boot() {
        // SAFETY: global writes only run after boot; the degrade path has
        // no store and PUT /omp/settings fails at settingsFor first.
        return (engine.settingsStore as SettingsStore).boot;
      },
      get bootDirectory() {
        // SAFETY: same boot-degrade stance as boot above.
        return (engine.settingsStore as SettingsStore).bootDirectory;
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
  registerPluginsDomainRoutes(route, {
    features: ompFeatures(),
    snapshots: () => engine.appliedPluginsSnapshots(),
    reloadSessions: (directory, sessionId) => engine.reloadAppliedPlugins(directory, sessionId),
  });
  registerProvidersDomainRoutes(route, {
    features: ompFeatures(),
    listEngineModels: () => {
      try {
        return engine.availableModels();
      } catch {
        return [];
      }
    },
    refreshModels: () => engine.refreshModels(),
  });

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
    const kindsRaw = url.searchParams.get('kinds') ?? '';
    const kinds = kindsRaw ? kindsRaw.split(',').filter(Boolean) : undefined;
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
    const body = await readJsonBody<SessionModelBody>(request);
    const result = await engine.setSessionModel({
      sessionID: ctx.params.id,
      directory,
      model: body?.model && typeof body.model === 'object' ? body.model : undefined,
      thinkingLevel:
        typeof body?.thinkingLevel === 'string' && body.thinkingLevel.length > 0 ? body.thinkingLevel : undefined,
    });
    if (!result.ok) return badRequest(result.error ?? 'model switch failed');
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
        const send = (text: string) => {
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
  const sseHandler = (request: Request, { global }: { global: boolean }) => {
    const directory = global ? null : directoryFromRequest({ url: new URL(request.url), headers: request.headers });
    const lastEventId = Number(request.headers.get('last-event-id') ?? 0) || 0;
    let closed = false;
    const stream = new ReadableStream({
      start(controller) {
        const encoder = new TextEncoder();
        const send = (text: string) => {
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
