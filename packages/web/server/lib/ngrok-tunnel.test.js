/**
 * ngrok-tunnel 纯函数部分的单元测试套件（bun:test）：从 JSON/文本两类输出
 * 中提取公网 URL，以及把 ngrok 输出提炼为一句错误摘要的优先级规则。
 */
import { describe, expect, it } from 'bun:test';

import {
  extractNgrokPublicUrlFromText,
  summarizeNgrokOutput,
} from './ngrok-tunnel.js';

/** extractNgrokPublicUrlFromText：JSON 行与纯文本两种来源的 URL 提取与过滤。 */
describe('extractNgrokPublicUrlFromText', () => {
  it('extracts public URL from ngrok JSON logs', () => {
    const url = extractNgrokPublicUrlFromText('{"lvl":"info","msg":"started tunnel","url":"https://demo.ngrok-free.app"}\n');

    expect(url).toBe('https://demo.ngrok-free.app');
  });

  it('extracts public URL from ngrok text output', () => {
    const url = extractNgrokPublicUrlFromText('Forwarding https://demo.ngrok-free.app -> http://127.0.0.1:3000');

    expect(url).toBe('https://demo.ngrok-free.app');
  });

  it('ignores non-ngrok URLs', () => {
    const url = extractNgrokPublicUrlFromText('{"url":"https://example.com"}\n');

    expect(url).toBeNull();
  });
});

/** summarizeNgrokOutput：错误摘要的选取优先级（JSON 错误 > ERROR 行 > 末行）。 */
describe('summarizeNgrokOutput', () => {
  it('prefers actionable JSON error details over trailing help text', () => {
    const summary = summarizeNgrokOutput([
      '{"err":"authentication failed: Your ngrok-agent version \\"3.3.1\\" is too old. The minimum supported agent version for your account is \\"3.20.0\\".\\r\\n\\r\\nERR_NGROK_121\\r\\n","lvl":"crit","msg":"command failed"}',
      'NAME:',
      '  http - start an HTTP tunnel',
    ]);

    expect(summary).toContain('Your ngrok-agent version "3.3.1" is too old');
    expect(summary).toContain('ERR_NGROK_121');
  });

  it('skips bare ERROR lines and keeps the useful error text', () => {
    const summary = summarizeNgrokOutput([
      'ERROR:',
      'ERROR:  authentication failed',
      'ERROR:',
      'ERROR:  ERR_NGROK_121',
    ]);

    expect(summary).toBe('authentication failed ERR_NGROK_121');
  });
});
