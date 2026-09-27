/**
 * 终端主题查询应答：在服务端代答 PTY 中 shell/程序发出的明暗模式
 * （DEC 私有模式 2031 与 996/997 查询）、前景/背景颜色（OSC 10/11）、
 * 主设备属性（DA1）与 Kitty 键盘协议查询，让尚无渲染器附着的
 * shell 启动握手不被查询超时阻塞。解析器跨 PTY chunk 安全。
 */
/** 客户端请求开启 2031 明暗模式上报的序列。 */
const MODE_SET = '\u001b[?2031h';
/** 客户端请求关闭 2031 模式的序列。 */
const MODE_RESET = '\u001b[?2031l';
/** 查询 2031 模式当前状态（DECRQM 形态）。 */
const CAPABILITY_QUERY = '\u001b[?2031$p';
/** 直接询问当前明暗（light/dark）的两种 DSR 序列。 */
const MODE_QUERIES = ['\u001b[?996n', '\u001b[?997n'];
// （中文说明）DA1 查询序列：浏览器终端未附着时 shell 也会先问这一句。
// Fish asks this before an unattached browser terminal can reply.
const PRIMARY_DEVICE_ATTRIBUTE_QUERIES = ['\u001b[c', '\u001b[0c'];
/** 保守的 VT100 DA1 应答，足以让 Fish 等不阻塞启动。 */
const PRIMARY_DEVICE_ATTRIBUTE_RESPONSE = '\u001b[?1;2c';
// （中文说明）Kitty 键盘协议查询：以 flags=0 应答让 Zellij 类客户端立即回退。
// Kitty keyboard protocol queries. Zellij-class clients block on the reply;
// answering flags=0 ("no enhancements") lets them fall back immediately.
const KITTY_PRIMARY_QUERY = '\u001b[?u';
/** Kitty 主查询应答：不启用任何增强。 */
const KITTY_PRIMARY_RESPONSE = '\u001b[?0u';
/** Kitty 次查询（能力报告询问）序列。 */
const KITTY_SECONDARY_QUERY = '\u001b[?>u';
/** Kitty 次查询应答：无已报告的能力。 */
const KITTY_SECONDARY_RESPONSE = '\u001b[?>0;0u';
/** OSC 10/11 颜色查询（BEL 与 ST 两种终结符形态，code 标记前景/背景）。 */
const OSC_QUERIES = [10, 11].flatMap((code) => [
  { sequence: `\u001b]${code};?\u0007`, code },
  { sequence: `\u001b]${code};?\u001b\\`, code },
]);
/** 需识别的全部控制序列清单；其最大长度决定 pending 尾部保留窗口。 */
const CONTROL_SEQUENCES = [
  MODE_SET,
  MODE_RESET,
  CAPABILITY_QUERY,
  ...MODE_QUERIES,
  ...PRIMARY_DEVICE_ATTRIBUTE_QUERIES,
  ...OSC_QUERIES.map(({ sequence }) => sequence),
];

/** 解析 #rgb/#rrggbb 或 rgb()/rgba() 颜色为 [r,g,b]；非法输入返回 null。 */
const parseColor = (value) => {
  if (typeof value !== 'string') return null;
  const hex = value.match(/^#([0-9a-f]{3}|[0-9a-f]{6})$/i)?.[1];
  if (hex) {
    const expanded = hex.length === 3 ? [...hex].map((part) => part + part).join('') : hex;
    return [0, 2, 4].map((offset) => Number.parseInt(expanded.slice(offset, offset + 2), 16));
  }
  const rgb = value.match(/^rgba?\(\s*(\d+)\s*,\s*(\d+)\s*,\s*(\d+)/i);
  return rgb ? rgb.slice(1, 4).map((part) => Math.min(255, Number(part))) : null;
};

/** 构造 OSC 颜色应答（每通道 8 位放大为 16 位 xxxxxx 格式）；颜色不可解析返回 null。 */
const colorReport = (code, color) => {
  const rgb = parseColor(color);
  if (!rgb) return null;
  const channels = rgb.map((channel) => channel.toString(16).padStart(2, '0').repeat(2));
  return `\u001b]${code};rgb:${channels.join('/')}\u001b\\`;
};

/** 构造当前明暗模式的上报序列（light=2 / dark=1）。 */
export const terminalThemeModeReport = (themeMode) => `\u001b[?997;${themeMode === 'light' ? 2 : 1}n`;

/**
 * 消费一段 PTY 输出中的主题/能力查询并生成应答序列。
 * 顺带跟踪 2031 模式开关；输出不含 ESC 时直接短路返回。
 * 尾部可能是某控制序列的不完整前缀，按最长序列长度保留进 pending。
 * @param {string} pending 上一 chunk 遗留的未完成序列前缀
 * @param {string} data 本 chunk 输出
 * @param {{ themeMode: string, background?: string, foreground?: string, modeEnabled: boolean }} appearance 当前外观
 * @param {{ respondToPrimaryDeviceAttributes?: boolean }} options 仅经典后端允许应答 DA1（conpty.dll 启动期探测会污染输入行）
 * @returns {{ pending: string, responses: string[], modeEnabled: boolean }} 应答序列按线上顺序排列，由调用方写回 PTY
 */
export const consumeTerminalThemeQueries = (
  pending,
  data,
  appearance,
  { respondToPrimaryDeviceAttributes = false } = {},
) => {
  if (!pending && !data.includes('\u001b')) return { pending: '', responses: [], modeEnabled: appearance.modeEnabled === true };
  const input = `${pending}${data}`;
  const responses = [];
  let modeEnabled = appearance.modeEnabled === true;

  for (let index = 0; index < input.length; index += 1) {
    if (input.startsWith(MODE_SET, index)) {
      modeEnabled = true;
      index += MODE_SET.length - 1;
      continue;
    }
    if (input.startsWith(MODE_RESET, index)) {
      modeEnabled = false;
      index += MODE_RESET.length - 1;
      continue;
    }
    if (input.startsWith(CAPABILITY_QUERY, index)) {
      responses.push(`\u001b[?2031;${modeEnabled ? 1 : 2}$y`);
      index += CAPABILITY_QUERY.length - 1;
      continue;
    }
    const modeQuery = MODE_QUERIES.find((query) => input.startsWith(query, index));
    if (modeQuery) {
      responses.push(terminalThemeModeReport(appearance.themeMode));
      index += modeQuery.length - 1;
      continue;
    }
    if (input.startsWith(KITTY_PRIMARY_QUERY, index)) {
      responses.push(KITTY_PRIMARY_RESPONSE);
      index += KITTY_PRIMARY_QUERY.length - 1;
      continue;
    }
    if (input.startsWith(KITTY_SECONDARY_QUERY, index)) {
      responses.push(KITTY_SECONDARY_RESPONSE);
      index += KITTY_SECONDARY_QUERY.length - 1;
      continue;
    }
    const primaryDeviceAttributeQuery = PRIMARY_DEVICE_ATTRIBUTE_QUERIES.find((query) => input.startsWith(query, index));
    if (primaryDeviceAttributeQuery && respondToPrimaryDeviceAttributes) {
      // A shell can ask before any browser terminal is attached. Answer with a
      // conservative VT100 DA1 response so Fish does not block startup for its
      // ten-second query timeout while waiting for a renderer that cannot see it.
      responses.push(PRIMARY_DEVICE_ATTRIBUTE_RESPONSE);
      index += primaryDeviceAttributeQuery.length - 1;
      continue;
    }
    const oscQuery = OSC_QUERIES.find(({ sequence }) => input.startsWith(sequence, index));
    if (oscQuery) {
      const response = colorReport(oscQuery.code, oscQuery.code === 10 ? appearance.foreground : appearance.background);
      if (response) responses.push(response);
      index += oscQuery.sequence.length - 1;
    }
  }

  let nextPending = '';
  const maxLength = Math.max(...CONTROL_SEQUENCES.map((sequence) => sequence.length));
  for (let length = 1; length < Math.min(input.length + 1, maxLength); length += 1) {
    const suffix = input.slice(-length);
    if (CONTROL_SEQUENCES.some((sequence) => sequence.length > length && sequence.startsWith(suffix))) nextPending = suffix;
  }
  return { pending: nextPending, responses, modeEnabled };
};
