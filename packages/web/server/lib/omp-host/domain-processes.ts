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
/**
 * 进程域（PLAN-session-process-monitor.md）：暴露引擎
 * ProcessLedger 的三个 HTTP 面——目录级快照、单次调用的输出尾巴与
 * kill 动作。所有权留在台账；本模块只是薄路由层（路由、门控、参数
 * 解码）。快照按目录作用域而所有权按会话作用域：未归属行携带
 * candidateSessionIds，由客户端按会话过滤（与 agent-runs 传输形状
 * 一致，spec 04 §5.5.1）。
 */

import { featureUnavailable } from './omp-parity.ts';
import type { OmpFeatures } from './omp-parity.ts';
import { normalizeDirectoryKey } from './registry.ts';
import type { LedgerKillRequest, LedgerKillResult, ProcessLedger } from './process-ledger.ts';
import type { UriRoute, UriRouteContext } from './domain-uri.ts';

/** 进程域依赖注入：feature 开关读取与惰性台账解析。 */
export interface ProcessDomainDeps {
  /** feature 开关读取函数；缺省视为全关。 */
  features?: () => OmpFeatures;
  /** 惰性解析：引擎异步解析 pi-natives，就绪前返回 null。 */
  /** Lazy: the engine resolves pi-natives asynchronously; null until ready. */
  ledger: () => ProcessLedger | null;
}

/** 进程域实例：仅暴露 mount 一个成员。 */
export interface ProcessDomain {
  /** 把三条路由挂载到给定的路由注册函数上。 */
  mount(route: UriRoute): void;
}

/** kill 请求体（运行时校验前的原始形状）。 */
interface KillBody {
  /** 动作名；仅 'kill' 受支持。 */
  kind?: unknown;
  /** 可选的单 pid 目标（省略则杀整棵 entry 树）。 */
  pid?: unknown;
}

/** Response.json 自身的参数契约——平台持有的类型透传。 */
type ResponseJsonData = Parameters<typeof Response.json>[0];

/** JSON 响应构造直通函数。 */
const json = (data: ResponseJsonData, init?: ResponseInit): Response => Response.json(data, init);

/** 从路由上下文读取并归一化 directory 查询参数；缺失返回 null。 */
const directoryOf = (ctx: UriRouteContext | undefined): string | null => {
  const raw = ctx?.url?.searchParams.get('directory');
  return raw ? normalizeDirectoryKey(raw) : null;
};

/** 读取请求 JSON 体；解析失败返回空对象（由 handler 兜底校验）。 */
const readJsonBody = async (request: Request): Promise<KillBody> => {
  try {
    // SAFETY: handlers runtime-validate `kind`/`pid` below.
    return (await request.json()) as KillBody;
  } catch {
    return {};
  }
};

/** 创建进程域：所有路由共享 feature 门控与 kill 结果到 HTTP 的映射。 */
export const createProcessDomain = (deps: ProcessDomainDeps): ProcessDomain => {
  /** 判定某 feature 键当前是否为 true。 */
  const featureOn = (key: string): boolean => deps.features?.()[key] === true;
  /** 门控：feature 关闭或台账未就绪返回 501 响应，否则返回台账。 */
  const gate = (): { ledger: ProcessLedger } | { blocked: Response } => {
    const ledger = deps.ledger();
    if (!featureOn('processes.v1') || !ledger) return { blocked: featureUnavailable('processes.v1') };
    return { ledger };
  };

  /** 把台账 kill 结果映射为 HTTP：forbidden→403、invalid→400、其余→404。 */
  const killResultToResponse = (result: LedgerKillResult): Response => {
    if (result.ok) return json(result);
    const status = result.error === 'forbidden' ? 403 : result.error === 'invalid' ? 400 : 404;
    return json({ error: `process-${result.error ?? 'error'}` }, { status });
  };

  return {
    /** 挂载三条路由：GET 快照 / GET 输出 / POST kill。 */
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
