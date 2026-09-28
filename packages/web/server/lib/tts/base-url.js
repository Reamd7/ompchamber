/**
 * 自定义 OpenAI 兼容 baseURL 的归一化与安全校验：仅接受 http/https、
 * 拒绝内嵌凭据；非本机地址默认禁止，除非处于 desktop 运行时或显式
 * 设置 OMPCHAMBER_ALLOW_REMOTE_OPENAI_COMPAT_URLS。合法输入去 query/hash/
 * 尾斜杠后返回 origin+path。
 */
/** 视为"本机"的 hostname 白名单。 */
const LOCAL_BASE_URL_HOSTS = new Set([
  'localhost',
  '127.0.0.1',
  '::1',
  'host.docker.internal',
]);

/** 判定环境变量开关：接受 boolean，或字符串 '1'/'true'（大小写不敏感）。 */
const isEnvFlagEnabled = (value) => {
  if (value === true || value === 1) return true;
  if (typeof value !== 'string') return false;
  const normalized = value.trim().toLowerCase();
  return normalized === '1' || normalized === 'true';
};

/** 规范化 hostname：trim + 小写，并去掉 IPv6 字面量的方括号。 */
const normalizeHostname = (hostname) => {
  if (typeof hostname !== 'string') return '';
  const trimmed = hostname.trim().toLowerCase();
  if (!trimmed) return '';
  if (trimmed.startsWith('[') && trimmed.endsWith(']')) {
    return trimmed.slice(1, -1);
  }
  return trimmed;
};

/** hostname（规范化后）是否在本机白名单内。 */
const isAllowedLocalHost = (hostname) => {
  const normalized = normalizeHostname(hostname);
  return LOCAL_BASE_URL_HOSTS.has(normalized);
};

/**
 * 校验并归一化自定义 baseURL。
 * @returns {{ value?: string, error?: string }} 空输入返回 { value: undefined }；
 *   合法时 value 为去掉 query/hash/尾斜杠 的 protocol//host+path；
 *   协议非法 / 带凭据 / 远程地址被禁分别返回对应 error
 */
export const normalizeCustomOpenAIBaseURL = (value) => {
  if (typeof value !== 'string' || !value.trim()) {
    return { value: undefined };
  }

  let parsed;
  try {
    parsed = new URL(value.trim());
  } catch {
    return { error: 'Custom server URL is invalid' };
  }

  if (parsed.protocol !== 'http:' && parsed.protocol !== 'https:') {
    return { error: 'Custom server URL must use http or https' };
  }

  if (parsed.username || parsed.password) {
    return { error: 'Custom server URL must not include credentials' };
  }

  const isDesktop = (process.env.OMPCHAMBER_RUNTIME || '').trim().toLowerCase() === 'desktop';
  const envFlagRaw = process.env.OMPCHAMBER_ALLOW_REMOTE_OPENAI_COMPAT_URLS;
  const hasExplicitFlag = typeof envFlagRaw === 'string' && envFlagRaw.trim().length > 0;
  const allowRemote = hasExplicitFlag ? isEnvFlagEnabled(envFlagRaw) : isDesktop;
  if (!allowRemote && !isAllowedLocalHost(parsed.hostname)) {
    return {
      error: 'Remote custom server URLs are disabled. Set OMPCHAMBER_ALLOW_REMOTE_OPENAI_COMPAT_URLS=true to allow this host.',
    };
  }

  parsed.hash = '';
  parsed.search = '';
  const pathname = parsed.pathname.replace(/\/+$/, '');
  const normalizedPath = pathname.length > 0 ? pathname : '';
  return { value: `${parsed.protocol}//${parsed.host}${normalizedPath}` };
};
