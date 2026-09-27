import fs from 'fs';
import { fileURLToPath } from 'url';
import { describe, expect, it } from 'vitest';
import { normalizeLanguage, __testing } from './languages.js';

// The languages a walkthrough may be written in have to agree with the locales
// the interface ships, because the picker offers exactly those and the server
// decides what the prompt asks for. The two lists cannot be one list — the
// server cannot import from `packages/ui` — so they are compared here instead.
//
// This exists because German was added to the interface and not here. Nothing
// broke loudly: the picker offered Deutsch, `normalizeLanguage` quietly resolved
// it to English, and a German user paid for a walkthrough written in English
// while the picker still said Deutsch. A drift this quiet needs a test, not
// vigilance.
/**
 * 语言清单一致性测试套件：服务端无法 import packages/ui，故直接读取其
 * runtime.ts 源码提取 LOCALES，与服务端清单双向比对，并验证每个 locale
 * 都能归一化为其自身、每个英文名可直接写入 prompt。
 */
/** UI 侧 Locale 定义文件的绝对路径（自本测试文件向上四级进入 packages/ui）。 */
const RUNTIME_TS = fileURLToPath(new URL('../../../../ui/src/lib/i18n/runtime.ts', import.meta.url));

/**
 * 从 runtime.ts 源码正则提取 LOCALES 数组中的字符串元素；找不到该导出时
 * 直接抛错，让定义被改名或移动显式失败，而不是静默通过。
 *
 * @returns UI 提供的全部 locale 标签
 */
const interfaceLocales = () => {
  const source = fs.readFileSync(RUNTIME_TS, 'utf8');
  const match = source.match(/export const LOCALES = \[([^\]]*)\]/);
  if (!match) throw new Error(`Could not find LOCALES in ${RUNTIME_TS}`);
  return match[1]
    .split(',')
    .map((entry) => entry.trim().replace(/^['"]|['"]$/g, ''))
    .filter(Boolean);
};

// 双向一致：不缺（picker 有而服务端无会静默回落英文）、不多（服务端有但 picker 不可达）。
describe('supported languages', () => {
  it('covers every locale the interface offers', () => {
    const missing = interfaceLocales().filter((locale) => !Object.hasOwn(__testing.LANGUAGE_NAMES, locale));

    expect(missing, `add these to LANGUAGE_NAMES in languages.js: ${missing.join(', ')}`).toEqual([]);
  });

  it('offers nothing the interface cannot label', () => {
    const locales = new Set(interfaceLocales());
    const extra = Object.keys(__testing.LANGUAGE_NAMES).filter((tag) => !locales.has(tag));

    // A language here that the interface does not know is not harmful, but it
    // is unreachable: the picker is built from the interface list.
    expect(extra, `unreachable from the picker: ${extra.join(', ')}`).toEqual([]);
  });

  it('resolves every interface locale to itself rather than to the default', () => {
    for (const locale of interfaceLocales()) {
      expect(normalizeLanguage(locale)).toBe(locale);
    }
  });

  it('names every supported language in English, for the prompt', () => {
    for (const [tag, name] of Object.entries(__testing.LANGUAGE_NAMES)) {
      expect(name, tag).toMatch(/^[A-Z][A-Za-z ]+$/);
    }
  });
});
