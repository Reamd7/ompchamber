// Processes domain (PLAN-session-process-monitor.md): routes that expose the
// engine's ProcessLedger — a directory-scoped snapshot, a per-invocation
// output tail, and a kill action. Ownership stays in the ledger; this module
// is the thin HTTP face (routes, gating, param decoding).
//
//   GET  /omp/processes?directory=                    -> {revision, generatedAt, entries[]}
//   GET  /omp/processes/output?directory=&key=        -> {key, output, truncated, live}
//   POST /omp/processes/{sessionID}/{key}  {kind:'kill', pid?} -> {ok, killed, skipped}
//
// Snapshots are directory-scoped while ownership stays session-scoped:
// unattributed rows carry `candidateSessionIds` and are filtered per session
// client-side, matching the agent-runs transport shape (spec 04 §5.5.1).

import { featureUnavailable } from './omp-parity.ts';
import type { OmpFeatures } from './omp-parity.ts';
import { normalizeDirectoryKey } from './registry.ts';
import type { LedgerKillRequest, LedgerKillResult, ProcessLedger } from './process-ledger.ts';
import type { UriRoute, UriRouteContext } from './domain-uri.ts';

export interface ProcessDomainDeps {
  features?: () => OmpFeatures;
  /** Lazy: the engine resolves pi-natives asynchronously; null until ready. */
  ledger: () => ProcessLedger | null;
}

export interface ProcessDomain {
  mount(route: UriRoute): void;
}

interface KillBody {
  kind?: unknown;
  pid?: unknown;
}

type ResponseJsonData = Parameters<typeof Response.json>[0];

const json = (data: ResponseJsonData, init?: ResponseInit): Response => Response.json(data, init);

const directoryOf = (ctx: UriRouteContext | undefined): string | null => {
  const raw = ctx?.url?.searchParams.get('directory');
  return raw ? normalizeDirectoryKey(raw) : null;
};

const readJsonBody = async (request: Request): Promise<KillBody> => {
  try {
    // SAFETY: handlers runtime-validate `kind`/`pid` below.
    return (await request.json()) as KillBody;
  } catch {
    return {};
  }
};

export const createProcessDomain = (deps: ProcessDomainDeps): ProcessDomain => {
  const featureOn = (key: string): boolean => deps.features?.()[key] === true;
  const gate = (): { ledger: ProcessLedger } | { blocked: Response } => {
    const ledger = deps.ledger();
    if (!featureOn('processes.v1') || !ledger) return { blocked: featureUnavailable('processes.v1') };
    return { ledger };
  };

  const killResultToResponse = (result: LedgerKillResult): Response => {
    if (result.ok) return json(result);
    const status = result.error === 'forbidden' ? 403 : result.error === 'invalid' ? 400 : 404;
    return json({ error: `process-${result.error ?? 'error'}` }, { status });
  };

  return {
    mount(route) {
      route('GET', '/omp/processes', (_request: Request, ctx?: UriRouteContext) => {
        const gated = gate();
        if ('blocked' in gated) return gated.blocked;
        const directory = directoryOf(ctx);
        if (!directory) return json({ error: 'processes-directory-required' }, { status: 400 });
        return json(gated.ledger.snapshot(directory));
      });

      route('GET', '/omp/processes/output', (_request: Request, ctx?: UriRouteContext) => {
        const gated = gate();
        if ('blocked' in gated) return gated.blocked;
        const directory = directoryOf(ctx);
        const key = ctx?.url?.searchParams.get('key') ?? '';
        if (!directory || !key) return json({ error: 'processes-key-required' }, { status: 400 });
        const entry = gated.ledger.output(key);
        if (!entry) return json({ error: 'process-output-not-found' }, { status: 404 });
        return json(entry);
      });

      route('POST', '/omp/processes/{sessionID}/{key}', async (request: Request, ctx?: UriRouteContext) => {
        const gated = gate();
        if ('blocked' in gated) return gated.blocked;
        const sessionID = decodeURIComponent(ctx?.params?.sessionID ?? '');
        const key = decodeURIComponent(ctx?.params?.key ?? '');
        if (!sessionID || !key) return json({ error: 'processes-key-required' }, { status: 400 });
        const body = await readJsonBody(request);
        if (body.kind !== 'kill') return json({ error: 'processes-action-unsupported' }, { status: 400 });
        const killReq: LedgerKillRequest = { sessionID, key };
        if (Number.isInteger(body.pid)) {
          // SAFETY: Number.isInteger validated the runtime value is an int.
          killReq.pid = body.pid as number;
        }
        const result = await gated.ledger.kill(killReq);
        return killResultToResponse(result);
      });
    },
  };
};
