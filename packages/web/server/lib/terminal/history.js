/**
 * 终端回放历史清洗：从 PTY 原始输出中剥离"设备/颜色查询与应答"类
 * 控制序列，保留渲染必需的显示控制（SGR、光标移动、普通 OSC 标题等），
 * 避免 snapshot.history 回放时重放对 shell 的查询副作用。
 * 手写解析、零依赖，且跨 PTY chunk 安全（不完整序列暂存 pending）。
 */
/** 判断字节码是否为 CSI 序列的最终字节（0x40-0x7e）。 */
const isCsiFinalByte = (code) => code >= 0x40 && code <= 0x7e;
/** 判定 CSI 序列是否属于查询应答类（DSR/CPR 光标报告、DA/DA2 设备属性、DECRQM 2031 查询、2031 模式开关），是则从历史中剔除。 */
const shouldStripCsi = (body, finalByte) =>
  finalByte === 'n'
  || (finalByte === 'R' && /^[0-9;?]*$/.test(body))
  || (finalByte === 'c' && /^[>0-9;?]*$/.test(body))
  || ((finalByte === 'p' || finalByte === 'y') && /^\?2031(?:;[0-9]+)?\$$/.test(body))
  || ((finalByte === 'h' || finalByte === 'l') && body === '?2031');
/** 判定 OSC 正文是否为颜色查询或颜色应答（10/11/12 后跟 ? 或 rgb:），是则剔除。 */
const shouldStripOsc = (content) => /^(10|11|12);(?:\?|rgb:)/.test(content);
/** 去掉 OSC/DCS 等字符串序列的结尾符（ST/BEL/SCI），返回正文。 */
const stripTerminator = (value) => {
  if (value.endsWith('\u001b\\')) return value.slice(0, -2);
  return value.endsWith('\u0007') || value.endsWith('\u009c') ? value.slice(0, -1) : value;
};
/** 从 start 起查找字符串序列的结尾（BEL 0x07、SCI 0x9c 或 ST ESC-\\），找不到返回 null（说明序列尚未完整到达）。 */
const findStringEnd = (input, start) => {
  for (let index = start; index < input.length; index += 1) {
    const code = input.charCodeAt(index);
    if (code === 0x07 || code === 0x9c) return index + 1;
    if (code === 0x1b && input.charCodeAt(index + 1) === 0x5c) return index + 2;
  }
  return null;
};
/** 解析简单 escape 序列（ESC + 可选中间字节 0x20-0x2f + 最终字节）的结束位置；无法确认完整时返回 null。 */
const findEscapeEnd = (input, start) => {
  let cursor = start;
  while (cursor < input.length && input.charCodeAt(cursor) >= 0x20 && input.charCodeAt(cursor) <= 0x2f) cursor += 1;
  if (cursor >= input.length) return null;
  return input.charCodeAt(cursor) >= 0x30 && input.charCodeAt(cursor) <= 0x7e ? cursor + 1 : start + 1;
};

/**
 * 清洗一段 PTY 输出，产出可安全进入回放历史的可见文本。
 * 逐字符扫描：识别 CSI/OSC/DCS/PM/APC/SOS 与简单 escape 序列，
 * 属于查询应答类的被剔除，其余原样保留；未完成的尾部序列暂存返回，
 * 由调用方在下一 chunk 携带 pending 续上。
 * @param {string} pending 上一 chunk 遗留的未完成序列前缀（首次传 ''）
 * @param {string} data 本 chunk 的原始 PTY 输出
 * @returns {{ visible: string, pending: string }} 保留的可见文本与待续写前缀
 */
export const sanitizeTerminalHistoryChunk = (pending, data) => {
  const input = `${pending}${data}`;
  let visible = '';
  let index = 0;
  while (index < input.length) {
    const code = input.charCodeAt(index);
    if (code === 0x1b) {
      const next = input.charCodeAt(index + 1);
      if (Number.isNaN(next)) return { visible, pending: input.slice(index) };
      if (next === 0x5b) {
        let cursor = index + 2;
        while (cursor < input.length && !isCsiFinalByte(input.charCodeAt(cursor))) cursor += 1;
        if (cursor >= input.length) return { visible, pending: input.slice(index) };
        const sequence = input.slice(index, cursor + 1);
        if (!shouldStripCsi(input.slice(index + 2, cursor), input[cursor])) visible += sequence;
        index = cursor + 1;
        continue;
      }
      if (next === 0x5d || next === 0x50 || next === 0x5e || next === 0x5f) {
        const end = findStringEnd(input, index + 2);
        if (end === null) return { visible, pending: input.slice(index) };
        const sequence = input.slice(index, end);
        const content = stripTerminator(input.slice(index + 2, end));
        if (next !== 0x5d || !shouldStripOsc(content)) visible += sequence;
        index = end;
        continue;
      }
      const end = findEscapeEnd(input, index + 1);
      if (end === null) return { visible, pending: input.slice(index) };
      visible += input.slice(index, end);
      index = end;
      continue;
    }
    if (code === 0x9b) {
      let cursor = index + 1;
      while (cursor < input.length && !isCsiFinalByte(input.charCodeAt(cursor))) cursor += 1;
      if (cursor >= input.length) return { visible, pending: input.slice(index) };
      const sequence = input.slice(index, cursor + 1);
      if (!shouldStripCsi(input.slice(index + 1, cursor), input[cursor])) visible += sequence;
      index = cursor + 1;
      continue;
    }
    if (code === 0x9d || code === 0x90 || code === 0x9e || code === 0x9f) {
      const end = findStringEnd(input, index + 1);
      if (end === null) return { visible, pending: input.slice(index) };
      const sequence = input.slice(index, end);
      const content = stripTerminator(input.slice(index + 1, end));
      if (code !== 0x9d || !shouldStripOsc(content)) visible += sequence;
      index = end;
      continue;
    }
    visible += input[index];
    index += 1;
  }
  return { visible, pending: '' };
};
