import { DEFAULT_LANGUAGE, languageName } from './languages.js';
import { MAX_CHAPTERS, MAX_CHAPTER_TITLE_CHARS, MAX_HUNKS_PER_STOP, MAX_STOPS } from './schema.js';

/**
 * walkthrough 的 prompt 组装模块：把变更来源说明、规模指引、上一版导览
 * 沿用提示与 digest JSON 拼成 user prompt，并按目标语言决定 system prompt；
 * 本模块只做纯文本拼装，不发起任何模型请求。
 */
/**
 * system prompt 常量：给模型定义"导览式评审"的角色与硬性规则 —— 按可理解
 * 顺序重组跨文件 hunk、只用 digest 给出的别名锚定、区分重要性分级、
 * 标题命名被评审的事物本身、散文用纯句子，且只回一个 JSON 对象。
 */
const SYSTEM = `You are writing a guided review of a code change for the engineer who is about to read it.

Your job is to impose a reading order the diff itself does not have. A diff is ordered by file path, which is almost never the order in which the change makes sense. Group related hunks — across files — into stops, and order the stops so that each one is understandable given the ones before it.

What a good stop says:
- what this code now does differently, in terms of behavior, not syntax
- why the surrounding hunks belong together
- what a reviewer should check or be suspicious about, when there is something

What a bad stop says:
- "Renamed X to Y", "Added a parameter", "Updated the imports" — restating the diff in prose is worthless; the reader can already see it
- speculation about intent you cannot support from the code

Rules:
- Anchor every stop to hunk aliases from the digest, exactly as given (h1, h2, …). Never invent an alias.
- Anchor each hunk at most once, in the stop where it matters most.
- You do not have to cover every hunk. Mechanical changes are better left out than padded into a stop; whatever you omit is still shown to the reader separately.
- Order stops so the reader builds understanding: entry points and data shape before the code that consumes them.
- importance: "critical" for changes that carry real risk or drive the rest, "context" for supporting changes, "normal" otherwise.
- Stop titles name the thing the stop is about, not the act of reviewing it. "Hardware keyboard bridge" and "Overflow menu removed" tell a reader scanning the contents what they will find; "Exercise the boundaries" and "Describe the contract" do not.
- Write prose as plain sentences. No markdown, no bullet lists, no code fences.

Respond with a single JSON object and nothing else. (Some providers refuse a structured-output request unless the word "json" appears in the request, which is why this is stated explicitly.)`;

// A reader who cannot follow English prose gets nothing out of a walkthrough,
// so the output language is the reader's, not the codebase's.
//
// Only prose is translated. Hunk aliases are keys the server resolves back to
// hunk ids, and `icon`/`importance` are enums the normalizer validates against
// fixed English values — translating either produces a walkthrough that drops
// its anchors or loses its styling, silently and completely. Identifiers taken
// from the diff stay verbatim for the same reason a translated function name
// would be unsearchable.
/**
 * 生成追加到 system prompt 末尾的目标语言指令片段。
 *
 * 只要求翻译散文（标题、导语、章节与站点的文字）；hunk 别名、icon 与
 * importance 枚举、标识符、路径必须保持英文 —— 原因见上方英文注释：
 * 翻译锚点会让 normalizer 无法回 resolve，导览会静默丢锚或丢样式。
 *
 * @param language 已归一化的语言标签（见 languages.js）
 * @returns 直接拼接在 SYSTEM 之后的指令文本
 */
const languageInstruction = (language) => `

Write all prose in ${languageName(language)}: the walkthrough title, the focus line, chapter titles and blurbs, and stop titles and prose. The reader of this review reads ${languageName(language)}.

Keep these in English exactly as given, regardless of the prose language: hunk aliases (h1, h2, …), the "icon" values, and the "importance" values. Keep identifiers, file paths, and API names as they appear in the code — never translate them.`;

/**
 * 依据变更规模计算"体量指引"段落：目标站点数约为 hunk 数除以 2.5 并夹在
 * 1 到 MAX_STOPS 之间；hunk 数不超过 4 时单章，否则按每 3 站一章夹在
 * 1 到 MAX_CHAPTERS。同时重申每站 hunk 上限与章节标题的窄列字数限制，
 * 引导模型产出"少而密"的站点划分。
 *
 * @param fileCount 可评审文件数
 * @param hunkCount 可评审 hunk 数
 * @returns 直接嵌入 prompt 的多行文本
 */
const sizing = ({ fileCount, hunkCount }) => {
  const targetStops = Math.max(1, Math.min(MAX_STOPS, Math.round(hunkCount / 2.5) || 1));
  const targetChapters = hunkCount <= 4
    ? 1
    : Math.max(1, Math.min(MAX_CHAPTERS, Math.ceil(targetStops / 3)));

  return `This change has ${fileCount} file(s) and ${hunkCount} reviewable hunk(s).

Aim for about ${targetStops} stop(s) across about ${targetChapters} chapter(s); never exceed ${MAX_STOPS} stops, ${MAX_CHAPTERS} chapters, or ${MAX_HUNKS_PER_STOP} hunks in one stop. Fewer, denser stops beat many thin ones.

Chapter titles render in a narrow column: at most ${MAX_CHAPTER_TITLE_CHARS} characters, one or two words.`;
};

/**
 * 把上一版 walkthrough 渲染进 prompt 的"沿用提示"段落；没有可用上一版
 * （缺 chapters）时返回空串，整段省略。提示要求模型对照当前 digest 全部
 * 重新锚定：保留仍准确的站点、改写代码已变的、删掉代码已消失的、补充
 * 新增的工作，而不是出于惯性行保留旧结构。
 *
 * @param previous 上一版 walkthrough 对象（title + chapters），可为空
 * @returns 嵌入 prompt 的文本；无上一版时为空字符串
 */
const previousWalkthroughSection = (previous) => {
  if (!previous || !Array.isArray(previous.chapters) || previous.chapters.length === 0) return '';

  const outline = previous.chapters
    .map((chapter) => {
      const stops = (chapter.stops || [])
        .map((stop) => `  - ${stop.title}: ${stop.prose}`)
        .join('\n');
      return `- ${chapter.title}${chapter.blurb ? ` — ${chapter.blurb}` : ''}\n${stops}`;
    })
    .join('\n');

  return `
A previous walkthrough of an earlier state of this change is below. The code has moved on since it was written, so its anchors are gone — deliberately, so you re-anchor everything against the current digest.

Keep the stops that are still accurate and phrased well, revise the ones whose code changed, drop the ones whose code no longer exists, and add stops for work that is new. Do not preserve its structure out of loyalty; preserve it only where it still fits.

Previous walkthrough — "${previous.title}":
${outline}
`;
};

// Used only when a provider rejects a schema request: the shape has to travel
// in the prompt instead of the request body.
/**
 * 输出 JSON 的形状说明（导出常量）：仅在 provider 拒绝 structured-output
 * 请求时降级使用 —— 把本应放在请求体里的 schema 改以文字写进 prompt。
 */
export const JSON_SHAPE_INSTRUCTION = `
Return ONLY a JSON object, with no prose around it and no markdown fences, in exactly this shape:
{"title": string, "focus": string, "chapters": [{"title": string, "icon": "bug"|"wrench"|"path"|"flask"|"doc"|"gear", "blurb": string, "stops": [{"title": string, "hunks": [string], "importance": "critical"|"normal"|"context", "prose": string}]}]}`;

/**
 * 组装一次 walkthrough 生成的完整 prompt。
 *
 * user prompt 依次为：来源一句话（工作区 / 分支三点对比 / PR）、体量指引、
 * 上一版沿用提示（可空）、序列化成 JSON 的 digest。system prompt 默认就是
 * SYSTEM；目标语言为默认英文时不再附加语言指令（见函数内注释）。
 *
 * @param digest buildDigest 产出的模型可见摘要（文件与 hunk 别名）
 * @param fileCount 可评审文件数，用于体量指引
 * @param hunkCount 可评审 hunk 数，用于体量指引
 * @param source 变更来源（working-tree / branch / pull request）
 * @param previousWalkthrough 上一版导览，供增量修订；可省略
 * @param language 目标语言标签，默认 DEFAULT_LANGUAGE
 * @returns { system, prompt }，分别作为 system 与 user 消息发送
 */
export function buildPrompt({ digest, fileCount, hunkCount, source, previousWalkthrough, language = DEFAULT_LANGUAGE }) {
  const sourceLine = source.kind === 'working-tree'
    ? `Uncommitted local changes (${source.scope === 'all' ? 'staged and unstaged' : source.scope}).`
    : source.kind === 'branch'
      ? `All work on branch "${source.headRef}" that is not in "${source.baseRef}". Changes merged in from ${source.baseRef} are already excluded.`
      : `Pull request #${source.number}.`;

  // user prompt：来源说明 + 体量指引 + 上一版沿用提示（可为空串）+ digest。
  const prompt = `Reviewing: ${sourceLine}

${sizing({ fileCount, hunkCount })}
${previousWalkthroughSection(previousWalkthrough)}
Change digest:
${JSON.stringify(digest)}`;

  // The default language adds nothing: the system prompt is already English, so
  // saying so would only spend context restating it.
  const system = language === DEFAULT_LANGUAGE ? SYSTEM : `${SYSTEM}${languageInstruction(language)}`;

  return { system, prompt };
}
