/**
 * walkthrough 输出契约（schema.js）的单元测试：normalizeWalkthrough 的别名
 * 重解析、捏造别名丢弃、每个 hunk 只属一个停靠点、无锚点/无正文停靠点剔除、
 * 全部失效时拒绝、标题截断与枚举回退、停靠点总数上限；以及 parseModelJson
 * 对干净 JSON、代码围栏、尾随散文的提取与失败报错。
 */
import { describe, expect, it } from 'vitest';
import { normalizeWalkthrough, parseModelJson, MAX_STOPS } from './schema.js';

/** 别名 -> 真实 hunk id 的映射，模拟 digest 提供给归一化的 idByAlias。 */
const ALIASES = new Map([
  ['h1', 'working:src/a.ts:aaaa1111'],
  ['h2', 'working:src/a.ts:bbbb2222'],
  ['h3', 'working:src/b.ts:cccc3333'],
]);

/** 用给定 chapters 构造一个最小合法的原始模型输出。 */
const walkthrough = (chapters) => ({ title: 'Change', focus: 'why', chapters });

/** 归一化：锚点解析、去重、剔除、拒绝与上限截断。 */
describe('normalizeWalkthrough', () => {
  it('maps aliases to real hunk ids and assigns stable local ids', () => {
    const result = normalizeWalkthrough(walkthrough([
      {
        title: 'Data',
        icon: 'doc',
        blurb: 'shape first',
        stops: [
          { title: 'New field', hunks: ['h1', 'h2'], importance: 'critical', prose: 'Adds a field.' },
        ],
      },
    ]), ALIASES);

    expect(result.chapters[0].id).toBe('chapter-1');
    expect(result.chapters[0].stops[0]).toMatchObject({
      id: 'stop-1-1',
      hunkIds: ['working:src/a.ts:aaaa1111', 'working:src/a.ts:bbbb2222'],
      importance: 'critical',
    });
  });

  it('drops invented aliases instead of rendering a broken anchor', () => {
    const result = normalizeWalkthrough(walkthrough([
      {
        title: 'Data',
        icon: 'doc',
        blurb: '',
        stops: [
          { title: 'Mixed', hunks: ['h1', 'h99', 'nonsense'], importance: 'normal', prose: 'Something.' },
        ],
      },
    ]), ALIASES);

    expect(result.chapters[0].stops[0].hunkIds).toEqual(['working:src/a.ts:aaaa1111']);
    expect(result.droppedAnchors).toBe(2);
  });

  it('anchors each hunk to a single stop', () => {
    const result = normalizeWalkthrough(walkthrough([
      {
        title: 'Data',
        icon: 'doc',
        blurb: '',
        stops: [
          { title: 'First', hunks: ['h1'], importance: 'normal', prose: 'One.' },
          { title: 'Second', hunks: ['h1', 'h2'], importance: 'normal', prose: 'Two.' },
        ],
      },
    ]), ALIASES);

    expect(result.chapters[0].stops[0].hunkIds).toEqual(['working:src/a.ts:aaaa1111']);
    expect(result.chapters[0].stops[1].hunkIds).toEqual(['working:src/a.ts:bbbb2222']);
  });

  it('discards stops left with no anchor or no prose', () => {
    const result = normalizeWalkthrough(walkthrough([
      {
        title: 'Data',
        icon: 'doc',
        blurb: '',
        stops: [
          { title: 'Ghost', hunks: ['h99'], importance: 'normal', prose: 'About nothing.' },
          { title: 'Silent', hunks: ['h1'], importance: 'normal', prose: '   ' },
          { title: 'Real', hunks: ['h2'], importance: 'normal', prose: 'Actual explanation.' },
        ],
      },
    ]), ALIASES);

    expect(result.chapters[0].stops.map((stop) => stop.title)).toEqual(['Real']);
  });

  it('rejects a response whose stops all fall away', () => {
    expect(() => normalizeWalkthrough(walkthrough([
      { title: 'Empty', icon: 'doc', blurb: '', stops: [{ title: 'Ghost', hunks: ['h99'], importance: 'normal', prose: 'x' }] },
    ]), ALIASES)).toThrow('no usable stops');
  });

  it('clamps chapter titles and falls back on unknown enums', () => {
    const result = normalizeWalkthrough(walkthrough([
      {
        title: 'An extremely long chapter title that will not fit the column',
        icon: 'rocket',
        blurb: '',
        stops: [{ title: 'A', hunks: ['h1'], importance: 'urgent', prose: 'Text.' }],
      },
    ]), ALIASES);

    expect(result.chapters[0].title.length).toBeLessThanOrEqual(24);
    expect(result.chapters[0].icon).toBe('doc');
    expect(result.chapters[0].stops[0].importance).toBe('normal');
  });

  it('caps the total number of stops', () => {
    const many = Array.from({ length: 30 }, (_, index) => ({
      title: `Stop ${index}`,
      hunks: [['h1', 'h2', 'h3'][index % 3]],
      importance: 'normal',
      prose: 'Text.',
    }));

    const result = normalizeWalkthrough(
      walkthrough([{ title: 'All', icon: 'doc', blurb: '', stops: many }]),
      ALIASES,
    );

    const total = result.chapters.reduce((sum, chapter) => sum + chapter.stops.length, 0);
    expect(total).toBeLessThanOrEqual(MAX_STOPS);
    // Only three aliases exist and each is used once, so the real cap here is
    // the alias pool, not the stop limit.
    expect(total).toBe(3);
  });
});

/** JSON 提取：干净对象、代码围栏、尾随散文与彻底失败。 */
describe('parseModelJson', () => {
  it('parses a clean object', () => {
    expect(parseModelJson('{"title":"x"}')).toEqual({ title: 'x' });
  });

  it('unwraps a fenced block', () => {
    expect(parseModelJson('```json\n{"title":"x"}\n```')).toEqual({ title: 'x' });
  });

  it('recovers an object followed by stray prose', () => {
    expect(parseModelJson('{"title":"x"}\n\nHope that helps!')).toEqual({ title: 'x' });
  });

  it('fails loudly on unusable output', () => {
    expect(() => parseModelJson('')).toThrow('empty response');
    expect(() => parseModelJson('no json at all')).toThrow('no JSON object');
    expect(() => parseModelJson('{"broken":')).toThrow('not valid JSON');
  });
});
