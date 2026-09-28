//! Port of `server/lib/tts/language-detect.js` — dependency-free language
//! detection for TTS voice selection. The writing script decides most cases
//! outright; Latin-script languages are told apart by characteristic letters
//! and function words. The answer is a best effort for voice selection, not a
//! linguistic claim — an unknown language falls back to English.
//!
//! `detect_text_language` also drives the dictation module's local TTS model
//! choice (`language: 'auto'`), so the tables below are behavior, not detail.
//!
//! 中文说明：移植自 `server/lib/tts/language-detect.js` 的无依赖语言检测，
//! 用于 TTS voice 选择：文字系统（script）能直接定案大多数语言，拉丁文字
//! 语言靠特征字母与功能词区分。结果只为 voice 选择服务，不是语言学论断，
//! 未知语言回退英语。`detect_text_language` 还驱动听写模块本地 TTS 模型的
//! `language: 'auto'` 选择，因此下面的表格是行为契约而非实现细节。

use std::collections::HashMap;

/// (`script`, char-predicate) pairs in the JS `SCRIPT_RANGES` order — order
/// matters: the dominant-script tie-break keeps the first maximum.
/// 元素顺序必须与 JS 一致：主导 script 并列时保留先出现的最大值。
type CharClass = fn(char) -> bool;

/// JS `SCRIPT_RANGES` 的移植：十个文字系统及各自的 Unicode 码位区间
///（谚文、假名、汉字、西里尔、希腊、阿拉伯、希伯来、泰文、天城文、拉丁）。
const SCRIPT_RANGES: [(&str, CharClass); 10] = [
    ("hangul", |c| {
        ('\u{AC00}'..='\u{D6AF}').contains(&c)
            || ('\u{1100}'..='\u{11FF}').contains(&c)
            || ('\u{3130}'..='\u{318F}').contains(&c)
    }),
    ("kana", |c| ('\u{3040}'..='\u{30FF}').contains(&c)),
    ("han", |c| {
        ('\u{4E00}'..='\u{9FFF}').contains(&c) || ('\u{3400}'..='\u{4DBF}').contains(&c)
    }),
    ("cyrillic", |c| ('\u{0400}'..='\u{04FF}').contains(&c)),
    ("greek", |c| ('\u{0370}'..='\u{03FF}').contains(&c)),
    ("arabic", |c| ('\u{0600}'..='\u{06FF}').contains(&c)),
    ("hebrew", |c| ('\u{0590}'..='\u{05FF}').contains(&c)),
    ("thai", |c| ('\u{0E00}'..='\u{0E7F}').contains(&c)),
    ("devanagari", |c| ('\u{0900}'..='\u{097F}').contains(&c)),
    ("latin", |c| {
        c.is_ascii_alphabetic() || ('\u{00C0}'..='\u{024F}').contains(&c)
    }),
];

/// 能由 script 直接定案的语言映射；拉丁、西里尔、假名、汉字返回 `None`，
/// 需要走后续的细分判定。
fn script_language(script: &str) -> Option<&'static str> {
    match script {
        "hangul" => Some("ko"),
        "greek" => Some("el"),
        "arabic" => Some("ar"),
        "hebrew" => Some("he"),
        "thai" => Some("th"),
        "devanagari" => Some("hi"),
        _ => None,
    }
}

/// 参与停用词评分的拉丁文字语言列表（顺序即 [`best_of`] 的候选顺序）。
const LATIN_LANGUAGES: [&str; 11] = [
    "en", "de", "fr", "es", "it", "pt", "pl", "nl", "cs", "tr", "sv",
];
/// 参与停用词评分的西里尔文字语言（乌克兰语、俄语）。
const CYRILLIC_LANGUAGES: [&str; 2] = ["uk", "ru"];

/// Whole-word function-word lists per language (all 20 entries — the JS keeps
/// the lists the same length so scores stay comparable).
/// 每种语言固定 20 个整词功能词；JS 侧刻意保持等长，使各语言得分可比。
fn stopwords(language: &str) -> &'static [&'static str] {
    match language {
        "en" => &[
            "the", "and", "is", "to", "of", "that", "you", "with", "for", "this", "are", "it",
            "not", "have", "can", "will", "your", "from", "which", "when",
        ],
        "de" => &[
            "und", "der", "die", "das", "ist", "nicht", "mit", "ein", "eine", "auch", "sich",
            "auf", "für", "wird", "werden", "oder", "aber", "wenn", "sind", "kann",
        ],
        "fr" => &[
            "le", "la", "les", "et", "est", "une", "des", "pour", "que", "qui", "dans", "pas",
            "vous", "sur", "avec", "sont", "nous", "cette", "mais", "plus",
        ],
        "es" => &[
            "el", "la", "los", "las", "que", "es", "una", "por", "para", "con", "del", "como",
            "pero", "más", "este", "esta", "son", "tiene", "puede", "también",
        ],
        "it" => &[
            "il", "la", "che", "di", "è", "una", "per", "non", "con", "del", "della", "come",
            "sono", "anche", "questo", "questa", "gli", "nel", "più", "essere",
        ],
        "pt" => &[
            "o", "a", "os", "as", "que", "é", "uma", "para", "com", "não", "do", "da", "como",
            "mas", "também", "este", "esta", "são", "você", "pode",
        ],
        "pl" => &[
            "i", "nie", "jest", "się", "na", "to", "że", "jak", "ale", "dla", "oraz", "przez",
            "czy", "tym", "jego", "można", "jeśli", "tego", "które", "także",
        ],
        "nl" => &[
            "de", "het", "een", "en", "van", "is", "niet", "dat", "met", "voor", "ook", "zijn",
            "maar", "als", "wordt", "deze", "kan", "naar", "bij", "dan",
        ],
        "cs" => &[
            "a", "je", "se", "na", "to", "že", "jak", "ale", "pro", "nebo", "jsou", "může", "také",
            "tento", "když", "jeho", "které", "být", "aby", "ještě",
        ],
        "tr" => &[
            "ve", "bir", "bu", "için", "ile", "de", "da", "ama", "gibi", "daha", "var", "olarak",
            "çok", "ne", "her", "kadar", "sonra", "değil", "olan", "ise",
        ],
        "sv" => &[
            "och", "att", "det", "är", "en", "som", "för", "inte", "med", "till", "den", "kan",
            "har", "ett", "men", "också", "eller", "från", "när", "vara",
        ],
        "uk" => &[
            "і",
            "та",
            "що",
            "це",
            "не",
            "як",
            "для",
            "він",
            "вона",
            "але",
            "або",
            "також",
            "тільки",
            "вже",
            "якщо",
            "його",
            "цей",
            "ця",
            "бути",
            "коли",
        ],
        "ru" => &[
            "и",
            "что",
            "это",
            "не",
            "как",
            "для",
            "он",
            "она",
            "но",
            "или",
            "также",
            "только",
            "уже",
            "если",
            "его",
            "этот",
            "эта",
            "быть",
            "когда",
            "чтобы",
        ],
        _ => &[],
    }
}

/// Characteristic Latin-script letters. `true` = case-insensitive (JS `/i`);
/// `false` matches the exact code points only (e.g. Turkish `İ`, `ß`).
/// 表顺序即判定优先顺序：排在前面的语言先被检查。
const LATIN_MARKERS: [(&str, &[char], bool); 8] = [
    ("pl", &['ł', 'ę', 'ą', 'ń', 'ś', 'ź', 'ż'], true),
    ("cs", &['ř', 'ě', 'ů', 'ť', 'ď', 'ň'], true),
    ("tr", &['ğ', 'ı', 'ş', 'İ'], false),
    ("pt", &['ã', 'õ'], true),
    ("es", &['ñ', '¿', '¡'], false),
    ("de", &['ß'], false),
    ("fr", &['œ'], true),
    ("sv", &['å'], true),
];

/// 乌克兰语独有字母（俄语字母表中不存在），用于区分 uk/ru。
const UK_MARKER_CHARS: [char; 4] = ['і', 'ї', 'є', 'ґ'];
/// 俄语独有字母（乌克兰语字母表中不存在），用于区分 uk/ru。
const RU_MARKER_CHARS: [char; 4] = ['ы', 'э', 'ъ', 'ё'];

/// JS `/[іїєґ]/gi` — matches either case of the Ukrainian-only letters.
/// 大小写两种形式都算命中。
fn is_uk_marker(c: char) -> bool {
    let lowered = c.to_lowercase().next().unwrap_or(c);
    UK_MARKER_CHARS.contains(&lowered) || UK_MARKER_CHARS.contains(&c)
}

/// JS `/[ыэъё]/gi`.
/// 大小写两种形式都算命中。
fn is_ru_marker(c: char) -> bool {
    let lowered = c.to_lowercase().next().unwrap_or(c);
    RU_MARKER_CHARS.contains(&lowered) || RU_MARKER_CHARS.contains(&c)
}

/// `detectTextLanguage` 的返回值：主语言与主导文字系统。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Detection {
    /// BCP-47 primary language subtag.
    /// 如 `uk`、`zh`、`en`。
    pub language: String,
    /// Dominant script name (`latin`, `kana`, `han`, ...).
    /// 如 `latin`、`kana`、`han`、`cyrillic`。
    pub script: String,
}

/// 统计文本中命中给定字符判定的字符个数。
fn count_matches(source: &str, class: CharClass) -> u64 {
    source.chars().filter(|c| class(*c)).count() as u64
}

/// `source.toLowerCase().split(/[^\p{L}\p{M}']+/u).filter(Boolean)`.
/// 组合符号（`\p{M}`）用常见区间近似，撇号也算词内字符；纯标点段被丢弃。
fn lowercase_words(source: &str) -> Vec<String> {
    let lowered = source.to_lowercase();
    let mut words = Vec::new();
    let mut current = String::new();
    for c in lowered.chars() {
        // \p{L} (approximated by `char::is_alphabetic`, which covers the
        // derived Alphabetic property), \p{M} (common combining-mark ranges),
        // or an apostrophe continue a word.
        let is_mark = matches!(
            c,
            '\u{0300}'..='\u{036F}' | '\u{0483}'..='\u{0489}' | '\u{064B}'..='\u{065F}'
        );
        if c.is_alphabetic() || is_mark || c == '\'' {
            current.push(c);
        } else if !current.is_empty() {
            words.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        words.push(current);
    }
    words
}

/// 对每个候选语言统计词表中功能词的整词命中次数，返回 语言 → 得分。
fn score_stopwords<'a>(words: &[String], languages: &[&'a str]) -> HashMap<&'a str, u64> {
    let mut scores = HashMap::new();
    for &language in languages {
        let list = stopwords(language);
        let hits = words
            .iter()
            .filter(|word| list.contains(&word.as_str()))
            .count() as u64;
        scores.insert(language, hits);
    }
    scores
}

/// `bestOf`: highest score wins, ties keep the fallback/first candidate
/// (strictly-greater replacement, iterating in `languages` order).
/// 即遍历时只有严格更高分才替换，0 分不会覆盖 fallback。
fn best_of<'a>(
    scores: &HashMap<&'a str, u64>,
    languages: &[&'a str],
    fallback: &'a str,
) -> &'a str {
    let mut best = fallback;
    let mut best_score = 0u64;
    for &language in languages {
        if let Some(&score) = scores.get(language)
            && score > best_score
        {
            best = language;
            best_score = score;
        }
    }
    best
}

/// 按 [`LATIN_MARKERS`] 的表顺序找第一个命中的特征字母所属语言；无命中返回
/// `None`。
fn pick_by_markers(source: &str) -> Option<&'static str> {
    for &(language, chars, case_insensitive) in LATIN_MARKERS.iter() {
        let hit = source.chars().any(|c| {
            if case_insensitive {
                let lowered = c.to_lowercase().next().unwrap_or(c);
                chars.contains(&lowered) || chars.contains(&c)
            } else {
                chars.contains(&c)
            }
        });
        if hit {
            return Some(language);
        }
    }
    None
}

/// `detectTextLanguage`.
///
/// 判定顺序：无字母 → 英语；假名出现且假名+汉字占比 ≥ 30% 判日语、纯汉字
/// 占比 ≥ 30% 判中文；主导 script 可直接定案的语言（韩/希腊/阿拉伯/希伯来/
/// 泰/印地）直接返回；西里尔走 uk/ru 特征字母与停用词三级判别；拉丁先看
/// 特征字母（停用词得分未超过其两倍时生效），再退回停用词最高分（平局为
/// 英语）。
pub fn detect_text_language(text: &str) -> Detection {
    let counts: Vec<(&str, u64)> = SCRIPT_RANGES
        .iter()
        .map(|&(script, class)| (script, count_matches(text, class)))
        .collect();
    let letters: u64 = counts.iter().map(|(_, count)| *count).sum();
    if letters == 0 {
        return Detection {
            language: "en".to_string(),
            script: "latin".to_string(),
        };
    }

    let kana = counts
        .iter()
        .find(|(script, _)| *script == "kana")
        .map(|(_, count)| *count)
        .unwrap_or(0);
    let han = counts
        .iter()
        .find(|(script, _)| *script == "han")
        .map(|(_, count)| *count)
        .unwrap_or(0);
    // Kana settles Japanese even when Han dominates the character count.
    if kana > 0 && (kana + han) as f64 >= letters as f64 * 0.3 {
        return Detection {
            language: "ja".to_string(),
            script: "kana".to_string(),
        };
    }
    if han > 0 && han as f64 >= letters as f64 * 0.3 {
        return Detection {
            language: "zh".to_string(),
            script: "han".to_string(),
        };
    }

    // Dominant script; first maximum wins (JS reduce keeps the earlier entry).
    let (script, _) = counts
        .iter()
        .fold((counts[0].0, counts[0].1), |best, entry| {
            if entry.1 > best.1 { *entry } else { best }
        });

    if let Some(language) = script_language(script) {
        return Detection {
            language: language.to_string(),
            script: script.to_string(),
        };
    }

    let words = lowercase_words(text);

    if script == "cyrillic" {
        let scores = score_stopwords(&words, &CYRILLIC_LANGUAGES);
        let uk_markers = count_matches(text, is_uk_marker);
        let ru_markers = count_matches(text, is_ru_marker);
        // Letters decide: the two alphabets differ in letters that occur in
        // nearly every sentence; a text with no Russian-only letters is far
        // more likely Ukrainian, so that tie goes to Ukrainian.
        if uk_markers != ru_markers {
            return Detection {
                language: if uk_markers > ru_markers { "uk" } else { "ru" }.to_string(),
                script: script.to_string(),
            };
        }
        if scores["uk"] != scores["ru"] {
            return Detection {
                language: if scores["uk"] > scores["ru"] {
                    "uk"
                } else {
                    "ru"
                }
                .to_string(),
                script: script.to_string(),
            };
        }
        return Detection {
            language: if ru_markers > 0 { "ru" } else { "uk" }.to_string(),
            script: script.to_string(),
        };
    }

    let scores = score_stopwords(&words, &LATIN_LANGUAGES);
    let marked = pick_by_markers(text);
    if let Some(marked) = marked {
        let best = best_of(&scores, &LATIN_LANGUAGES, marked);
        // A characteristic letter outranks stopword counts unless another
        // language clearly dominates the function words.
        if scores[marked] * 2 >= scores[best] {
            return Detection {
                language: marked.to_string(),
                script: script.to_string(),
            };
        }
    }
    Detection {
        language: best_of(&scores, &LATIN_LANGUAGES, "en").to_string(),
        script: script.to_string(),
    }
}

/// 每种语言的 locale 前缀偏好表（按优先级排列，如法语 `fr_FR`、`fr_CA`、
/// `fr`）；表外语言回退为语言码本身。
fn locale_prefixes_for_language(language: &str) -> Vec<String> {
    let table: &[&str] = match language {
        "en" => &["en_US", "en_GB", "en"],
        "uk" => &["uk_UA", "uk"],
        "ru" => &["ru_RU", "ru"],
        "de" => &["de_DE", "de"],
        "fr" => &["fr_FR", "fr_CA", "fr"],
        "es" => &["es_ES", "es_MX", "es"],
        "it" => &["it_IT", "it"],
        "pt" => &["pt_BR", "pt_PT", "pt"],
        "pl" => &["pl_PL", "pl"],
        "nl" => &["nl_NL", "nl_BE", "nl"],
        "cs" => &["cs_CZ", "cs"],
        "tr" => &["tr_TR", "tr"],
        "sv" => &["sv_SE", "sv"],
        "zh" => &["zh_CN", "zh_TW", "zh_HK", "zh"],
        "ja" => &["ja_JP", "ja"],
        "ko" => &["ko_KR", "ko"],
        "el" => &["el_GR", "el"],
        "ar" => &["ar_001", "ar_SA", "ar"],
        "he" => &["he_IL", "he"],
        "th" => &["th_TH", "th"],
        "hi" => &["hi_IN", "hi"],
        _ => return vec![language.to_string()],
    };
    table.iter().map(|prefix| prefix.to_string()).collect()
}

/// A voice entry from a `say -v '?'` listing.
/// 名称与 locale 两个字段即可支撑 voice 挑选。
#[derive(Debug, Clone, PartialEq)]
pub struct VoiceEntry {
    /// voice 显示名（可能带 "(Enhanced)"、"(Premium)" 等变体后缀）。
    pub name: String,
    /// `say` 输出的 locale（如 `uk_UA`）。
    pub locale: String,
}

/// `/\((Enhanced|Premium)\)/i`.
/// 只看名称中的后缀子串，不要求精确匹配。
fn is_enhanced_variant(name: &str) -> bool {
    let lowered = name.to_lowercase();
    lowered.contains("(enhanced)") || lowered.contains("(premium)")
}

/// `pickVoiceForLanguage`: prefer an enhanced/premium variant of a matching
/// voice, then any voice of the exact locale, then any voice of the language.
/// 同一前缀组内优先 Enhanced/Premium 变体，否则取列表中的第一个。
pub fn pick_voice_for_language(language: &str, voices: &[VoiceEntry]) -> Option<String> {
    let prefixes = locale_prefixes_for_language(language);
    for prefix in &prefixes {
        let matching: Vec<&VoiceEntry> = voices
            .iter()
            .filter(|voice| {
                voice.locale == *prefix
                    || voice.locale.starts_with(&format!("{prefix}_"))
                    || (*prefix == language && voice.locale.starts_with(&format!("{language}_")))
            })
            .collect();
        if matching.is_empty() {
            continue;
        }
        let enhanced = matching
            .iter()
            .find(|voice| is_enhanced_variant(&voice.name))
            .copied();
        let pick = enhanced.unwrap_or(matching[0]);
        return Some(pick.name.clone());
    }
    None
}
/// `languageOfLocale` (`uk_UA` → `uk`; `null` stays `None`).
/// 按 `_` 或 `-` 切分取首段并小写化。
pub fn language_of_locale(locale: Option<&str>) -> Option<String> {
    let locale = locale?;
    if locale.is_empty() {
        return None;
    }
    locale
        .split(['_', '-'])
        .next()
        .map(|primary| primary.to_lowercase())
}

/// 语言检测与 voice 挑选的行为契约测试。
#[cfg(test)]
mod tests {
    use super::*;

    /// 取检测结果的 language 字段的便捷封装。
    fn detect(text: &str) -> String {
        detect_text_language(text).language
    }

    /// 验证 16 种表内语言的完整句子都能被正确识别。
    #[test]
    fn detects_table_languages() {
        let cases = [
            (
                "en",
                "The build is green and the tests pass, so you can merge this now.",
            ),
            (
                "uk",
                "Привіт! Це тестове повідомлення, і воно написане українською мовою.",
            ),
            (
                "ru",
                "Привет! Это тестовое сообщение, и оно написано на русском языке.",
            ),
            (
                "de",
                "Die Änderung ist fertig und die Tests laufen ohne Fehler durch.",
            ),
            (
                "fr",
                "La modification est prête et les tests passent sans erreur.",
            ),
            (
                "es",
                "El cambio está listo y las pruebas pasan sin errores.",
            ),
            ("it", "La modifica è pronta e i test passano senza errori."),
            (
                "pt",
                "A alteração está pronta e os testes passam sem erros, você pode continuar.",
            ),
            ("pl", "Zmiana jest gotowa i testy przechodzą bez błędów."),
            (
                "nl",
                "De wijziging is klaar en de tests slagen zonder fouten.",
            ),
            ("cs", "Změna je hotová a testy procházejí bez chyb."),
            ("tr", "Değişiklik hazır ve testler hatasız geçiyor."),
            ("sv", "Ändringen är klar och testerna går igenom utan fel."),
            ("zh", "修改已经完成，所有测试都通过了。"),
            ("ja", "変更が完了し、すべてのテストに合格しました。"),
            ("ko", "변경이 완료되었고 모든 테스트를 통과했습니다."),
        ];
        for (language, text) in cases {
            assert_eq!(detect(text), language, "text: {text}");
        }
    }

    /// 验证短句（停用词证据不足）靠独有字母区分乌克兰语与俄语。
    #[test]
    fn tells_short_uk_ru_phrases_apart_by_letters() {
        assert_eq!(detect("Готово. Запушено."), "uk");
        assert_eq!(detect("Все ок"), "uk");
        assert_eq!(detect("Добре, давай так зробимо"), "uk");
        assert_eq!(detect("Хорошо, давай так и сделаем"), "ru");
        assert_eq!(detect("Готово, всё запушено."), "ru");
    }

    /// 验证无字母输入（纯数字/标点/空串）回退英语。
    #[test]
    fn falls_back_to_english_without_letters() {
        assert_eq!(detect("1234 ... !!!"), "en");
        assert_eq!(detect(""), "en");
    }

    /// 验证英文长句中的外来词（如 façade）不会翻转整体语言判定。
    #[test]
    fn quoted_foreign_word_does_not_flip_english() {
        let text = "The façade of the building is the part that you see from the street, and it is not the same as the interior.";
        assert_eq!(detect(text), "en");
    }

    /// 验证 Detection.script 报告主导文字系统（假名/汉字/西里尔）。
    #[test]
    fn reports_the_dominant_script() {
        assert_eq!(detect_text_language("変更が完了し").script, "kana");
        assert_eq!(detect_text_language("修改已经完成").script, "han");
        assert_eq!(detect_text_language("Привіт").script, "cyrillic");
    }

    /// 构造覆盖多语言的 `say` voice 列表。
    fn voices() -> Vec<VoiceEntry> {
        vec![
            VoiceEntry {
                name: "Samantha".into(),
                locale: "en_US".into(),
            },
            VoiceEntry {
                name: "Daniel".into(),
                locale: "en_GB".into(),
            },
            VoiceEntry {
                name: "Lesya".into(),
                locale: "uk_UA".into(),
            },
            VoiceEntry {
                name: "Lesya (Enhanced)".into(),
                locale: "uk_UA".into(),
            },
            VoiceEntry {
                name: "Milena".into(),
                locale: "ru_RU".into(),
            },
            VoiceEntry {
                name: "Anna".into(),
                locale: "de_DE".into(),
            },
        ]
    }

    /// 验证同语言同 locale 时优先选 "(Enhanced)" 变体。
    #[test]
    fn prefers_the_enhanced_variant_of_a_matching_voice() {
        assert_eq!(
            pick_voice_for_language("uk", &voices()).as_deref(),
            Some("Lesya (Enhanced)")
        );
    }

    /// 验证英语按前缀优先级选主 locale 的 voice（en_US 优先）。
    #[test]
    fn prefers_the_primary_locale_of_a_language() {
        assert_eq!(
            pick_voice_for_language("en", &voices()).as_deref(),
            Some("Samantha")
        );
    }

    /// 验证没有任何 voice 会说该语言时返回 `None`。
    #[test]
    fn returns_none_when_no_voice_speaks_the_language() {
        assert_eq!(pick_voice_for_language("ja", &voices()), None);
    }

    /// 验证表外语言回退为语言码本身作为前缀去匹配。
    #[test]
    fn unknown_language_falls_back_to_its_own_locale_prefix() {
        let list = vec![VoiceEntry {
            name: "V".into(),
            locale: "fi_FI".into(),
        }];
        assert_eq!(pick_voice_for_language("fi", &list).as_deref(), Some("V"));
    }

    /// 验证 locale 的主语言子标签提取与 `None`/空串边界。
    #[test]
    fn reads_the_language_subtag() {
        assert_eq!(language_of_locale(Some("uk_UA")).as_deref(), Some("uk"));
        assert_eq!(language_of_locale(Some("en-GB")).as_deref(), Some("en"));
        assert_eq!(language_of_locale(None), None);
        assert_eq!(language_of_locale(Some("")), None);
    }
}
