/**
 * Walkthrough 的输出契约：模型必须产出的 JSON 形状（responseSchema，供
 * structured output）、对模型实际输出的归一化（normalizeWalkthrough，把锚点
 * 重新解析回真实 hunk id、丢弃失配项），以及从散文/代码围栏包裹中提取 JSON
 * 的解析器（parseModelJson）。模型只被信任写散文和分组，所有锚点都对回
 * digest 校验。
 */
// Shape of the walkthrough the model must produce, plus normalization of what
// it actually produced. The model is only ever trusted for prose and grouping —
// every anchor it returns is re-resolved against the digest here, and anything
// that does not resolve is dropped rather than rendered as a broken stop.

/** walkthrough 数据结构版本；变更即全量缓存失效。 */
export const WALKTHROUGH_VERSION = 1;

// Bumping this invalidates every cached walkthrough, which is the point: a
// changed prompt produces different output and old entries would misrepresent
// what the current code would say.
/** 提示词版本；提示词一变缓存即失效，旧条目不再代表当前代码会说的话。 */
export const PROMPT_VERSION = 3;

/** 最多章节数，归一化时截断多余章节。 */
export const MAX_CHAPTERS = 6;
/** 全文停靠点总数上限（跨章节累计）。 */
export const MAX_STOPS = 16;
/** 每个停靠点最多锚定的 hunk 数。 */
export const MAX_HUNKS_PER_STOP = 14;
/** 章节标题的最大字符数。 */
export const MAX_CHAPTER_TITLE_CHARS = 24;

/** 允许的章节图标枚举值。 */
const CHAPTER_ICONS = ['bug', 'wrench', 'path', 'flask', 'doc', 'gear'];
/** 允许的停靠点重要级别枚举值。 */
const STOP_IMPORTANCE = ['critical', 'normal', 'context'];

/** 交给 provider structured output 的 JSON Schema：标题 + 焦点 + 章节树（图标/简介/停靠点），禁止多余字段。 */
export const responseSchema = {
  type: 'object',
  properties: {
    title: { type: 'string' },
    focus: { type: 'string' },
    chapters: {
      type: 'array',
      items: {
        type: 'object',
        properties: {
          title: { type: 'string' },
          icon: { type: 'string', enum: CHAPTER_ICONS },
          blurb: { type: 'string' },
          stops: {
            type: 'array',
            items: {
              type: 'object',
              properties: {
                title: { type: 'string' },
                hunks: { type: 'array', items: { type: 'string' } },
                importance: { type: 'string', enum: STOP_IMPORTANCE },
                prose: { type: 'string' },
              },
              required: ['title', 'hunks', 'importance', 'prose'],
              additionalProperties: false,
            },
          },
        },
        required: ['title', 'icon', 'blurb', 'stops'],
        additionalProperties: false,
      },
    },
  },
  required: ['title', 'focus', 'chapters'],
  additionalProperties: false,
};

/** 把任意值转为修剪过的字符串，可按 max 截断；非字符串返回空串。 */
const asString = (value, max) => {
  if (typeof value !== 'string') return '';
  const trimmed = value.trim();
  return max && trimmed.length > max ? trimmed.slice(0, max) : trimmed;
};

/**
 * Turn a raw model response into a walkthrough anchored to real hunk ids.
 *
 * @param {object} raw parsed model JSON
 * @param {Map<string,string>} idByAlias alias → real hunk id, from the digest
 * @returns {{title: string, focus: string, chapters: Array<object>, droppedAnchors: number}}
 */
/**
 * 把原始模型输出归一化为锚定真实 hunk id 的 walkthrough：hunk 别名经
 * idByAlias 重解析，解析不到或与已有停靠点重复的锚点直接丢弃并计数
 * （droppedAnchors）；没有锚点或没有正文的停靠点、没有停靠点的章节被剔除，
 * 数量超限被截断；完全无可用停靠点时抛 invalid-walkthrough。
 */
export function normalizeWalkthrough(raw, idByAlias) {
  if (!raw || typeof raw !== 'object') {
    throw Object.assign(new Error('Model returned no walkthrough object'), { code: 'invalid-walkthrough' });
  }

  const usedIds = new Set();
  let droppedAnchors = 0;
  let stopCount = 0;

  const chapters = [];
  for (const [chapterIndex, rawChapter] of (Array.isArray(raw.chapters) ? raw.chapters : []).entries()) {
    if (chapters.length >= MAX_CHAPTERS) break;
    if (!rawChapter || typeof rawChapter !== 'object') continue;

    const stops = [];
    for (const rawStop of Array.isArray(rawChapter.stops) ? rawChapter.stops : []) {
      if (stopCount >= MAX_STOPS) break;
      if (!rawStop || typeof rawStop !== 'object') continue;

      const hunkIds = [];
      for (const alias of Array.isArray(rawStop.hunks) ? rawStop.hunks : []) {
        const id = idByAlias.get(typeof alias === 'string' ? alias.trim() : '');
        if (!id) {
          droppedAnchors += 1;
          continue;
        }
        // One hunk belongs to exactly one stop; a model that anchors the same
        // code twice would otherwise render it twice in the stream.
        if (usedIds.has(id)) continue;
        if (hunkIds.length >= MAX_HUNKS_PER_STOP) break;
        usedIds.add(id);
        hunkIds.push(id);
      }

      const prose = asString(rawStop.prose);
      if (hunkIds.length === 0 || !prose) continue;

      stopCount += 1;
      stops.push({
        id: `stop-${chapterIndex + 1}-${stops.length + 1}`,
        title: asString(rawStop.title) || `Step ${stopCount}`,
        hunkIds,
        importance: STOP_IMPORTANCE.includes(rawStop.importance) ? rawStop.importance : 'normal',
        prose,
      });
    }

    if (stops.length === 0) continue;

    chapters.push({
      id: `chapter-${chapters.length + 1}`,
      title: asString(rawChapter.title, MAX_CHAPTER_TITLE_CHARS) || `Part ${chapters.length + 1}`,
      icon: CHAPTER_ICONS.includes(rawChapter.icon) ? rawChapter.icon : 'doc',
      blurb: asString(rawChapter.blurb),
      stops,
    });
  }

  if (chapters.length === 0) {
    throw Object.assign(
      new Error('Model returned no usable stops for this diff'),
      { code: 'invalid-walkthrough' },
    );
  }

  return {
    title: asString(raw.title) || 'Change walkthrough',
    focus: asString(raw.focus),
    chapters,
    droppedAnchors,
  };
}

/**
 * Extract a JSON object from a model response that may or may not honour the
 * schema — some providers wrap it in prose or a fenced block.
 */
/**
 * 从可能不遵守 schema 的模型回复中提取 JSON 对象：先剥掉 markdown 代码围栏
 * 直接 parse，失败后从首个 "{" 起向左收缩、尝试可 parse 的最外层对象；
 * 始终失败则抛 invalid-walkthrough。
 */
export function parseModelJson(text) {
  if (typeof text !== 'string' || !text.trim()) {
    throw Object.assign(new Error('Model returned an empty response'), { code: 'invalid-walkthrough' });
  }

  const withoutFence = text.trim().replace(/^```(?:json)?\s*/i, '').replace(/\s*```$/, '');

  try {
    return JSON.parse(withoutFence);
  } catch {
    // Fall through to a bounded scan for the outermost object.
  }

  const start = withoutFence.indexOf('{');
  if (start === -1) {
    throw Object.assign(new Error('Model response contained no JSON object'), { code: 'invalid-walkthrough' });
  }

  for (let end = withoutFence.lastIndexOf('}'); end > start; end = withoutFence.lastIndexOf('}', end - 1)) {
    try {
      return JSON.parse(withoutFence.slice(start, end + 1));
    } catch {
      // Keep shrinking from the right.
    }
  }

  throw Object.assign(new Error('Model response was not valid JSON'), { code: 'invalid-walkthrough' });
}
