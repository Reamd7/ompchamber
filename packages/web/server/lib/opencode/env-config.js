/**
 * OpenCode 托管引擎的环境变量配置解析：从注入的 env 读取端口
 * （OPENCODE_PORT / OMPCHAMBER_OPENCODE_PORT / OMPCHAMBER_INTERNAL_PORT）、
 * 外部引擎地址（OPENCODE_HOST，形如 http://host:port 的 URL）与绑定主机名
 * （OMPCHAMBER_OPENCODE_HOSTNAME），逐一做合法性校验并产出统一配置对象。
 * 非法输入只经 logger 告警并回退安全默认值（null / 127.0.0.1），不抛异常。
 */
import { isIP } from 'node:net';

/** 主机名最大长度（DNS 全名的 253 字符上限）。 */
const MAX_HOSTNAME_LENGTH = 253;
/** 单个 DNS 标签的合法形式：字母或数字开头结尾，中间可含连字符。 */
const HOSTNAME_LABEL_RE = /^[a-zA-Z0-9](?:[a-zA-Z0-9-]*[a-zA-Z0-9])?$/;
// 中文补充：命中此正则（纯数字点分）却又不是合法 IP 的值，视为手误的
// IP 地址（如 "0.0.0.0.0"）予以拒绝，防止其以“语法上合法的主机名”漏过。
// All-numeric dotted values must be a real IPv4 address; otherwise typo'd IPs
// like "0.0.0.0.0" would slip through as (technically valid) hostnames.
const ALL_NUMERIC_DOTTED_RE = /^\d+(?:\.\d+)*$/;

// 中文补充：校验托管 OpenCode 服务可用的绑定主机名——只接受 IPv4、IPv6
// （可带方括号）或 DNS 风格主机名；URL、端口号、路径、空白、下划线等一律拒绝。
// Valid bind hostnames for the managed OpenCode server: IPv4, IPv6 (with or
// without brackets), or a DNS-style hostname. Everything else (URLs, ports,
// paths, whitespace, underscores) is rejected.
/**
 * 判断值是否为托管 OpenCode 引擎可用的绑定主机名。
 *
 * 校验顺序：非字符串或 trim 后为空、超长直接拒绝；带方括号的形式要求
 * 内部是合法 IPv6；合法 IP（v4/v6）直接通过；纯数字点分串（非合法 IP
 * 的手误）拒绝；最后按 DNS 标签逐段校验主机名。
 *
 * @param {unknown} value 待校验的主机名
 * @returns {boolean} 是否合法
 */
export const isValidOpenCodeHostname = (value) => {
  if (typeof value !== 'string') return false;
  const trimmed = value.trim();
  if (!trimmed || trimmed.length > MAX_HOSTNAME_LENGTH) return false;
  if (trimmed.startsWith('[') && trimmed.endsWith(']')) {
    return isIP(trimmed.slice(1, -1)) === 6;
  }
  if (isIP(trimmed) !== 0) return true;
  if (ALL_NUMERIC_DOTTED_RE.test(trimmed)) return false;
  return trimmed.split('.').every((label) => HOSTNAME_LABEL_RE.test(label));
};

/**
 * 解析与 OpenCode 托管引擎相关的环境变量配置。
 *
 * 端口取三个候选变量中第一个能解析为正整数的值；OPENCODE_HOST 必须是
 * 带显式端口、不含 path/query/hash 的 http/https URL；主机名需通过
 * isValidOpenCodeHostname 校验。每个非法输入都会告警并回退默认值。
 *
 * @param {object} [options] 可选注入：env（环境变量对象）、logger（默认 console）
 * @returns {{ configuredOpenCodePort: number|null,
 *   configuredOpenCodeHost: { origin: string, port: number }|null,
 *   effectivePort: number|null, configuredOpenCodeHostname: string }}
 */
export const resolveOpenCodeEnvConfig = (options = {}) => {
  const env = options.env && typeof options.env === 'object' ? options.env : {};
  const logger = options.logger ?? console;

  // 端口配置：三个候选环境变量按优先级取第一个能解析为正整数的值。
  const configuredOpenCodePort = (() => {
    const raw =
      env.OPENCODE_PORT ||
      env.OMPCHAMBER_OPENCODE_PORT ||
      env.OMPCHAMBER_INTERNAL_PORT;
    if (!raw) {
      return null;
    }
    const parsed = parseInt(raw, 10);
    return Number.isFinite(parsed) && parsed > 0 ? parsed : null;
  })();

  // 外部引擎地址：OPENCODE_HOST 需为带显式端口的 http(s) URL，格式问题一律告警并置 null。
  const configuredOpenCodeHost = (() => {
    const raw = typeof env.OPENCODE_HOST === 'string' ? env.OPENCODE_HOST.trim() : '';
    if (!raw) return null;

    // 对非法 OPENCODE_HOST 输出统一格式的告警日志。
    const warnInvalidHost = (reason) => {
      logger.warn(`[config] Ignoring OPENCODE_HOST=${JSON.stringify(raw)}: ${reason}`);
    };

    let url;
    try {
      url = new URL(raw);
    } catch {
      warnInvalidHost('not a valid URL');
      return null;
    }
    if (url.protocol !== 'http:' && url.protocol !== 'https:') {
      warnInvalidHost(`must use http or https scheme (got ${JSON.stringify(url.protocol)})`);
      return null;
    }
    const port = parseInt(url.port, 10);
    if (!Number.isFinite(port) || port <= 0) {
      warnInvalidHost('must include an explicit port (example: http://hostname:4096)');
      return null;
    }
    if (url.pathname !== '/' || url.search || url.hash) {
      warnInvalidHost('must not include path, query, or hash');
      return null;
    }
    return { origin: url.origin, port };
  })();

  // 中文补充：OPENCODE_HOST 与 OPENCODE_PORT 同时设置时，前者优先。
  // OPENCODE_HOST takes precedence over OPENCODE_PORT when both are set
  const effectivePort = configuredOpenCodeHost?.port ?? configuredOpenCodePort;

  const configuredOpenCodeHostname = (() => {
    const raw = env.OMPCHAMBER_OPENCODE_HOSTNAME;
    if (typeof raw !== 'string') {
      return '127.0.0.1';
    }
    const trimmed = raw.trim();
    if (!trimmed) {
      logger.warn(
        `[config] Ignoring OMPCHAMBER_OPENCODE_HOSTNAME=${JSON.stringify(raw)}: empty after trimming`,
      );
      return '127.0.0.1';
    }
    if (!isValidOpenCodeHostname(trimmed)) {
      logger.error(
        `[config] Rejecting OMPCHAMBER_OPENCODE_HOSTNAME=${JSON.stringify(raw)}: `
        + 'must be a valid hostname or IP address (for example 127.0.0.1, 0.0.0.0, localhost, [::1]); '
        + 'falling back to 127.0.0.1 (loopback only)',
      );
      return '127.0.0.1';
    }
    return trimmed;
  })();

  return {
    configuredOpenCodePort,
    configuredOpenCodeHost,
    effectivePort,
    configuredOpenCodeHostname,
  };
};
