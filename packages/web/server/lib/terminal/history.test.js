/**
 * （中文套件说明）终端回放历史清洗测试：剔除设备/颜色查询问答但保留
 * 显示控制、跨 chunk 的不完整序列续写，以及普通 OSC 标题的保留。
 */
import { describe, expect, it } from 'vitest';
import { sanitizeTerminalHistoryChunk } from './history.js';

// 终端回放历史：查询序列剔除、显示控制保留与跨 chunk 续写。
describe('terminal replay history', () => {
  it('removes device and color query exchanges while preserving display controls', () => {
    const input = `before\u001b[6n\u001b[12;40R\u001b[>0c\u001b[?2031h\u001b[?2031$p\u001b[?2031;1$y\u001b]10;?\u0007\u001b[31mred\u001b[0mafter`;
    expect(sanitizeTerminalHistoryChunk('', input)).toEqual({ visible: 'before\u001b[31mred\u001b[0mafter', pending: '' });
  });

  it('carries incomplete control sequences across PTY chunks', () => {
    const first = sanitizeTerminalHistoryChunk('', 'text\u001b]11;');
    expect(first).toEqual({ visible: 'text', pending: '\u001b]11;' });
    expect(sanitizeTerminalHistoryChunk(first.pending, '?\u001b\\next')).toEqual({ visible: 'next', pending: '' });
  });

  it('preserves ordinary OSC titles and split UTF-16 text', () => {
    expect(sanitizeTerminalHistoryChunk('', '\u001b]0;title\u0007ok')).toEqual({ visible: '\u001b]0;title\u0007ok', pending: '' });
  });
});
