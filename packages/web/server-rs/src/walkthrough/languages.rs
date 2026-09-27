//! 服务端自持的导览输出语言表：tag 与 UI 的 `Locale` 联合类型一致，
//! 未知 tag 一律回落 English（与该设置存在之前的行为完全相同）。
//! Port of `server/lib/walkthrough/languages.js`.
//!
//! This is the server's own list rather than an import from the UI package:
//! the server cannot reach `packages/ui`, and the two lists answer different
//! questions anyway. The tags match the UI's `Locale` union so the picker can
//! pass its own value straight through. A tag we do not know resolves to
//! English, which is exactly what the feature did before the setting existed.

/// 默认输出语言（English）。
/// `DEFAULT_LANGUAGE`.
pub const DEFAULT_LANGUAGE: &str = "en";

/// tag → 英文语言名对照表（提示词要求模型用该英文名书写正文），
/// 插入顺序与 JS 对象字面量一致。
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

/// 把调用方传入的 tag 规整为受支持值：缺失、空串、未知都回落 English，
/// 并容忍大小写与分隔符漂移（如 `uk-UA`、`pt_br`）。
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

/// 返回规范化 tag 对应的英文语言名（供提示词引用），未知返回 English。
/// `languageName`: English name of a normalized tag, for the prompt.
pub fn language_name(language: &str) -> &'static str {
    LANGUAGE_NAMES
        .iter()
        .find(|(tag, _)| *tag == language)
        .map(|(_, name)| *name)
        .unwrap_or("English")
}

/// 语言规整与命名的测试。
#[cfg(test)]
mod tests {
    use super::{DEFAULT_LANGUAGE, language_name, normalize_language};

/// 界面直接传出的 tag 原样通过。
    #[test]
    fn accepts_the_tags_the_interface_uses() {
        assert_eq!(normalize_language(Some("uk")), "uk");
        assert_eq!(normalize_language(Some("zh-TW")), "zh-TW");
        assert_eq!(normalize_language(Some("pt-BR")), "pt-BR");
    }

/// 平台 locale 的大小写与分隔符漂移被规整到受支持 tag。
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
/// 未知或缺失的 tag 回落 English 而不是让请求失败。
    #[test]
    fn falls_back_to_english_rather_than_failing() {
        assert_eq!(normalize_language(Some("kl")), "en");
        assert_eq!(normalize_language(Some("")), "en");
        assert_eq!(normalize_language(None), "en");
    }

/// 语言名以英文给出，与系统提示词的语言保持一致。
    #[test]
    fn names_languages_in_english_matching_the_language_of_the_prompt() {
        assert_eq!(language_name("uk"), "Ukrainian");
        assert_eq!(language_name("nope"), "English");
        assert_eq!(language_name(DEFAULT_LANGUAGE), "English");
    }
}
