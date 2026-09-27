/**
 * walkthrough 多语言支持的测试：语言标签归一化（平台大小写/地区后缀漂移与
 * 未知标签回退英文）、提示词中的语言指令（只翻译散文，别名与枚举保持英文）、
 * 缓存键按语言隔离，以及生成/读取时语言切换不串缓存与回退语义（无该语言
 * 条目时返回最近一份并如实报告其语言）。
 */
import fs from 'fs';
import os from 'os';
import path from 'path';
import { afterAll, beforeEach, describe, expect, it, vi } from 'vitest';

/** 临时数据目录：在被测模块 import 前设置以隔离落盘。 */
const TEMP_DATA_DIR = fs.mkdtempSync(path.join(os.tmpdir(), 'walkthrough-language-'));
// 先设环境变量再动态导入被测模块。
process.env.OMPCHAMBER_DATA_DIR = TEMP_DATA_DIR;

vi.mock('../git/service.js', () => ({
  getRepositoryRoot: vi.fn(async () => '/repo'),
  getDiff: vi.fn(),
  getRangeDiff: vi.fn(),
  getUntrackedDiffs: vi.fn(async () => []),
  listUntrackedPaths: vi.fn(async () => []),
}));
vi.mock('../small-model/index.js', () => ({
  describeSmallModel: vi.fn(),
  generateSmallModelText: vi.fn(),
}));

/** 语言工具：归一化与英文名。 */
const { normalizeLanguage, languageName } = await import('./languages.js');
/** 提示词构建。 */
const { buildPrompt } = await import('./prompt.js');
/** 缓存键构建。 */
const { buildCacheKey } = await import('./store.js');
/** 被测的生成/读取入口。 */
const { generateWalkthrough, getWalkthrough } = await import('./index.js');
/** small-model mock，供配置模型与回复。 */
const { describeSmallModel, generateSmallModelText } = await import('../small-model/index.js');
/** git service 的 getDiff mock。 */
const { getDiff } = await import('../git/service.js');

/** 测试用来源：working-tree 全部改动。 */
const SOURCE = { kind: 'working-tree', scope: 'all' };

/** 测试用 diff：单文件单 hunk。 */
const PATCH = `diff --git a/src/a.ts b/src/a.ts
--- a/src/a.ts
+++ b/src/a.ts
@@ -1,1 +1,2 @@
+const added = true;
`;

/** 测试用模型回复：一章一停靠点的合法 walkthrough JSON。 */
const RESPONSE = JSON.stringify({
  title: 'Change',
  focus: 'why',
  chapters: [{
    title: 'Data',
    icon: 'doc',
    blurb: '',
    stops: [{ title: 'Adds a flag', hunks: ['h1'], importance: 'normal', prose: 'It adds a flag.' }],
  }],
});

/** buildPrompt 的最小输入。 */
const PROMPT_INPUT = {
  digest: { files: [] },
  fileCount: 1,
  hunkCount: 1,
  source: SOURCE,
};

/** 构造缓存键用的最小 files 输入。 */
const FILES = [{ path: 'src/a.ts', status: 'modified', hunks: [{ id: 'unstaged:src/a.ts:abcd1234' }] }];
/** 生成指定语言下的缓存键，便于比较不同语言的键是否分离。 */
const keyFor = (language) => buildCacheKey({
  repoRoot: '/repo',
  sourceKey: 'working-tree:all',
  providerID: 'anthropic',
  modelID: 'claude-haiku-4-5',
  language,
  files: FILES,
});

/** 标签归一化：接受界面标签、容忍平台漂移、未知回退英文。 */
describe('normalizeLanguage', () => {
  it('accepts the tags the interface uses', () => {
    expect(normalizeLanguage('uk')).toBe('uk');
    expect(normalizeLanguage('zh-TW')).toBe('zh-TW');
    expect(normalizeLanguage('pt-BR')).toBe('pt-BR');
  });

  it('tolerates case and separator drift from a platform locale', () => {
    expect(normalizeLanguage('uk-UA')).toBe('uk');
    expect(normalizeLanguage('pt_br')).toBe('pt-BR');
    expect(normalizeLanguage('ja-JP')).toBe('ja');
  });

  // A language preference is about prose. Refusing to write a walkthrough over
  // an unrecognised tag would be a worse answer than writing it in English.
  it('falls back to English rather than failing', () => {
    expect(normalizeLanguage('kl')).toBe('en');
    expect(normalizeLanguage('')).toBe('en');
    expect(normalizeLanguage(undefined)).toBe('en');
    expect(normalizeLanguage({ toString: () => 'uk' })).toBe('en');
  });

  it('names languages in English, matching the language of the prompt', () => {
    expect(languageName('uk')).toBe('Ukrainian');
    expect(languageName('nope')).toBe('English');
  });
});

/** 提示词语言指令：默认英文、按需翻译散文、非散文部分保持英文。 */
describe('prompt language instruction', () => {
  it('says nothing when the prompt language is already the output language', () => {
    const { system } = buildPrompt({ ...PROMPT_INPUT, language: 'en' });
    expect(system).not.toMatch(/Write all prose/);
  });

  it('asks for prose in the chosen language', () => {
    const { system } = buildPrompt({ ...PROMPT_INPUT, language: 'uk' });
    expect(system).toMatch(/Write all prose in Ukrainian/);
  });

  // Aliases are keys the server resolves back to hunk ids and icon/importance
  // are validated against fixed English values, so a translated one is dropped
  // by the normalizer — silently losing an anchor or a style.
  it('holds back the parts that are not prose', () => {
    const { system } = buildPrompt({ ...PROMPT_INPUT, language: 'ja' });
    expect(system).toMatch(/Keep these in English exactly as given/);
    expect(system).toMatch(/hunk aliases/);
    expect(system).toMatch(/"importance"/);
  });

  it('defaults to English when no language is passed', () => {
    expect(buildPrompt(PROMPT_INPUT).system).toBe(buildPrompt({ ...PROMPT_INPUT, language: 'en' }).system);
  });
});

/** 缓存键：不同语言必须得到不同的键。 */
describe('cache key', () => {
  // Without the language in the key, asking for a translation is answered with
  // the untranslated entry that was already there.
  it('separates walkthroughs written in different languages', () => {
    expect(keyFor('uk')).not.toBe(keyFor('en'));
  });

  it('is stable for the same language', () => {
    expect(keyFor('uk')).toBe(keyFor('uk'));
  });
});

/** 端到端生成与读取：语言随结果记录、按语言取缓存、来回切换零生成、回退诚实。 */
describe('generating in a language', () => {
  beforeEach(() => {
    fs.rmSync(path.join(TEMP_DATA_DIR, 'walkthroughs'), { recursive: true, force: true });
    describeSmallModel.mockResolvedValue({
      providerID: 'anthropic',
      modelID: 'claude-haiku-4-5',
      source: 'config',
      inputCharBudget: 1_000_000,
      structuredOutput: true,
    });
    getDiff.mockImplementation(async (_dir, options) => (options?.staged ? '' : PATCH));
    generateSmallModelText.mockReset();
    generateSmallModelText.mockResolvedValue({ text: RESPONSE });
  });

  afterAll(() => {
    fs.rmSync(TEMP_DATA_DIR, { recursive: true, force: true });
  });

  it('sends the instruction and records the language with the result', async () => {
    const result = await generateWalkthrough({ directory: '/repo', source: SOURCE, language: 'uk' });

    expect(generateSmallModelText.mock.calls[0][0].system).toMatch(/Write all prose in Ukrainian/);
    expect(result.language).toBe('uk');
  });

  it('does not serve one language from the other language cache entry', async () => {
    await generateWalkthrough({ directory: '/repo', source: SOURCE, language: 'uk' });
    generateSmallModelText.mockClear();

    const english = await generateWalkthrough({ directory: '/repo', source: SOURCE, language: 'en' });

    expect(english.fromCache).toBeFalsy();
    expect(generateSmallModelText).toHaveBeenCalledTimes(1);
    expect(english.language).toBe('en');
  });

  // Switching away and back must not cost a second generation: the earlier
  // walkthrough is still addressed by its own key.
  it('returns the earlier language from cache when it is asked for again', async () => {
    await generateWalkthrough({ directory: '/repo', source: SOURCE, language: 'uk' });
    await generateWalkthrough({ directory: '/repo', source: SOURCE, language: 'en' });
    generateSmallModelText.mockClear();

    const back = await generateWalkthrough({ directory: '/repo', source: SOURCE, language: 'uk' });

    expect(back.fromCache).toBe(true);
    expect(back.language).toBe('uk');
    expect(generateSmallModelText).not.toHaveBeenCalled();
  });

  // The pointer only knows what was generated here last. After a language
  // switch that is the answer to a different question, and reading it instead
  // left the panel showing English while the picker said Ukrainian — with the
  // Ukrainian walkthrough sitting unused in the cache.
  it('reads back the walkthrough written in the language being asked for', async () => {
    await generateWalkthrough({ directory: '/repo', source: SOURCE, language: 'uk' });
    await generateWalkthrough({ directory: '/repo', source: SOURCE, language: 'en' });
    generateSmallModelText.mockClear();

    const ukrainian = await getWalkthrough({ directory: '/repo', source: SOURCE, language: 'uk' });

    expect(ukrainian.language).toBe('uk');
    expect(ukrainian.walkthrough).toBeTruthy();
    expect(generateSmallModelText).not.toHaveBeenCalled();
  });

  it('switches back and forth without generating anything', async () => {
    await generateWalkthrough({ directory: '/repo', source: SOURCE, language: 'uk' });
    await generateWalkthrough({ directory: '/repo', source: SOURCE, language: 'ja' });
    generateSmallModelText.mockClear();

    expect((await getWalkthrough({ directory: '/repo', source: SOURCE, language: 'uk' })).language).toBe('uk');
    expect((await getWalkthrough({ directory: '/repo', source: SOURCE, language: 'ja' })).language).toBe('ja');
    expect((await getWalkthrough({ directory: '/repo', source: SOURCE, language: 'uk' })).language).toBe('uk');
    expect(generateSmallModelText).not.toHaveBeenCalled();
  });

  // Falling back is still right: an English review beats an empty panel, and
  // the response says which language it is in so the panel can be honest.
  it('falls back to the last walkthrough when none exists in that language', async () => {
    await generateWalkthrough({ directory: '/repo', source: SOURCE, language: 'en' });

    const korean = await getWalkthrough({ directory: '/repo', source: SOURCE, language: 'ko' });

    expect(korean.walkthrough).toBeTruthy();
    expect(korean.language).toBe('en');
  });

  it('treats an unknown language as English rather than failing the request', async () => {
    const result = await generateWalkthrough({ directory: '/repo', source: SOURCE, language: 'kl' });

    expect(result.language).toBe('en');
    expect(generateSmallModelText.mock.calls[0][0].system).not.toMatch(/Write all prose/);
  });
});
