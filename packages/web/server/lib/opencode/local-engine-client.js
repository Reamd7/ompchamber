// Server-side HTTP client for the managed omp host.
//
// Replaces the @opencode-ai/sdk generated client in the web server's own
// call sites (openchamber-sessions, scheduled-tasks, openchamber-control,
// skill-routes). Same result convention as the SDK: every call resolves to
// `{ data, error?, response }` and never throws for HTTP outcomes.
//
// Node-safe (no Bun or TypeScript imports) — the web server also runs
// in-process inside Electron's Node runtime.
/**
 * （中文说明）面向托管 omp host 的服务端 HTTP 客户端。
 *
 * 以与 @opencode-ai/sdk 相同的约定返回 { data, error?, response }，HTTP
 * 层面的失败（非 2xx、网络异常、响应不可达）一律通过 error 字段返回而不
 * 抛异常，使 openchamber-sessions、scheduled-tasks 等调用点无需 try/catch。
 */

/**
 * 把查询参数对象序列化为 URL 查询串（含前导 '?'）。
 * undefined/null 值被跳过，其余强转为字符串；没有有效参数时返回空串。
 * @param {object} params - 查询参数键值对。
 * @returns {string}
 */
const asQuery = (params) => {
  const query = new URLSearchParams();
  for (const [key, value] of Object.entries(params ?? {})) {
    if (value === undefined || value === null) continue;
    query.set(key, String(value));
  }
  const text = query.toString();
  return text ? `?${text}` : '';
};

/**
 * 创建本地引擎客户端。
 * @param {object} options - baseUrl 必填（尾部斜杠会被剥掉）；headers 为
 *   附加的公共请求头；directory 会被编码进 x-opencode-directory 头以
 *   指定工作目录；fetchImpl 可注入替代全局 fetch（测试用）。
 * @throws baseUrl 缺失时抛出 Error。
 * @returns 按 SDK 资源分组的客户端（session / experimental / command / app）。
 */
export const createLocalEngineClient = ({ baseUrl, headers = {}, directory, fetchImpl = fetch } = {}) => {
  const root = String(baseUrl ?? '').replace(/\/+$/, '');
  if (!root) throw new Error('createLocalEngineClient requires a baseUrl');
  const baseHeaders = {
    ...headers,
    ...(directory ? { 'x-opencode-directory': encodeURIComponent(directory) } : {}),
  };

  /**
   * 执行一次 HTTP 请求并把结果归一化为 SDK 约定。
   * 有 body 时自动加 content-type 并 JSON 序列化；响应体先按文本读取再
   * 尝试 JSON 解析，失败则包装为 { message: text }。非 2xx 或网络异常时
   * 返回 error 对象（name + message），成功时返回 data；两者互斥、不抛异常。
   * @param {string} method - HTTP 方法。
   * @param {string} pathname - 相对根路径的路径（自动拼接查询串）。
   * @param {object} [options] - query 查询参数、body JSON 请求体。
   */
  const request = async (method, pathname, { query, body } = {}) => {
    try {
      const response = await fetchImpl(`${root}${pathname}${asQuery(query)}`, {
        method,
        headers: {
          ...(body !== undefined ? { 'content-type': 'application/json' } : {}),
          ...baseHeaders,
        },
        ...(body !== undefined ? { body: JSON.stringify(body) } : {}),
      });
      const text = await response.text();
      let payload = null;
      if (text) {
        try {
          payload = JSON.parse(text);
        } catch {
          payload = { message: text };
        }
      }
      if (!response.ok) {
        const message = payload?.data?.message ?? payload?.message ?? `${method} ${pathname} failed with ${response.status}`;
        return { data: undefined, error: { name: payload?.name ?? 'UnknownError', message }, response };
      }
      return { data: payload, error: undefined, response };
    } catch (error) {
      return { data: undefined, error: { name: 'UnknownError', message: error?.message ?? String(error) }, response: undefined };
    }
  };

  // SDK 形状的 API 分组：资源路径与 @opencode-ai/sdk 保持一致
  return {
    // 会话资源：创建、列表、状态、消息、命令与 fork
    session: {
      create: (params) => request('POST', '/session', { body: params }),
      list: (params) => request('GET', '/session', { query: params }),
      status: (params) => request('GET', '/session/status', { query: params }),
      messages: ({ sessionID, ...rest }) =>
        request('GET', `/session/${encodeURIComponent(sessionID)}/message`, { query: rest }),
      command: ({ sessionID, ...rest }) =>
        request('POST', `/session/${encodeURIComponent(sessionID)}/command`, { body: rest }),
      fork: ({ sessionID, ...rest }) =>
        request('POST', `/session/${encodeURIComponent(sessionID)}/fork`, { body: rest }),
    },
    // 实验性接口：扩展的会话列表
    experimental: {
      session: {
        list: (params) => request('GET', '/experimental/session', { query: params }),
      },
    },
    // 自定义命令列表
    command: {
      list: (params) => request('GET', '/command', { query: params }),
    },
    // 应用级资源：技能（skills）列表
    app: {
      skills: (params) => request('GET', '/skill', { query: params }),
    },
  };
};
