//! Port of `server/lib/text/summarization.js` (modes: tts / notification /
//! note). The JS retired its model-backed path (`zenModel` is ignored); the
//! local sanitize/distill fallbacks are the whole behavior.
//!
//! The regex-driven sanitizers are re-implemented as character scans (no
//! regex crate in the allowed dependency set); each function documents the JS
//! pattern it mirrors. Length arithmetic uses UTF-16 code-unit semantics to
//! match JS `String.length` / `slice`.
//!
//! The small-model seam is injectable ([`ModelSummarySource`]): production
//! passes `None` (exactly the JS stub behavior); a future model-backed
//! summarizer can be wired without touching this module again.
//! 中文说明：本模块移植自 `server/lib/text/summarization.js`（模式：
//! tts / notification / note）。JS 侧已退役模型摘要路径（忽略
//! zenModel），本地 sanitize/distill 兜底即全部行为。正则驱动的清洗器
//! 在此重实现为字符扫描（允许的依赖集里没有 regex crate），每个函数
//! 都注明它对应的 JS 模式；长度运算采用 UTF-16 码元语义以对齐 JS 的
//! String.length/slice。小模型接缝可注入（ModelSummarySource）：生产
//! 传 None（与 JS 桩完全一致），未来接入模型摘要无需再改本模块。

use std::sync::Arc;

use futures::future::BoxFuture;
use serde_json::{Map, Value};

use crate::small_model::http::{utf16_len, utf16_prefix};

/// 摘要模式：决定使用哪套清洗器与蒸馏兜底（对应 JS 的 mode 参数）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SummaryMode {
    /// 语音朗读：最激进的清洗（去 markdown、代码、URL、路径与标点）。
    Tts,
    /// 系统通知：去除 markdown 结构，但保留内联代码与强调的内部文本。
    Notification,
    /// 笔记标题：同通知清洗，另外整体删除 URL 与引号。
    Note,
}

/// SummaryMode 的解析辅助。
impl SummaryMode {
    /// 从字符串解析模式；无法识别时回落 Tts（与 JS 的 default 分支一致）。
    fn from_str(value: &str) -> Self {
        match value {
            "note" => Self::Note,
            "notification" => Self::Notification,
            _ => Self::Tts,
        }
    }
}

// ---------------------------------------------------------------------------
// Character-class helpers (JS regex semantics, ASCII classes)
// ---------------------------------------------------------------------------

/// JS 的 `\w` 字符类：ASCII 字母数字与下划线。
fn is_js_word(character: char) -> bool {
    character.is_ascii_alphanumeric() || character == '_'
}

/// JS 的 `\s` 字符类：ASCII 空白 + Unicode 空白 + BOM（逐字符枚举）。
fn is_js_whitespace(character: char) -> bool {
    matches!(
        character,
        ' ' | '\t' | '\n' | '\r' | '\u{b}' | '\u{c}' | '\u{a0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200a}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202f}'
                | '\u{205f}'
                | '\u{3000}'
                | '\u{feff}'
    )
}

/// `text.split(/\s+/).join(' ').trim()` — collapse every whitespace run.
/// 中文说明：等价 `split(/\s+/).join(' ').trim()`——把所有空白串折叠
/// 为单个空格。
fn collapse_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<&str>>().join(" ")
}

// ---------------------------------------------------------------------------
// Shared markdown scanners
// ---------------------------------------------------------------------------

/// ```[\s\S]*?``` (lazy): fenced code blocks, replaced by `replacement`.
/// 中文说明：惰性匹配三反引号围栏代码块并整体替换为 replacement；
/// 没有闭合围栏时保留原文。
fn replace_fenced_code(text: &str, replacement: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("```") {
        let after = &rest[start + 3..];
        match after.find("```") {
            Some(end) => {
                result.push_str(&rest[..start]);
                result.push_str(replacement);
                rest = &after[end + 3..];
            }
            None => break,
        }
    }
    result.push_str(rest);
    result
}

/// Leftmost-first pairing: JS `aMARK(.*?)MARKb` pairs the first marker with
/// the next occurrence of the same marker. `keep_content` selects the JS
/// replacement: emphasis keeps the inner text (`**x**` → `x`), TTS inline
/// code replaces the whole match with a space (`` `x` `` → ` `).
/// 中文说明：最左优先配对——JS 正则把第一个标记与下一个同标记配对；
/// keep_content 决定保留内部文本（强调标记）还是整体换成空格（TTS
/// 内联代码）。
fn replace_delimited(text: &str, marker: &str, keep_content: bool) -> String {
    let marker_chars: Vec<char> = marker.chars().collect();
    let chars: Vec<char> = text.chars().collect();
    let starts_with = |at: usize, marker: &[char]| {
        chars.len() >= at + marker.len() && chars[at..at + marker.len()] == marker[..]
    };
    let find_from = |from: usize, marker: &[char]| -> Option<usize> {
        (from..=chars.len().saturating_sub(marker.len())).find(|&at| starts_with(at, marker))
    };
    let mut result = String::with_capacity(text.len());
    let mut index = 0;
    while index < chars.len() {
        if starts_with(index, &marker_chars)
            && let Some(close) = find_from(index + marker_chars.len(), &marker_chars)
        {
            if keep_content {
                result.extend(chars[index + marker_chars.len()..close].iter());
            } else {
                result.push(' ');
            }
            index = close + marker_chars.len();
            continue;
        }
        result.push(chars[index]);
        index += 1;
    }
    result
}

/// `\[(.*?)\]\((.*?)\)` → link text.
/// 中文说明：markdown 链接（方括号文字 + 圆括号地址）替换为链接文字；
/// 逐字符非贪婪扫描，未配对的方括号原样保留。
fn replace_markdown_links(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut result = String::with_capacity(text.len());
    let mut index = 0;
    while index < chars.len() {
        if chars[index] == '[' {
            // Non-greedy: the next ']' then '(' then the next ')'.
            let close = (index + 1..chars.len()).find(|&at| chars[at] == ']');
            if let Some(close) = close
                && chars.get(close + 1) == Some(&'(')
                && let Some(paren) = (close + 2..chars.len()).find(|&at| chars[at] == ')')
            {
                result.extend(chars[index + 1..close].iter());
                index = paren + 1;
                continue;
            }
        }
        result.push(chars[index]);
        index += 1;
    }
    result
}

/// Per-line `^[\t ]*[-*+]\s+` / `^#{1,6}\s+` / `^\s*[$#>]\s*` strip. `sigil`
/// selects the bullet set; the JS '#' heading removal happens before the TTS
/// `[$#>]` pass removes '#' characters entirely, so the TTS variant only ever
/// sees '$' and '>' in practice.
/// 中文说明：按行剥离行首 bullet、ATX 标题与 `$`/`#`/`>` 符号前缀；
/// 三个开关各自独立生效（实际 TTS 路径只会遇到 `$` 与 `>`）。
fn strip_line_prefixes(text: &str, bullets: bool, headings: bool, sigils: bool) -> String {
    let mut result = String::with_capacity(text.len());
    for line in text.split('\n') {
        let trimmed = line.trim_start_matches([' ', '\t']);
        let mut rest = trimmed;
        if bullets {
            let mut characters = rest.chars();
            if let Some(first) = characters.next()
                && matches!(first, '-' | '*' | '+')
                && rest[1..].starts_with(is_js_whitespace)
            {
                rest = rest[1..].trim_start_matches(is_js_whitespace);
            }
        }
        if headings {
            let hashes = rest
                .chars()
                .take_while(|&character| character == '#')
                .count();
            if (1..=6).contains(&hashes)
                && rest[hashes..].chars().next().is_some_and(is_js_whitespace)
            {
                rest = rest[hashes..].trim_start_matches(is_js_whitespace);
            }
        }
        if sigils {
            let leading = rest.len() - rest.trim_start_matches(is_js_whitespace).len();
            let after_ws = &rest[leading..];
            if let Some(first) = after_ws.chars().next()
                && matches!(first, '$' | '#' | '>')
            {
                rest = after_ws[1..].trim_start_matches(is_js_whitespace);
            }
        }
        result.push_str(rest);
        result.push('\n');
    }
    result.pop();
    result
}

// ---------------------------------------------------------------------------
// Sanitizers
// ---------------------------------------------------------------------------

/// `sanitizeForTTS`: markdown, code, URLs, paths and punctuation removed.
/// 中文说明：sanitizeForTTS——为语音朗读清除 markdown、代码、URL、
/// 路径与各类标点，共八步流水线，最后折叠空白。
pub fn sanitize_for_tts(text: &str) -> String {
    if text.is_empty() {
        return String::new();
    }
    // 1. Fenced blocks and inline code become spaces.
    let text = replace_fenced_code(text, " ");
    let text = replace_delimited(&text, "`", false);
    // 2. Remove emphasis/backtick/hash characters (`[*_~`#]`).
    let text: String = text
        .chars()
        .filter(|character| !matches!(character, '*' | '_' | '~' | '`' | '#'))
        .collect();
    // 3. Per-line `^\s*[$#>]\s*`.
    let text = strip_line_prefixes(&text, false, false, true);
    // 4. Shell-ish punctuation `[|&;<>]` → space.
    let text: String = text
        .chars()
        .map(|character| {
            if matches!(character, '|' | '&' | ';' | '<' | '>') {
                ' '
            } else {
                character
            }
        })
        .collect();
    // 5. Drop backslashes and brackets/braces/parens/quotes.
    let text: String = text
        .chars()
        .filter(|character| {
            !matches!(
                character,
                '\\' | '[' | ']' | '{' | '}' | '(' | ')' | '"' | '\''
            )
        })
        .collect();
    // 6. URLs → " a link ".
    let text = replace_urls(&text, " a link ");
    // 7. Slash-prefixed path runs `/[\w\-./]+` removed.
    let text = strip_path_runs(&text);
    // 8. Collapse whitespace.
    collapse_whitespace(&text)
}

/// `https?:\/\/[^\s]+` → `replacement`.
/// 中文说明：匹配 http/https URL（延伸到下一个空白字符）并整体替换为
/// replacement。
fn replace_urls(text: &str, replacement: &str) -> String {
    let bytes: Vec<char> = text.chars().collect();
    let mut result = String::with_capacity(text.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes.get(index) == Some(&'h')
            && bytes.get(index + 1) == Some(&'t')
            && bytes.get(index + 2) == Some(&'t')
            && bytes.get(index + 3) == Some(&'p')
        {
            let scheme_len = if bytes.get(index + 4) == Some(&'s') {
                5
            } else {
                4
            };
            if bytes.get(index + scheme_len) == Some(&':')
                && bytes.get(index + scheme_len + 1) == Some(&'/')
                && bytes.get(index + scheme_len + 2) == Some(&'/')
            {
                let start = index + scheme_len + 3;
                let end = (start..bytes.len())
                    .find(|&at| is_js_whitespace(bytes[at]))
                    .unwrap_or(bytes.len());
                result.push_str(replacement);
                index = end;
                continue;
            }
        }
        result.push(bytes[index]);
        index += 1;
    }
    result
}

/// `/[\w\-./]+` — maximal runs starting with '/' of word/dash/dot/slash.
/// 中文说明：删除以 `/` 开头的最长路径片段（字母数字/连字符/点/斜杠
/// 组成的连续段）。
fn strip_path_runs(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut result = String::with_capacity(text.len());
    let mut index = 0;
    while index < chars.len() {
        if chars[index] == '/' {
            let end = (index..chars.len())
                .find(|&at| !(is_js_word(chars[at]) || matches!(chars[at], '-' | '.' | '/')))
                .unwrap_or(chars.len());
            index = end;
            continue;
        }
        result.push(chars[index]);
        index += 1;
    }
    result
}

/// `sanitizeForNotification`：去除围栏代码、bullet/标题前缀、各类强调
/// 标记与 markdown 链接壳（保留内部文本），最后折叠空白。
fn sanitize_for_notification(text: &str) -> String {
    if text.is_empty() {
        return String::new();
    }
    let text = replace_fenced_code(text, " ");
    let text = replace_delimited(&text, "`", true);
    let text = strip_line_prefixes(&text, true, true, false);
    let text = replace_delimited(&text, "**", true);
    let text = replace_delimited(&text, "__", true);
    let text = replace_delimited(&text, "*", true);
    let text = replace_delimited(&text, "_", true);
    let text = replace_markdown_links(&text);
    collapse_whitespace(&text)
}

/// `sanitizeForNote`: like notification but URLs and quotes are removed.
/// 中文说明：sanitizeForNote——同通知清洗，但 URL 整体删除（替换为空）
/// 且额外去掉单双引号。
pub fn sanitize_for_note(text: &str) -> String {
    if text.is_empty() {
        return String::new();
    }
    let text = replace_fenced_code(text, " ");
    let text = replace_delimited(&text, "`", true);
    let text = strip_line_prefixes(&text, true, true, false);
    let text = replace_delimited(&text, "**", true);
    let text = replace_delimited(&text, "__", true);
    let text = replace_delimited(&text, "*", true);
    let text = replace_delimited(&text, "_", true);
    let text = replace_markdown_links(&text);
    let text = replace_urls(&text, "");
    let text: String = text
        .chars()
        .filter(|character| *character != '"' && *character != '\'')
        .collect();
    collapse_whitespace(&text)
}

/// 按模式分派到对应的清洗器（note/notification/tts）。
fn sanitize_by_mode(text: &str, mode: SummaryMode) -> String {
    match mode {
        SummaryMode::Note => sanitize_for_note(text),
        SummaryMode::Notification => sanitize_for_notification(text),
        SummaryMode::Tts => sanitize_for_tts(text),
    }
}

// ---------------------------------------------------------------------------
// Distillation fallbacks
// ---------------------------------------------------------------------------

/// `(?<=[.!?])\s+` sentence split.
/// 中文说明：在 `.`/`!`/`?` 后跟空白处断句并 trim；结尾没有终止符的
/// 残句也作为一句返回。
fn split_sentences(text: &str) -> Vec<String> {
    let mut sentences = Vec::new();
    let mut current = String::new();
    let chars: Vec<char> = text.chars().collect();
    let mut index = 0;
    while index < chars.len() {
        let character = chars[index];
        current.push(character);
        if matches!(character, '.' | '!' | '?') {
            let next = chars.get(index + 1);
            if next.is_none() || next.is_some_and(|next| is_js_whitespace(*next)) {
                let trimmed = current.trim();
                if !trimmed.is_empty() {
                    sentences.push(trimmed.to_string());
                }
                current.clear();
            }
        }
        index += 1;
    }
    let trimmed = current.trim();
    if !trimmed.is_empty() {
        sentences.push(trimmed.to_string());
    }
    sentences
}

/// 大小写不敏感地剥离任一候选前缀；都不匹配时原样返回借用的原文。
fn strip_prefix_ci<'a>(text: &'a str, candidates: &[&str]) -> &'a str {
    for candidate in candidates {
        if text.len() >= candidate.len() && text[..candidate.len()].eq_ignore_ascii_case(candidate)
        {
            return &text[candidate.len()..];
        }
    }
    text
}

/// `distillNoteFallback`.
/// 中文说明：distillNoteFallback——剥离 "In summary"/"Here's a note"
/// 式开场白后取首句，再按分号/冒号/括号/破折号与逗号依次截短；仍超
/// 上限（min(maxLength, 65% 比例) 且不低于 32）时截断加省略号。
fn distill_note_fallback(text: &str, max_length: u64) -> String {
    let sanitized = sanitize_for_note(text);
    if sanitized.is_empty() {
        return String::new();
    }
    // `^In summary[:,]?\s*` then `^Here(?:s| is) (?:a )?note[:,]?\s*` (ci).
    let normalized = strip_prefix_ci(&sanitized, &["In summary:", "In summary,"]).trim_start();
    let normalized = strip_prefix_ci(normalized.trim(), &["In summary"]).trim();
    let normalized = {
        let lower = normalized.to_lowercase();
        let mut rest: &str = normalized;
        for prefix in ["heres ", "here is "] {
            if lower.starts_with(prefix) {
                rest = &normalized[prefix.len()..];
                let lower_rest = rest.to_lowercase();
                if lower_rest.starts_with("a ") {
                    rest = &rest[2..];
                }
                let lower_rest = rest.to_lowercase();
                if lower_rest.starts_with("note") {
                    rest = &rest["note".len()..];
                    let trimmed = rest.trim_start_matches([',', ':']).trim_start();
                    rest = trimmed;
                }
                break;
            }
        }
        rest.trim().to_string()
    };

    let sentences = split_sentences(&normalized);
    let base = sentences
        .first()
        .cloned()
        .unwrap_or_else(|| normalized.clone());
    // Split on `[;:()-]\s+` then `,\s+`, keep the first segment.
    let segment_after = |text: &str| -> String {
        let chars: Vec<char> = text.chars().collect();
        let mut index = 0;
        while index < chars.len() {
            if matches!(chars[index], ';' | ':' | '(' | ')' | '-') {
                let next = chars.get(index + 1);
                let after = chars.get(index + 2);
                if next.is_some_and(|next| is_js_whitespace(*next))
                    && after.is_some_and(|character| !is_js_whitespace(*character))
                {
                    return text[..char_byte_index(text, index)].trim().to_string();
                }
            }
            index += 1;
        }
        text.trim().to_string()
    };
    let after_punctuation = segment_after(&base);
    let best = {
        // `,\s+` split.
        let chars: Vec<char> = after_punctuation.chars().collect();
        let mut index = 0;
        let mut cut = None;
        while index + 1 < chars.len() {
            if chars[index] == ','
                && is_js_whitespace(chars[index + 1])
                && chars
                    .get(index + 2)
                    .is_some_and(|character| !is_js_whitespace(*character))
            {
                cut = Some(index);
                break;
            }
            index += 1;
        }
        match cut {
            Some(cut) => after_punctuation[..char_byte_index(&after_punctuation, cut)]
                .trim()
                .to_string(),
            None => after_punctuation.trim().to_string(),
        }
    };

    let normalized_len = utf16_len(&normalized) as f64;
    let ideal_limit = (max_length as f64).min(32.0_f64.max((normalized_len * 0.65).floor()));
    let ideal_limit = ideal_limit.max(0.0) as usize;

    if utf16_len(&best) <= ideal_limit {
        return best;
    }
    let clipped = utf16_prefix(&best, ideal_limit.saturating_sub(1))
        .trim()
        .to_string();
    if !clipped.is_empty() {
        format!("{clipped}\u{2026}")
    } else {
        utf16_prefix(&best, ideal_limit).trim().to_string()
    }
}

/// 把字符下标换算成字节下标（Rust 字符串切片需要）；越界返回文本长度。
fn char_byte_index(text: &str, char_index: usize) -> usize {
    text.char_indices()
        .nth(char_index)
        .map(|(byte_index, _)| byte_index)
        .unwrap_or(text.len())
}

/// `distillNotificationFallback`.
/// 中文说明：distillNotificationFallback——取首个至少 20 字符的句子
/// （否则首句），超限时截断加省略号；max_length 缺失或非法时上限 100。
fn distill_notification_fallback(text: &str, max_length: Option<f64>) -> String {
    let sanitized = sanitize_for_notification(text);
    if sanitized.is_empty() {
        return String::new();
    }
    let sentences = split_sentences(&sanitized);
    let candidate = sentences
        .iter()
        .find(|sentence| utf16_len(sentence) >= 20)
        .cloned()
        .or_else(|| sentences.first().cloned())
        .unwrap_or_else(|| sanitized.clone());
    let limit = match max_length {
        Some(value) if value.is_finite() => (value.floor().max(20.0)) as usize,
        _ => 100,
    };
    if utf16_len(&candidate) <= limit {
        return candidate;
    }
    let clipped = utf16_prefix(&candidate, limit.saturating_sub(1))
        .trim()
        .to_string();
    if !clipped.is_empty() {
        format!("{clipped}\u{2026}")
    } else {
        utf16_prefix(&candidate, limit).trim().to_string()
    }
}

/// 按模式分派到对应的蒸馏兜底；TTS 没有蒸馏步骤，仅做清洗。
fn fallback_by_mode(text: &str, max_length: Option<f64>, mode: SummaryMode) -> String {
    match mode {
        SummaryMode::Note => distill_note_fallback(text, max_length.unwrap_or(0.0) as u64),
        SummaryMode::Notification => distill_notification_fallback(text, max_length),
        SummaryMode::Tts => sanitize_by_mode(text, mode),
    }
}

// ---------------------------------------------------------------------------
// The seam + entrypoint
// ---------------------------------------------------------------------------

/// 传给模型摘要接缝的请求：待摘要全文与目标长度上限。
pub struct SummaryModelRequest {
    /// 待摘要的原始文本。
    pub text: String,
    /// 期望的摘要长度上限（UTF-16 码元）。
    pub max_length: u64,
}

/// 模型摘要接缝返回的 future；Err 为人类可读的失败原因。
pub type SummaryModelFuture = BoxFuture<'static, Result<String, String>>;
/// 可注入的模型摘要来源（为已退役的 zen 提供者预留的前向兼容位）。
pub type ModelSummarySource = Arc<dyn Fn(SummaryModelRequest) -> SummaryModelFuture + Send + Sync>;

/// summarize_text 的入参集合（对应 JS summarizeText 的四个参数）。
pub struct SummarizeParams<'a> {
    /// 待处理文本；None 视为空串（reason 记 "No text provided"）。
    pub text: Option<&'a str>,
    /// 触发摘要的长度阈值（UTF-16 码元）；不超过则只清洗不摘要。
    pub threshold: u64,
    /// 摘要长度上限；None/非法值由各模式兜底自行取默认。
    pub max_length: Option<f64>,
    /// 摘要模式，决定清洗与蒸馏策略。
    pub mode: SummaryMode,
}

/// `summarizeText`. With `model: None` this is byte-for-byte the JS stub:
/// the local fallback always answers and `summarized` is always false.
/// A provided seam is used for over-threshold text (forward-compat for the
/// retired zen provider's replacement; the production wiring passes `None`).
/// 中文说明：summarizeText——model 为 None 时与 JS 桩逐字节一致：始终
/// 由本地兜底回答、summarized 恒为 false；提供接缝时仅用于超阈值文本
/// （生产接线传 None）。
pub async fn summarize_text(
    params: SummarizeParams<'_>,
    model: Option<ModelSummarySource>,
) -> Value {
    let text = params.text.unwrap_or("");
    let summary = fallback_by_mode(text, params.max_length, params.mode);
    let has_text = !text.is_empty();
    if !has_text || utf16_len(text) as u64 <= params.threshold {
        return json_object(
            summary,
            false,
            if has_text {
                Some("Text under threshold")
            } else {
                Some("No text provided")
            },
            None,
            None,
        );
    }

    if let Some(model) = model {
        let max_length = params
            .max_length
            .filter(|value| value.is_finite() && *value > 0.0)
            .unwrap_or(500.0) as u64;
        if let Ok(model_text) = model(SummaryModelRequest {
            text: text.to_string(),
            max_length,
        })
        .await
        {
            let model_text = model_text.trim();
            if !model_text.is_empty() {
                let clipped = if utf16_len(model_text) as u64 <= max_length {
                    model_text.to_string()
                } else {
                    format!(
                        "{}\u{2026}",
                        utf16_prefix(model_text, max_length.saturating_sub(1) as usize).trim()
                    )
                };
                let summary_length = utf16_len(&clipped);
                return json_object(
                    clipped,
                    true,
                    None,
                    Some(summary_length),
                    Some(utf16_len(text)),
                );
            }
        }
    }

    json_object(
        summary.clone(),
        false,
        Some("Model summarization provider unavailable"),
        Some(utf16_len(&summary)),
        Some(utf16_len(text)),
    )
}
/// 组装 JS 形状的返回对象（summary/summarized/reason/summaryLength/
/// originalLength）；可选参数缺省时对应键整个省略。
fn json_object(
    summary: String,
    summarized: bool,
    reason: Option<&str>,
    summary_length: Option<usize>,
    original_length: Option<usize>,
) -> Value {
    let mut object = Map::new();
    object.insert("summary".to_string(), Value::String(summary));
    object.insert("summarized".to_string(), Value::Bool(summarized));
    if let Some(reason) = reason {
        object.insert("reason".to_string(), Value::String(reason.to_string()));
    }
    if let Some(length) = original_length {
        object.insert("originalLength".to_string(), Value::from(length as u64));
    }
    if let Some(length) = summary_length {
        object.insert("summaryLength".to_string(), Value::from(length as u64));
    }
    Value::Object(object)
}

/// `summarizeText` default entrypoint (no model seam) matching the JS export
/// shape callers build with plain arguments.
/// 中文说明：summarizeText 的默认入口（无模型接缝），参数形状与 JS
/// 导出一致，供调用方按普通参数直接构造。
pub async fn summarize_text_simple(
    text: Option<&str>,
    threshold: u64,
    max_length: Option<f64>,
    mode: &str,
) -> Value {
    summarize_text(
        SummarizeParams {
            text,
            threshold,
            max_length,
            mode: SummaryMode::from_str(mode),
        },
        None,
    )
    .await
}
