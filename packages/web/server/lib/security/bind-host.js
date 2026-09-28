/**
 * 绑定地址（bind host）安全分类：判断服务器要绑定的 host 是否确认为
 * 回环地址，供“未启用 UI 认证却要暴露到局域网”这类危险配置的拦截与
 * 告警决策使用。统一处理 IPv4 / IPv6 / [方括号] / IPv4-mapped IPv6 等
 * 书写形式；无法证明是回环的一律按暴露处理（白名单式判定）。
 */
import net from 'node:net';

/** 归一化 host 字符串：trim + 转小写，并去掉 IPv6 方括号（如 [::1] 变 ::1）；非字符串返回空串。 */
const stripIpv6Brackets = (value) => {
  if (typeof value !== 'string') return '';
  const trimmed = value.trim().toLowerCase();
  if (trimmed.startsWith('[') && trimmed.endsWith(']')) {
    return trimmed.slice(1, -1);
  }
  return trimmed;
};

/** 把 IPv4-mapped IPv6 地址（::ffff:127.0.0.1）还原成 IPv4 点分形式，其余形式原样返回。 */
const normalizeIpv4MappedAddress = (host) => {
  const normalized = stripIpv6Brackets(host);
  const match = normalized.match(/^::ffff:(\d{1,3}(?:\.\d{1,3}){3})$/);
  return match ? match[1] : normalized;
};

/** 仅当 host 是合法 IPv4 且首段为 127（整个 127.0.0.0/8 回环段）时为 true。 */
const isLoopbackIpv4 = (host) => {
  if (net.isIP(host) !== 4) return false;
  const first = Number.parseInt(host.split('.')[0] || '', 10);
  return first === 127;
};

/**
 * 判断绑定 host 是否确认为回环：localhost、任意 127.0.0.0/8 的 IPv4
 * （含 IPv4-mapped 形式）或 ::1。通配、LAN、IPv6 本地地址、主机名等
 * 无法证明是回环的情况一律返回 false。
 */
export const isLoopbackBindHost = (host) => {
  const normalized = normalizeIpv4MappedAddress(host);
  if (!normalized) return false;
  if (normalized === 'localhost') return true;
  if (isLoopbackIpv4(normalized)) return true;
  return net.isIP(normalized) === 6 && normalized === '::1';
};

/** isLoopbackBindHost 的否定：绑定该 host 即暴露到网络（回环以外的一切情况）。 */
export const isNetworkExposedBindHost = (host) => !isLoopbackBindHost(host);

/** 读取 OMPCHAMBER_ALLOW_UNAUTHENTICATED_LAN=true 逃生开关：用户显式接受未认证局域网暴露的风险。 */
export const isUnsafeUnauthenticatedLanAllowed = (env = process.env) =>
  env?.OMPCHAMBER_ALLOW_UNAUTHENTICATED_LAN === 'true';

/** 读取 OMPCHAMBER_DEV_SERVER=true 显式开发服务器标记（NODE_ENV=development 不算数）。 */
export const isDevelopmentServer = (env = process.env) =>
  env?.OMPCHAMBER_DEV_SERVER === 'true';


/**
 * 构造“未认证却绑定到网络地址”的拒绝文案：提示先设置 --ui-password /
 * OMPCHAMBER_UI_PASSWORD 再暴露 LAN，或设置
 * OMPCHAMBER_ALLOW_UNAUTHENTICATED_LAN=true 自担风险。host 缺省时用通用措辞。
 */
export const getUnauthenticatedLanErrorMessage = (host) =>
  `OMPChamber refuses to bind to ${host || 'a network-exposed host'} without UI authentication. `
  + 'Set --ui-password or OMPCHAMBER_UI_PASSWORD before exposing it over LAN, '
  + 'or set OMPCHAMBER_ALLOW_UNAUTHENTICATED_LAN=true to accept the risk.';
