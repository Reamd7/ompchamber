// Provider state that exists only inside the running OpenCode process.
//
// A plugin registers its provider from the `config` hook and supplies the
// credential from its `auth` loader, both at startup. Neither ends up in
// `opencode.json` or `auth.json`, so a server that only reads files sees
// nothing — which is why plugin-backed models used to fail here with
// "has no known API base URL" while working fine in chat (#2666).
//
// `GET /provider` is where that state becomes visible. It reports, per
// provider, the resolved `options.baseURL` and `options.apiKey`, and per model
// the wire adapter (`api.npm`) and endpoint (`api.url`).
//
// What it does NOT report is `options.fetch`. OpenCode strips functions from
// the response, and a plugin is free to put its whole protocol in there:
// rewriting the path, signing the request, translating the payload. Such a
// provider advertises a perfectly ordinary base URL that answers nothing we
// know how to ask, and no field distinguishes the two.
//
// Asking the endpoint (`GET /models`) looked like the way to tell them apart,
// and it does answer correctly for that case — but measured against the 166
// providers in the models.dev catalog it also denies six that work fine and
// simply have no `/models` route. A provider that vanishes from the picker
// explains nothing; one that fails on use says why. So this module reports
// what it knows and leaves the verdict to the call itself.
/**
 * 维护只存在于运行中 OpenCode 进程内的 provider 状态：插件注册的
 * provider 与凭证不落盘，只能通过 GET /provider 观测。快照带 30 秒 TTL，
 * 并发读取合并为单次请求（single-flight）；拉取失败时返回 null 表示
 * "未知"，调用方应回落到基于 auth.json 的文件解析。
 */

/** 快照缓存有效期（毫秒）：过期后下一次读取触发重新拉取。 */
const SNAPSHOT_TTL_MS = 30_000;
/** 拉取 /provider 的超时（毫秒），避免 OpenCode 无响应时拖住调用方。 */
const SNAPSHOT_TIMEOUT_MS = 5_000;

// opencode zen hands out this sentinel instead of a key when the user has no
// zen login, and trims its catalog to the free models. Those run on OpenCode's
// own subsidised infrastructure and are meant to be reached through OpenCode,
// not by us. Treating the sentinel as a credential would do exactly that, so
// it is never accepted as one.
/** opencode zen 的匿名哨兵值：出现即代表没有真实凭证，绝不能当作 apiKey 使用。 */
export const ZEN_ANONYMOUS_API_KEY = 'public';

/** 与运行中 OpenCode 的连接（buildOpenCodeUrl / getOpenCodeAuthHeaders）；null 表示未接线。 */
let connection = null;
/** 最近一次成功拉取的快照；null 表示尚无缓存。 */
let snapshot = null;
/** 快照落地时间戳（毫秒），用于 TTL 判断。 */
let snapshotAt = 0;
/** 进行中的拉取 Promise，并发读取去重用（single-flight）。 */
let inflight = null;

/**
 * Wires this module to the running OpenCode instance. Called once at server
 * startup; pass `null` to detach. Until it is wired every lookup answers
 * "nothing known", which leaves the file-based resolution unchanged.
 */
/** 接入 OpenCode 连接（传 null 解除绑定）；接线或换连接时同步清空全部缓存。 */
export function configureOpenCodeRuntimeProviders(next) {
  connection = next ?? null;
  resetOpenCodeRuntimeProviders();
}

/**
 * Drops every cached answer. OpenCode restarts reload plugins, which can
 * change ports, keys and the provider list itself.
 */
/** 清空快照与进行中的请求；OpenCode 重启后必须调用，因为插件、端口与密钥都可能变化。 */
export function resetOpenCodeRuntimeProviders() {
  snapshot = null;
  snapshotAt = 0;
  inflight = null;
}

/**
 * The boundary. Everything the `/provider` payload claims is checked here, so
 * the rest of this module and its callers work with settled values:
 * a credential we may use, an endpoint, and whether the provider is the
 * anonymous zen case.
 *
 * The credential deliberately prefers `options.apiKey` over the `key` field:
 * for a plugin provider the former is what its auth loader produced and what
 * OpenCode itself sends, while `key` only carries env/auth.json values this
 * server can already read from disk.
 */
/**
 * 把 /provider 响应归一化为内部快照：providers（Map，id 到条目）与
 * connected（Set）。所有字段校验与清洗（trim、去尾部斜杠、剔除 zen 哨兵）
 * 都集中在此，下游拿到的都是稳定值。
 */
function parseProviderListing(payload) {
  const providers = new Map();
  const connected = new Set();
  if (!payload || typeof payload !== 'object') return { providers, connected };

  // 归一化字符串：非字符串或纯空白返回 null。
  const text = (value) => (typeof value === 'string' && value.trim() ? value.trim() : null);
  // 归一化记录：非对象返回空对象，便于安全地链式取字段。
  const record = (value) => (value && typeof value === 'object' ? value : {});
  // 归一化端点：去掉末尾斜杠。
  const endpoint = (value) => text(value)?.replace(/\/+$/, '') ?? null;

  for (const raw of Array.isArray(payload.all) ? payload.all : []) {
    const id = text(record(raw).id);
    if (!id) continue;
    const options = record(record(raw).options);
    const firstModel = record(Object.values(record(record(raw).models))[0]);
    const declaredKey = text(options.apiKey);
    providers.set(id, {
      id,
      source: text(record(raw).source),
      apiKey: declaredKey === ZEN_ANONYMOUS_API_KEY ? null : (declaredKey ?? text(record(raw).key)),
      baseURL: endpoint(options.baseURL) ?? endpoint(record(firstModel.api).url),
      // True only for the zen-without-login case: a provider that is present
      // and usable through OpenCode, but that we must not call ourselves.
      anonymousZen: declaredKey === ZEN_ANONYMOUS_API_KEY,
    });
  }

  // Providers OpenCode considers usable right now. A provider can be present
  // in `all` (it is in the catalog) without any credential behind it.
  for (const raw of Array.isArray(payload.connected) ? payload.connected : []) {
    const id = text(raw);
    if (id) connected.add(id);
  }

  return { providers, connected };
}

/**
 * 实际执行 GET /provider 并解析为快照；非 2xx 或网络失败时抛错，
 * 缓存回退策略由 getRuntimeProviderSnapshot 决定。
 */
const fetchSnapshot = async () => {
  const response = await fetch(connection.buildOpenCodeUrl('/provider', ''), {
    headers: { Accept: 'application/json', ...connection.getOpenCodeAuthHeaders() },
    signal: AbortSignal.timeout(SNAPSHOT_TIMEOUT_MS),
  });
  if (!response.ok) {
    throw new Error(`OpenCode provider listing failed with ${response.status}`);
  }
  return parseProviderListing(await response.json());
};

/**
 * The current runtime provider snapshot, or `null` when OpenCode cannot be
 * reached.
 *
 * `null` means "unknown", never "no providers": callers must fall back to
 * their file-based resolution rather than treat an unreachable OpenCode as an
 * empty provider list.
 */
/**
 * 读取当前运行时 provider 快照（TTL 缓存 + 单飞合并）。
 * @returns {Promise<{providers: Map<string, Object>, connected: Set<string>}|null>}
 *   null 表示 "未知"（未接线，或拉取失败且无旧快照）：调用方必须回落到
 *   文件解析，绝不能当作空 provider 列表处理。
 */
export async function getRuntimeProviderSnapshot() {
  if (!connection) return null;
  if (snapshot && Date.now() - snapshotAt < SNAPSHOT_TTL_MS) return snapshot;
  if (!inflight) {
    inflight = fetchSnapshot().finally(() => {
      inflight = null;
    });
  }
  try {
    snapshot = await inflight;
    snapshotAt = Date.now();
    return snapshot;
  } catch {
    // Keep serving the previous snapshot when there is one: a momentarily
    // unreachable OpenCode should not retract providers that were resolving a
    // second ago.
    return snapshot;
  }
}

/**
 * Runtime credential and endpoint for one provider, or `null` when OpenCode
 * knows nothing about it.
 */
/** 取单个 provider 的运行时凭证与端点；OpenCode 不了解该 provider 时返回 null。 */
export async function getRuntimeProvider(providerID) {
  const current = await getRuntimeProviderSnapshot();
  return current?.providers.get(providerID) ?? null;
}


