// Language the walkthrough prose is written in.
//
// This is the server's own list rather than an import from the UI package: the
// server cannot reach `packages/ui`, and the two lists answer different
// questions anyway. The UI list is "which locales do we have a dictionary
// for"; this one is "which languages may we ask a model to write in", and it
// needs the English endonym-free name that goes into the prompt.
//
// The tags match the UI's `Locale` union so the picker can pass its own value
// straight through. A tag we do not know resolves to English, which is exactly
// what the feature did before the setting existed.

/**
 * walkthrough 散文语言模块：维护"可要求模型写作的语言"清单，把调用方
 * 传入的任意标签归一化到受支持标签，并提供写入 prompt 的英文语言名。
 * 清单必须与 packages/ui 的 Locale 集合保持一致（由 languages.test.js 把关）。
 */
/**
 * 默认语言（英文）：未知、缺失或畸形的标签一律回落到这里，而不是让请求失败。
 */
export const DEFAULT_LANGUAGE = 'en';

// Value is what the prompt says to write in. Naming the language in English
// keeps the instruction in the same language as the rest of the system prompt,
// which every model handles more reliably than a switch mid-sentence.
/**
 * 受支持语言表：标签 -> 写入 prompt 的英文语言名。用英文命名语言，使指令
 * 与 system prompt 其余部分保持同一种语言（原因见上方英文注释）；键集合
 * 必须与 UI 的 LOCALES 逐一对齐，否则语言选择会静默失效。
 */
const LANGUAGE_NAMES = {
  en: 'English',
  de: 'German',
  fr: 'French',
  'zh-CN': 'Simplified Chinese',
  'zh-TW': 'Traditional Chinese',
  uk: 'Ukrainian',
  es: 'Spanish',
  'pt-BR': 'Brazilian Portuguese',
  ko: 'Korean',
  pl: 'Polish',
  ja: 'Japanese',
  tr: 'Turkish',
};

/**
 * Coerce a caller-supplied tag to one we support.
 *
 * Unknown, absent, and malformed all collapse to English rather than failing
 * the request: the language is a preference about prose, and refusing to
 * generate a walkthrough over one is a worse answer than writing it in English.
 *
 * 中文补充：三级匹配 —— 原值直接命中；否则按小写 + 连字符归一后做前缀
 * 匹配（容忍 uk-UA、pt_br 这类漂移）；再退化到基础子标签精确匹配。全部
 * 落空则返回 DEFAULT_LANGUAGE。
 */
export function normalizeLanguage(value) {
  if (typeof value !== 'string' || !value) return DEFAULT_LANGUAGE;
  if (Object.hasOwn(LANGUAGE_NAMES, value)) return value;

  // Tolerate case and separator drift (`uk-UA`, `pt_br`) so a runtime that
  // passes a platform locale does not silently fall back to English.
  const normalized = value.toLowerCase().replace(/_/g, '-');
  const match = Object.keys(LANGUAGE_NAMES).find((tag) => {
    const lower = tag.toLowerCase();
    return lower === normalized || normalized.startsWith(`${lower}-`);
  });
  if (match) return match;

  // 最后一级：只取基础语言子标签（'zh-CN' 中的 'zh'）再精确匹配一次。
  const base = normalized.split('-')[0];
  const baseMatch = Object.keys(LANGUAGE_NAMES).find((tag) => tag.toLowerCase() === base);
  return baseMatch ?? DEFAULT_LANGUAGE;
}

/** English name of a normalized tag, for the prompt. */
/** 取归一化标签对应的英文语言名，写入 prompt；未知标签回落 English。 */
export function languageName(language) {
  return LANGUAGE_NAMES[language] ?? LANGUAGE_NAMES[DEFAULT_LANGUAGE];
}

// The tags this list must agree with live in `packages/ui/src/lib/i18n`, which
// the server cannot import. `languages.test.js` compares the two by reading
// that file, because a locale added on one side only fails silently: the picker
// offers the language and the walkthrough comes back in English.
/** 仅供测试的内部导出：languages.test.js 借它与 UI 的 LOCALES 做一致性比对。 */
export const __testing = { LANGUAGE_NAMES };
