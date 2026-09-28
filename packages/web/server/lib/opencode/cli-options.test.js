/**
 * cli-options 模块的单元测试。
 *
 * 验证 serve 端口解析的优先级：显式 --port 标志 > OMPCHAMBER_PORT 环境
 * 变量 > defaultPort；畸形环境变量值与未展开的 shell 模板
 * （${OMPCHAMBER_PORT:-3001}）被忽略并回退默认值。
 */
import { describe, expect, test } from 'vitest';
import { parseServeCliOptions } from './cli-options.js';

// serve CLI 选项的端口解析优先级与容错
describe('parseServeCliOptions port resolution', () => {
  test('falls back to OMPCHAMBER_PORT env when no flag is passed', () => {
    const options = parseServeCliOptions({ argv: [], env: { OMPCHAMBER_PORT: '3902' }, defaultPort: 3000 });
    expect(options.port).toBe(3902);
  });

  test('an explicit --port flag wins over the environment', () => {
    const options = parseServeCliOptions({
      argv: ['--port', '4555'],
      env: { OMPCHAMBER_PORT: '3902' },
      defaultPort: 3000,
    });
    expect(options.port).toBe(4555);
  });

  test('ignores malformed env values and unexpanded shell templates', () => {
    const options = parseServeCliOptions({
      argv: ['--port', '${OMPCHAMBER_PORT:-3001}'],
      env: { OMPCHAMBER_PORT: 'not-a-number' },
      defaultPort: 3000,
    });
    expect(options.port).toBe(3000);
  });

  test('uses the default when nothing is configured', () => {
    const options = parseServeCliOptions({ argv: [], env: {}, defaultPort: 3000 });
    expect(options.port).toBe(3000);
  });
});
