// omp host entry: an OpenCode-compatible HTTP+SSE server backed by an
// embedded @oh-my-pi/pi-coding-agent engine.
//
// Launch shape mirrors the managed OpenCode server this replaces:
//   bun host.js serve --hostname 127.0.0.1 --port 3902
//
// The spawner (web server lifecycle / VS Code parity impl) sets
// OPENCODE_SERVER_PASSWORD for HTTP Basic auth, exactly as it did for
// `opencode serve`. Readiness is signaled by the same stdout line the
// OpenCode server printed, so existing wait-for-ready logic keeps working.

import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { OmpHostEngine } from './engine.js';
import { registerEndpoints } from './endpoints.js';

const HOST_VERSION = 'openchamber-omp-host/1.0.0';

const parseArgs = (argv) => {
  const args = { command: argv[0] ?? 'serve', hostname: '127.0.0.1', port: 0 };
  for (let i = 1; i < argv.length; i += 1) {
    const arg = argv[i];
    if (arg === '--hostname') args.hostname = argv[++i];
    else if (arg === '--port') args.port = Number(argv[++i]);
    else if (arg?.startsWith('--hostname=')) args.hostname = arg.slice('--hostname='.length);
    else if (arg?.startsWith('--port=')) args.port = Number(arg.slice('--port='.length));
  }
  return args;
};

const __dirname = path.dirname(fileURLToPath(import.meta.url));

export const startOmpHost = async ({ hostname = '127.0.0.1', port = 0, engine } = {}) => {
  const hostEngine = engine ?? new OmpHostEngine();

  const password = (process.env.OPENCODE_SERVER_PASSWORD ?? '').trim();
  const expectedAuthorization = password
    ? 'Basic ' + Buffer.from(`opencode:${password}`).toString('base64')
    : null;
  const routes = [];
  const route = (method, pattern, handler) => {
    const names = [];
    const regex = new RegExp(
      '^' +
        pattern.replace(/\{(\w+)\}/g, (_, name) => {
          names.push(name);
          return '([^/]+)';
        }) +
        '$',
    );
    routes.push({ method, regex, names, handler });
  };
  const { sseHandler } = registerEndpoints(route, hostEngine, {
    version: HOST_VERSION,
    dirname: __dirname,
  });

  // Bun's default 10s idle timeout kills long-lived SSE streams; the 15s
  // heartbeat keeps connections alive well under the 255s ceiling.
  const server = Bun.serve({
    hostname,
    port,
    idleTimeout: 255,
    async fetch(request) {
      const url = new URL(request.url);
      const pathname = decodeURIComponent(url.pathname);

      if (expectedAuthorization && request.headers.get('authorization') !== expectedAuthorization) {
        return new Response('Unauthorized', { status: 401, headers: { 'WWW-Authenticate': 'Basic realm="opencode"' } });
      }

      if ((pathname === '/global/event' || pathname === '/event') && request.method === 'GET') {
        return sseHandler(request, { global: pathname === '/global/event' });
      }

      for (const entry of routes) {
        if (entry.method !== request.method) continue;
        const match = entry.regex.exec(pathname);
        if (!match) continue;
        const params = {};
        entry.names.forEach((name, index) => {
          params[name] = match[index + 1];
        });
        try {
          return await entry.handler(request, { params, url, headers: request.headers, engine: hostEngine });
        } catch (error) {
          console.error('[omp-host] handler error:', pathname, error);
          return Response.json(
            { name: 'UnknownError', data: { message: error?.message ?? String(error) } },
            { status: 500 },
          );
        }
      }

      return Response.json(
        { name: 'UnknownError', data: { message: `Not found: ${request.method} ${pathname}` } },
        { status: 404 },
      );
    },
  });

  const baseUrl = `http://${server.hostname === '0.0.0.0' ? '127.0.0.1' : server.hostname}:${server.port}`;
  return {
    server,
    baseUrl,
    engine: hostEngine,
    async close() {
      await hostEngine.shutdown();
      server.stop(true);
    },
  };
};

const isMain = import.meta.main ?? false;
if (isMain) {
  const args = parseArgs(process.argv.slice(2));
  const host = await startOmpHost(args);
  // Same readiness line the managed OpenCode server printed on stdout.
  console.log(`opencode server listening on ${host.baseUrl}`);
  const shutdown = async (signal) => {
    console.log(`[omp-host] received ${signal}, shutting down`);
    await host.close();
    process.exit(0);
  };
  process.on('SIGTERM', () => void shutdown('SIGTERM'));
  process.on('SIGINT', () => void shutdown('SIGINT'));
}
