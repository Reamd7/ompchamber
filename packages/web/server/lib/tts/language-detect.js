/**
 * （中文模块说明）为 TTS 选 voice 服务的轻量语言检测：文字系统先行
 * （韩/日/中/西里尔/希腊/阿拉伯等一锤定音），拉丁系再用特征字母与
 * 停用词计分辨；零依赖，识别失败回退英语而非报错。
 */
/**
 * Language detection for text-to-speech voice selection.
 *
 * Picks the language a piece of chat text is written in so a TTS provider
 * can choose a matching voice or model. Deliberately small and dependency
 * free: the writing system decides most cases outright, and Latin-script
 * languages are told apart by function words and characteristic letters.
 * The answer is a best effort for voice selection, not a linguistic claim —
 * an unknown language falls back to English rather than failing.
 */

/** 文字系统 -> 匹配该系统字符的正则；顺序即检测顺序。 */
const SCRIPT_RANGES = [
  ['hangul', /[가-힯ᄀ-ᇿ㄰-㆏]/g],
  ['kana', /[぀-ヿ]/g],
  ['han', /[一-鿿㐀-䶿]/g],
  ['cyrillic', /[Ѐ-ӿ]/g],
  ['greek', /[Ͱ-Ͽ]/g],
  ['arabic', /[؀-ۿ]/g],
  ['hebrew', /[֐-׿]/g],
  ['thai', /[฀-๿]/g],
  ['devanagari', /[ऀ-ॿ]/g],
  ['latin', /[A-Za-zÀ-ɏ]/g],
];

/** 一文字一语言的系统：直接映射到语言子 tag。 */
const SCRIPT_LANGUAGE = {
  hangul: 'ko',
  greek: 'el',
  arabic: 'ar',
  hebrew: 'he',
  thai: 'th',
  devanagari: 'hi',
};

// （中文说明）拉丁系各语言的独有（或近乎独有）特征字母表。
// Letters that only (or overwhelmingly) occur in one language of a script.
const LATIN_MARKERS = {
  pl: /[łęąńśźż]/i,
  cs: /[řěůťďň]/i,
  tr: /[ğışİ]/,
  pt: /[ãõ]/i,
  es: /[ñ¿¡]/,
  de: /[ß]/,
  fr: /[œ]/i,
  sv: /[å]/i,
};

// （中文说明）各语言等长的高频功能词表，按整词命中计分以保证可比性。
// Frequent function words per language. Scored by whole-word hits; every
// list has the same length so scores stay comparable.
const STOPWORDS = {
  en: ['the', 'and', 'is', 'to', 'of', 'that', 'you', 'with', 'for', 'this', 'are', 'it', 'not', 'have', 'can', 'will', 'your', 'from', 'which', 'when'],
  de: ['und', 'der', 'die', 'das', 'ist', 'nicht', 'mit', 'ein', 'eine', 'auch', 'sich', 'auf', 'für', 'wird', 'werden', 'oder', 'aber', 'wenn', 'sind', 'kann'],
  fr: ['le', 'la', 'les', 'et', 'est', 'une', 'des', 'pour', 'que', 'qui', 'dans', 'pas', 'vous', 'sur', 'avec', 'sont', 'nous', 'cette', 'mais', 'plus'],
  es: ['el', 'la', 'los', 'las', 'que', 'es', 'una', 'por', 'para', 'con', 'del', 'como', 'pero', 'más', 'este', 'esta', 'son', 'tiene', 'puede', 'también'],
  it: ['il', 'la', 'che', 'di', 'è', 'una', 'per', 'non', 'con', 'del', 'della', 'come', 'sono', 'anche', 'questo', 'questa', 'gli', 'nel', 'più', 'essere'],
  pt: ['o', 'a', 'os', 'as', 'que', 'é', 'uma', 'para', 'com', 'não', 'do', 'da', 'como', 'mas', 'também', 'este', 'esta', 'são', 'você', 'pode'],
  pl: ['i', 'nie', 'jest', 'się', 'na', 'to', 'że', 'jak', 'ale', 'dla', 'oraz', 'przez', 'czy', 'tym', 'jego', 'można', 'jeśli', 'tego', 'które', 'także'],
  nl: ['de', 'het', 'een', 'en', 'van', 'is', 'niet', 'dat', 'met', 'voor', 'ook', 'zijn', 'maar', 'als', 'wordt', 'deze', 'kan', 'naar', 'bij', 'dan'],
  cs: ['a', 'je', 'se', 'na', 'to', 'že', 'jak', 'ale', 'pro', 'nebo', 'jsou', 'může', 'také', 'tento', 'když', 'jeho', 'které', 'být', 'aby', 'ještě'],
  tr: ['ve', 'bir', 'bu', 'için', 'ile', 'de', 'da', 'ama', 'gibi', 'daha', 'var', 'olarak', 'çok', 'ne', 'her', 'kadar', 'sonra', 'değil', 'olan', 'ise'],
  sv: ['och', 'att', 'det', 'är', 'en', 'som', 'för', 'inte', 'med', 'till', 'den', 'kan', 'har', 'ett', 'men', 'också', 'eller', 'från', 'när', 'vara'],
  uk: ['і', 'та', 'що', 'це', 'не', 'як', 'для', 'він', 'вона', 'але', 'або', 'також', 'тільки', 'вже', 'якщо', 'його', 'цей', 'ця', 'бути', 'коли'],
  ru: ['и', 'что', 'это', 'не', 'как', 'для', 'он', 'она', 'но', 'или', 'также', 'только', 'уже', 'если', 'его', 'этот', 'эта', 'быть', 'когда', 'чтобы'],
};

/** 参与停用词计分的拉丁系语言。 */
const LATIN_LANGUAGES = ['en', 'de', 'fr', 'es', 'it', 'pt', 'pl', 'nl', 'cs', 'tr', 'sv'];
/** 参与计分的西里尔系语言（乌克兰语/俄语）。 */
const CYRILLIC_LANGUAGES = ['uk', 'ru'];

/** 统计 text 中命中正则的次数。 */
const countMatches = (text, pattern) => {
  const matches = text.match(pattern);
  return matches ? matches.length : 0;
};

/** 对分词结果计算各语言的停用词命中数，返回 language -> score。 */
const scoreStopwords = (words, languages) => {
  const scores = {};
  for (const language of languages) {
    const list = new Set(STOPWORDS[language]);
    let hits = 0;
    for (const word of words) {
      if (list.has(word)) hits += 1;
    }
    scores[language] = hits;
  }
  return scores;
};

/** 取计分最高者；全零时返回 fallback。 */
const bestOf = (scores, fallback) => {
  let best = fallback;
  let bestScore = 0;
  for (const [language, score] of Object.entries(scores)) {
    if (score > bestScore) {
      best = language;
      bestScore = score;
    }
  }
  return best;
};

/** 用特征字母表试探文本，返回第一个命中的语言；未命中返回 null。 */
const pickByMarkers = (text, markers) => {
  for (const [language, pattern] of Object.entries(markers)) {
    if (pattern.test(text)) return language;
  }
  return null;
};

/**
 * （中文说明）检测主流程：无字母回退英语；kana/han 占比定日/中；
 * 一文字一语言系统直接映射；西里尔靠乌克兰/俄语特征字母分胜负
 * （平手倾向乌克兰）；拉丁系特征字母优先于停用词计分。
 */
/**
 * @param {string} text
 * @returns {{ language: string, script: string }} BCP-47 primary language subtag and the dominant script.
 */
export function detectTextLanguage(text) {
  const source = typeof text === 'string' ? text : '';
  const counts = SCRIPT_RANGES.map(([script, pattern]) => [script, countMatches(source, pattern)]);
  const letters = counts.reduce((sum, [, count]) => sum + count, 0);
  if (letters === 0) return { language: 'en', script: 'latin' };

  // Kana settles Japanese even when Han dominates the character count.
  const kana = counts.find(([script]) => script === 'kana')?.[1] ?? 0;
  const han = counts.find(([script]) => script === 'han')?.[1] ?? 0;
  if (kana > 0 && kana + han >= letters * 0.3) return { language: 'ja', script: 'kana' };
  if (han > 0 && han >= letters * 0.3) return { language: 'zh', script: 'han' };

  const [script] = counts.reduce((best, entry) => (entry[1] > best[1] ? entry : best));

  if (script in SCRIPT_LANGUAGE) return { language: SCRIPT_LANGUAGE[script], script };

  const words = source.toLowerCase().split(/[^\p{L}\p{M}']+/u).filter(Boolean);

  if (script === 'cyrillic') {
    const scores = scoreStopwords(words, CYRILLIC_LANGUAGES);
    const ukMarkers = countMatches(source, /[іїєґ]/gi);
    const ruMarkers = countMatches(source, /[ыэъё]/gi);
    // Letters decide: the two alphabets differ in letters that occur in
    // nearly every sentence. Function words only settle a text that shows
    // neither set, and a text with no Russian-only letters is far more
    // likely Ukrainian than the reverse, so that tie goes to Ukrainian.
    if (ukMarkers !== ruMarkers) return { language: ukMarkers > ruMarkers ? 'uk' : 'ru', script };
    if (scores.uk !== scores.ru) return { language: scores.uk > scores.ru ? 'uk' : 'ru', script };
    return { language: ruMarkers > 0 ? 'ru' : 'uk', script };
  }

  const scores = scoreStopwords(words, LATIN_LANGUAGES);
  const marked = pickByMarkers(source, LATIN_MARKERS);
  // A characteristic letter outranks stopword counts unless another language
  // clearly dominates the function words (a German text quoting "façade").
  if (marked && scores[marked] * 2 >= scores[bestOf(scores, marked)]) {
    return { language: marked, script };
  }
  return { language: bestOf(scores, 'en'), script };
}

/**
 * （中文说明）语言 -> voice 列表使用的 locale 前缀优先序；
 * 未知语言直接返回 [language] 本身。
 */
/**
 * Map a detected language onto the locales a voice list uses (`uk_UA`,
 * `en_US`...). Returns the preferred locale prefixes in order.
 * @param {string} language
 * @returns {string[]}
 */
function localePrefixesForLanguage(language) {
  const table = {
    en: ['en_US', 'en_GB', 'en'],
    uk: ['uk_UA', 'uk'],
    ru: ['ru_RU', 'ru'],
    de: ['de_DE', 'de'],
    fr: ['fr_FR', 'fr_CA', 'fr'],
    es: ['es_ES', 'es_MX', 'es'],
    it: ['it_IT', 'it'],
    pt: ['pt_BR', 'pt_PT', 'pt'],
    pl: ['pl_PL', 'pl'],
    nl: ['nl_NL', 'nl_BE', 'nl'],
    cs: ['cs_CZ', 'cs'],
    tr: ['tr_TR', 'tr'],
    sv: ['sv_SE', 'sv'],
    zh: ['zh_CN', 'zh_TW', 'zh_HK', 'zh'],
    ja: ['ja_JP', 'ja'],
    ko: ['ko_KR', 'ko'],
    el: ['el_GR', 'el'],
    ar: ['ar_001', 'ar_SA', 'ar'],
    he: ['he_IL', 'he'],
    th: ['th_TH', 'th'],
    hi: ['hi_IN', 'hi'],
  };
  return table[language] ?? [language];
}

/**
 * （中文说明）按前缀序在 voice 列表里挑：优先 Enhanced/Premium 变体，
 * 其次精确 locale，再次同语言任意 voice；无匹配返回 null。
 */
/**
 * Choose a voice for a language from a `say`-style voice list.
 * Prefers an enhanced/premium variant of a matching voice, then any voice of
 * the exact locale, then any voice of the language. Returns null when the
 * list has no voice for that language.
 * @param {string} language
 * @param {ReadonlyArray<{ name: string, locale: string }>} voices
 * @returns {string | null}
 */
export function pickVoiceForLanguage(language, voices) {
  const prefixes = localePrefixesForLanguage(language);
  for (const prefix of prefixes) {
    const matching = voices.filter((voice) => voice.locale === prefix || voice.locale.startsWith(`${prefix}_`) || (prefix === language && voice.locale.startsWith(`${language}_`)));
    if (matching.length === 0) continue;
    const enhanced = matching.find((voice) => /\((Enhanced|Premium)\)/i.test(voice.name));
    return (enhanced ?? matching[0]).name;
  }
  return null;
}

/**
 * （中文说明）从 voice 的 locale 提取语言子 tag（uk_UA/en-GB -> uk/en）；
 * 非法输入返回 null。
 */
/**
 * Language of a voice, from its locale (`uk_UA` → `uk`).
 * @param {string | null | undefined} locale
 * @returns {string | null}
 */
export function languageOfLocale(locale) {
  if (typeof locale !== 'string' || !locale) return null;
  return locale.split(/[_-]/)[0].toLowerCase();
}
