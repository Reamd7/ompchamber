/**
 * （中文套件说明）终端主题查询应答测试：OpenTUI 启动握手的完整代答、
 * 跨 chunk 拆分查询的去重、重复查询的顺序应答、DA1 回退开关，
 * 以及 Kitty 键盘协议的 flags=0 应答。
 */
import { describe, expect, test } from 'vitest';
import { consumeTerminalThemeQueries } from './theme-response.js';

/** 测试用的浅色外观（themeMode/前景/背景/模式开关）。 */
const lightAppearance = {
  themeMode: 'light',
  foreground: '#1b1b1b',
  background: '#faf8f0',
  modeEnabled: false,
};

// 终端主题应答：握手代答、跨 chunk 续写、查询顺序与 DA1/Kitty 回退。
describe('terminal theme responses', () => {
  test('answers the complete OpenTUI startup handshake', () => {
    const result = consumeTerminalThemeQueries(
      '',
      '\u001b[?2031h\u001b]10;?\u001b\\\u001b]11;?\u001b\\\u001b[?2031$p',
      lightAppearance,
    );
    expect(result).toEqual({
      pending: '',
      modeEnabled: true,
      responses: [
        '\u001b]10;rgb:1b1b/1b1b/1b1b\u001b\\',
        '\u001b]11;rgb:fafa/f8f8/f0f0\u001b\\',
        '\u001b[?2031;1$y',
      ],
    });
  });

  test('handles a query split across PTY output chunks without duplicating it', () => {
    const first = consumeTerminalThemeQueries('', '\u001b]11;', { ...lightAppearance, themeMode: 'dark' });
    const second = consumeTerminalThemeQueries(first.pending, '?\u001b\\', { ...lightAppearance, themeMode: 'dark', modeEnabled: first.modeEnabled });
    const third = consumeTerminalThemeQueries(second.pending, 'x', { ...lightAppearance, themeMode: 'dark', modeEnabled: second.modeEnabled });
    expect(second.responses).toEqual(['\u001b]11;rgb:fafa/f8f8/f0f0\u001b\\']);
    expect(second.pending).toBe('');
    expect(third.responses).toEqual([]);
  });

  test('answers every repeated query in wire order', () => {
    const result = consumeTerminalThemeQueries('', '\u001b[?996n\u001b[?996n\u001b]10;?\u0007\u001b]10;?\u0007', lightAppearance);
    expect(result.responses).toEqual([
      '\u001b[?997;2n',
      '\u001b[?997;2n',
      '\u001b]10;rgb:1b1b/1b1b/1b1b\u001b\\',
      '\u001b]10;rgb:1b1b/1b1b/1b1b\u001b\\',
    ]);
  });

  test('answers a primary device attribute query when the fallback is enabled', () => {
    const attached = consumeTerminalThemeQueries('', '\u001b[0c', lightAppearance);
    const unattached = consumeTerminalThemeQueries('', '\u001b[0c', lightAppearance, {
      respondToPrimaryDeviceAttributes: true,
    });

    expect(attached.responses).toEqual([]);
    expect(unattached.responses).toEqual(['\u001b[?1;2c']);
  });

  test('answers a primary device attribute query split across PTY chunks', () => {
    const first = consumeTerminalThemeQueries('', '\u001b[0', lightAppearance, {
      respondToPrimaryDeviceAttributes: true,
    });
    const second = consumeTerminalThemeQueries(first.pending, 'c', {
      ...lightAppearance,
      modeEnabled: first.modeEnabled,
    }, { respondToPrimaryDeviceAttributes: true });

    expect(first.pending).toBe('\u001b[0');
    expect(second.responses).toEqual(['\u001b[?1;2c']);
  });

  test('answers kitty keyboard protocol queries with flags=0 so clients fall back', () => {
    const result = consumeTerminalThemeQueries('', '\u001b[?u\u001b[?>u', lightAppearance);
    expect(result.responses).toEqual(['\u001b[?0u', '\u001b[?>0;0u']);
    expect(result.pending).toBe('');
  });
});
