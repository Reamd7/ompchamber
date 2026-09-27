/**
 * @module proxy-headers
 * OpenCode 反向代理的 header 白名单工具。
 *
 * 负责两件事：
 * 1. 组装转发给受管 OpenCode 上游的请求 header —— 剔除 hop-by-hop header、
 *    content-length、accept-encoding 等逐跳/传输层字段，并用受管上游自身的
 *    Authorization 覆盖客户端凭证（客户端的 UI bearer token 对上游无效，
 *    转发只会换来 401）。
 * 2. 过滤上游响应中回写给下游客户端的 header —— 例如 content-encoding：代理
 *    已拿到解压后的 body，若透传该 header，客户端会按 gzip 解码纯文本而失败。
 */

/**
 * 转发到上游时必须剔除的请求 header 集合（小写键）。
 *
 * 包含两类：
 * - 客户端凭证（authorization）：上游只认自己的鉴权，转发客户端 bearer 会让
 *   所有上游响应变成 401（见下方内嵌英文注释）；
 * - hop-by-hop / 传输层 header（host、connection、content-length、
 *   transfer-encoding 等）：由本次代理连接自行协商，透传会破坏 HTTP 语义。
 */
const filteredRequestHeaders = new Set([
  // Client credentials for the OMPChamber server (UI client tokens) must
  // never reach the managed OpenCode upstream — it only accepts its own auth,
  // so a forwarded client bearer turns every upstream response into a 401.
  'authorization',
  'host',
  'connection',
  'content-length',
  'transfer-encoding',
  'keep-alive',
  'te',
  'trailer',
  'upgrade',
  'accept-encoding',
]);

/**
 * 上游响应中禁止回写给下游客户端的 header 集合（小写键）。
 *
 * 主要是 hop-by-hop header，外加 www-authenticate（会触发浏览器弹出上游的
 * 认证框，而客户端应走 OMPChamber 自己的鉴权）和 content-encoding
 * （代理已解压 body，透传会导致客户端二次解码失败）。
 */
const filteredResponseHeaders = new Set([
  'connection',
  'content-length',
  'transfer-encoding',
  'keep-alive',
  'te',
  'trailer',
  'upgrade',
  'www-authenticate',
  'content-encoding',
]);

/**
 * 组装转发给受管 OpenCode 上游的请求 header。
 *
 * 遍历入站请求 header：跳过白名单（filteredRequestHeaders）中的键和空值，
 * 其余键统一转为小写；数组值以 ", " 拼接、其它值转字符串。最后若提供了
 * 上游鉴权 header（authHeaders.Authorization），则以它作为唯一的 Authorization。
 *
 * @param {Record<string, string | string[] | undefined>} requestHeaders 客户端原始请求 header
 * @param {{ Authorization?: string }} [authHeaders] 受管 OpenCode 的鉴权 header
 * @returns {Record<string, string>} 可直接传给 fetch 的转发 header 对象
 */
export const collectForwardProxyHeaders = (requestHeaders, authHeaders = {}) => {
  const headers = {};

  for (const [key, value] of Object.entries(requestHeaders || {})) {
    // 空值直接跳过；键小写化后对照白名单，命中（含客户端自带的
    // authorization）即不转发。
    if (!value) continue;
    const normalizedKey = key.toLowerCase();
    if (filteredRequestHeaders.has(normalizedKey)) continue;
    headers[normalizedKey] = Array.isArray(value) ? value.join(', ') : String(value);
  }

  // 客户端凭证已在上方循环剔除，这里写入受管上游自己的 Authorization，
  // 保证转发请求的凭证唯一且来自可信来源。
  if (authHeaders.Authorization) {
    headers.Authorization = authHeaders.Authorization;
  }

  return headers;
};

/**
 * 判断某个上游响应 header 是否允许回写给下游客户端。
 *
 * 非字符串或空白键一律拒绝；其余按键小写对照 filteredResponseHeaders
 * 白名单，命中则不转发。
 *
 * @param {string} key 响应 header 名
 * @returns {boolean} true 表示可以转发
 */
export const shouldForwardProxyResponseHeader = (key) => {
  if (typeof key !== 'string' || key.trim().length === 0) {
    return false;
  }

  return !filteredResponseHeaders.has(key.toLowerCase());
};

/**
 * 把上游响应 header 应用到 express 响应对象上（白名单过滤后逐个 setHeader）。
 *
 * responseHeaders 须支持 entries() 迭代（如 fetch 的 Headers 对象）；
 * response 缺失或没有 setHeader 方法时静默跳过，便于测试传入部分 mock。
 *
 * @param {Iterable<[string, string]>} responseHeaders 上游返回的 header 集合
 * @param {{ setHeader?: Function }} response express Response（或等价 mock）
 */
export const applyForwardProxyResponseHeaders = (responseHeaders, response) => {
  if (!responseHeaders || typeof response?.setHeader !== 'function') {
    return;
  }

  for (const [key, value] of responseHeaders.entries()) {
    // 白名单外的 header 原样透传，保持上游响应语义完整。
    if (!shouldForwardProxyResponseHeader(key)) {
      continue;
    }
    response.setHeader(key, value);
  }
};
