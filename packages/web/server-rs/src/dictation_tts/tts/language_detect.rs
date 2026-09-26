//! Port of `server/lib/tts/language-detect.js` — dependency-free language
//! detection for TTS voice selection. The writing script decides most cases
//! outright; Latin-script languages are told apart by characteristic letters
//! and function words. The answer is a best effort for voice selection, not a
//! linguistic claim — an unknown language falls back to English.
//!
//! `detect_text_language` also drives the dictation module's local TTS model
//! choice (`language: 'auto'`), so the tables below are behavior, not detail.

use std::collections::HashMap;

/// (`script`, char-predicate) pairs in the JS `SCRIPT_RANGES` order — order
/// matters: the dominant-script tie-break keeps the first maximum.
type CharClass = fn(char) -> bool;

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

const LATIN_LANGUAGES: [&str; 11] = [
    "en", "de", "fr", "es", "it", "pt", "pl", "nl", "cs", "tr", "sv",
];
const CYRILLIC_LANGUAGES: [&str; 2] = ["uk", "ru"];

/// Whole-word function-word lists per language (all 20 entries — the JS keeps
/// the lists the same length so scores stay comparable).
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

const UK_MARKER_CHARS: [char; 4] = ['і', 'ї', 'є', 'ґ'];
const RU_MARKER_CHARS: [char; 4] = ['ы', 'э', 'ъ', 'ё'];

/// JS `/[іїєґ]/gi` — matches either case of the Ukrainian-only letters.
fn is_uk_marker(c: char) -> bool {
    let lowered = c.to_lowercase().next().unwrap_or(c);
    UK_MARKER_CHARS.contains(&lowered) || UK_MARKER_CHARS.contains(&c)
}

/// JS `/[ыэъё]/gi`.
fn is_ru_marker(c: char) -> bool {
    let lowered = c.to_lowercase().next().unwrap_or(c);
    RU_MARKER_CHARS.contains(&lowered) || RU_MARKER_CHARS.contains(&c)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Detection {
    /// BCP-47 primary language subtag.
    pub language: String,
    /// Dominant script name (`latin`, `kana`, `han`, ...).
    pub script: String,
}

fn count_matches(source: &str, class: CharClass) -> u64 {
    source.chars().filter(|c| class(*c)).count() as u64
}

/// `source.toLowerCase().split(/[^\p{L}\p{M}']+/u).filter(Boolean)`.
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
#[derive(Debug, Clone, PartialEq)]
pub struct VoiceEntry {
    pub name: String,
    pub locale: String,
}

/// `/\((Enhanced|Premium)\)/i`.
fn is_enhanced_variant(name: &str) -> bool {
    let lowered = name.to_lowercase();
    lowered.contains("(enhanced)") || lowered.contains("(premium)")
}

/// `pickVoiceForLanguage`: prefer an enhanced/premium variant of a matching
/// voice, then any voice of the exact locale, then any voice of the language.
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

#[cfg(test)]
mod tests {
    use super::*;

    fn detect(text: &str) -> String {
        detect_text_language(text).language
    }

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

    #[test]
    fn tells_short_uk_ru_phrases_apart_by_letters() {
        assert_eq!(detect("Готово. Запушено."), "uk");
        assert_eq!(detect("Все ок"), "uk");
        assert_eq!(detect("Добре, давай так зробимо"), "uk");
        assert_eq!(detect("Хорошо, давай так и сделаем"), "ru");
        assert_eq!(detect("Готово, всё запушено."), "ru");
    }

    #[test]
    fn falls_back_to_english_without_letters() {
        assert_eq!(detect("1234 ... !!!"), "en");
        assert_eq!(detect(""), "en");
    }

    #[test]
    fn quoted_foreign_word_does_not_flip_english() {
        let text = "The façade of the building is the part that you see from the street, and it is not the same as the interior.";
        assert_eq!(detect(text), "en");
    }

    #[test]
    fn reports_the_dominant_script() {
        assert_eq!(detect_text_language("変更が完了し").script, "kana");
        assert_eq!(detect_text_language("修改已经完成").script, "han");
        assert_eq!(detect_text_language("Привіт").script, "cyrillic");
    }

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

    #[test]
    fn prefers_the_enhanced_variant_of_a_matching_voice() {
        assert_eq!(
            pick_voice_for_language("uk", &voices()).as_deref(),
            Some("Lesya (Enhanced)")
        );
    }

    #[test]
    fn prefers_the_primary_locale_of_a_language() {
        assert_eq!(
            pick_voice_for_language("en", &voices()).as_deref(),
            Some("Samantha")
        );
    }

    #[test]
    fn returns_none_when_no_voice_speaks_the_language() {
        assert_eq!(pick_voice_for_language("ja", &voices()), None);
    }

    #[test]
    fn unknown_language_falls_back_to_its_own_locale_prefix() {
        let list = vec![VoiceEntry {
            name: "V".into(),
            locale: "fi_FI".into(),
        }];
        assert_eq!(pick_voice_for_language("fi", &list).as_deref(), Some("V"));
    }

    #[test]
    fn reads_the_language_subtag() {
        assert_eq!(language_of_locale(Some("uk_UA")).as_deref(), Some("uk"));
        assert_eq!(language_of_locale(Some("en-GB")).as_deref(), Some("en"));
        assert_eq!(language_of_locale(None), None);
        assert_eq!(language_of_locale(Some("")), None);
    }
}
