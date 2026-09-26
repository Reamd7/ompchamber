//! Port of `server/lib/walkthrough/languages.js`.
//!
//! This is the server's own list rather than an import from the UI package:
//! the server cannot reach `packages/ui`, and the two lists answer different
//! questions anyway. The tags match the UI's `Locale` union so the picker can
//! pass its own value straight through. A tag we do not know resolves to
//! English, which is exactly what the feature did before the setting existed.

/// `DEFAULT_LANGUAGE`.
pub const DEFAULT_LANGUAGE: &str = "en";

/// `LANGUAGE_NAMES`: the value is what the prompt says to write in. Naming the
/// language in English keeps the instruction in the same language as the rest
/// of the system prompt, which every model handles more reliably than a switch
/// mid-sentence. Insertion order mirrors the JS object literal.
const LANGUAGE_NAMES: &[(&str, &str)] = &[
    ("en", "English"),
    ("de", "German"),
    ("fr", "French"),
    ("zh-CN", "Simplified Chinese"),
    ("zh-TW", "Traditional Chinese"),
    ("uk", "Ukrainian"),
    ("es", "Spanish"),
    ("pt-BR", "Brazilian Portuguese"),
    ("ko", "Korean"),
    ("pl", "Polish"),
    ("ja", "Japanese"),
    ("tr", "Turkish"),
];

/// `normalizeLanguage`: coerce a caller-supplied tag to one we support.
///
/// Unknown, absent, and malformed all collapse to English rather than failing
/// the request: the language is a preference about prose, and refusing to
/// generate a walkthrough over one is a worse answer than writing it in
/// English.
pub fn normalize_language(value: Option<&str>) -> String {
    let Some(value) = value else {
        return DEFAULT_LANGUAGE.to_string();
    };
    if value.is_empty() {
        return DEFAULT_LANGUAGE.to_string();
    }
    if LANGUAGE_NAMES.iter().any(|(tag, _)| *tag == value) {
        return value.to_string();
    }

    // Tolerate case and separator drift (`uk-UA`, `pt_br`) so a runtime that
    // passes a platform locale does not silently fall back to English.
    let normalized = value.to_lowercase().replace('_', "-");
    if let Some((tag, _)) = LANGUAGE_NAMES.iter().find(|(tag, _)| {
        tag.to_lowercase() == normalized
            || normalized.starts_with(&format!("{}-", tag.to_lowercase()))
    }) {
        return tag.to_string();
    }

    let base = normalized.split('-').next().unwrap_or("");
    if let Some((tag, _)) = LANGUAGE_NAMES
        .iter()
        .find(|(tag, _)| tag.to_lowercase() == base)
    {
        return tag.to_string();
    }
    DEFAULT_LANGUAGE.to_string()
}

/// `languageName`: English name of a normalized tag, for the prompt.
pub fn language_name(language: &str) -> &'static str {
    LANGUAGE_NAMES
        .iter()
        .find(|(tag, _)| *tag == language)
        .map(|(_, name)| *name)
        .unwrap_or("English")
}

#[cfg(test)]
mod tests {
    use super::{DEFAULT_LANGUAGE, language_name, normalize_language};

    #[test]
    fn accepts_the_tags_the_interface_uses() {
        assert_eq!(normalize_language(Some("uk")), "uk");
        assert_eq!(normalize_language(Some("zh-TW")), "zh-TW");
        assert_eq!(normalize_language(Some("pt-BR")), "pt-BR");
    }

    #[test]
    fn tolerates_case_and_separator_drift_from_a_platform_locale() {
        assert_eq!(normalize_language(Some("uk-UA")), "uk");
        assert_eq!(normalize_language(Some("pt_br")), "pt-BR");
        assert_eq!(normalize_language(Some("ja-JP")), "ja");
        assert_eq!(normalize_language(Some("ZH-cn")), "zh-CN");
    }

    // A language preference is about prose. Refusing to write a walkthrough
    // over an unrecognised tag would be a worse answer than writing it in
    // English.
    #[test]
    fn falls_back_to_english_rather_than_failing() {
        assert_eq!(normalize_language(Some("kl")), "en");
        assert_eq!(normalize_language(Some("")), "en");
        assert_eq!(normalize_language(None), "en");
    }

    #[test]
    fn names_languages_in_english_matching_the_language_of_the_prompt() {
        assert_eq!(language_name("uk"), "Ukrainian");
        assert_eq!(language_name("nope"), "English");
        assert_eq!(language_name(DEFAULT_LANGUAGE), "English");
    }
}
