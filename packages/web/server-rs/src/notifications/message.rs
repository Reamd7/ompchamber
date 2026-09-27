//! Port of `server/lib/notifications/message.js`: notification text
//! truncation and plain-text normalization. The JS regex chain is
//! hand-rolled (no regex crate) with identical pass order and match
//! semantics (non-greedy pairs, single-line vs crossing-newline spans).
//!
//! 中文概述：把 markdown 风格的 agent 消息压平成单行纯文本并按上限
//! 截断（默认 250 字符）。JS 端的正则链在此以手写扫描复刻，各 pass
//! 的顺序、非贪婪配对与跨行能力都与 JS 保持一致，保证两端输出逐字符
//! 相同 —— 该输出直接进入推送正文与原生通知标题。

/// 截断默认上限：250 字符（与 JS 端同名默认值一致）。
pub const DEFAULT_NOTIFICATION_MESSAGE_MAX_LENGTH: usize = 250;

/// `resolvePositiveNumber`: missing / non-finite / non-positive numbers
/// fall back to the provided default.
/// 中文：入参 `Option<f64>` 是 JS 可选数字的直接映射；正数截断取整，
/// 其余情况（缺失/NaN/Inf/0/负数）一律回退 fallback。
fn resolve_positive_number(value: Option<f64>, fallback: usize) -> usize {
    match value {
        Some(number) if number.is_finite() && number > 0.0 => number.trunc() as usize,
        _ => fallback,
    }
}

/// JS `\s` 的判定等价物：`char::is_whitespace` 覆盖 Unicode 空白类别，
/// 与 JS 正则的 `\s` 语义在常见输入上一致。
fn js_whitespace(character: char) -> bool {
    character.is_whitespace()
}

/// Replace every non-greedy `opener…closer` span. `fixed` mirrors a literal
/// replacement; `None` keeps the span content (`$1`). `cross_newline`
/// mirrors `[\s\S]*?` versus `.*?` (which cannot cross a newline).
/// 中文：实现要点：找到 opener 后仅在限定范围内找最近 closer；
/// 未配对的 opener 原样保留并从其后继续扫描 —— 与 JS 非贪婪正则的
/// 行为完全一致，也不会死循环。
fn replace_spans(
    text: &str,
    opener: &str,
    closer: &str,
    fixed: Option<&str>,
    cross_newline: bool,
) -> String {
    let mut result = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(open_index) = rest.find(opener) {
        let after_open = &rest[open_index + opener.len()..];
        let search_end = if cross_newline {
            after_open.len()
        } else {
            after_open.find('\n').unwrap_or(after_open.len())
        };
        let searchable = &after_open[..search_end];
        if let Some(close_offset) = searchable.find(closer) {
            result.push_str(&rest[..open_index]);
            match fixed {
                Some(replacement) => result.push_str(replacement),
                None => result.push_str(&searchable[..close_offset]),
            }
            rest = &after_open[close_offset + closer.len()..];
        } else {
            // Unmatched opener stays literal; matching continues after it.
            result.push_str(&rest[..open_index + opener.len()]);
            rest = after_open;
        }
    }
    result.push_str(rest);
    result
}

/// `^[\t ]*[-*+]\s+` (multiline): strip list markers from line starts.
/// The `\s+` requirement means the marker must be followed by whitespace.
/// 中文：仅在「行首（可含 tab/空格缩进）出现 -/*/+ 且后跟空白」时剥
/// 离标记；`#hashtag`、`5*3` 之类不受影响。
fn strip_list_markers(text: &str) -> String {
    text.lines()
        .map(|line| {
            let body = line.trim_start_matches(['\t', ' ']);
            let after_marker = body.strip_prefix(['-', '*', '+']).unwrap_or(body);
            if after_marker.len() != body.len() {
                match after_marker.chars().next() {
                    Some(character) if js_whitespace(character) => {
                        return after_marker.trim_start_matches(js_whitespace);
                    }
                    // No whitespace after the marker: not a list item.
                    _ => return line,
                }
            }
            line
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// `^#{1,6}\s+` (multiline): strip heading markers. No leading-whitespace
/// allowance — the hash must be the first character of the line.
/// 中文：行首 1–6 个 `#` 且后跟空白才剥离；`#` 不在行首或后无空白则
/// 整行原样保留。
fn strip_heading_markers(text: &str) -> String {
    text.lines()
        .map(|line| {
            let hashes = line.chars().take_while(|c| *c == '#').count();
            if (1..=6).contains(&hashes) {
                let after = &line[hashes..];
                match after.chars().next() {
                    Some(character) if js_whitespace(character) => {
                        return after.trim_start_matches(js_whitespace);
                    }
                    _ => return line,
                }
            }
            line
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// `\s*\n\s*` then `\s+` then trim: every whitespace run becomes one
/// space, with no leading/trailing space.
/// 中文：一次遍历完成「换行折叠 + 连续空白压成单空格 + 去首尾」，
/// 结果恒为不含换行的单行文本；与 JS 两次 replace + trim 等价。
fn collapse_whitespace(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut pending_space = false;
    for character in text.chars() {
        if js_whitespace(character) {
            pending_space = true;
        } else {
            if pending_space && !result.is_empty() {
                result.push(' ');
            }
            pending_space = false;
            result.push(character);
        }
    }
    result
}

/// `\[(.*?)\]\((.*?)\)` → link text (single line).
/// 中文：`[text](url)`（text 与 url 均不跨行）替换为 text；未闭合的
/// `[` 原样保留，扫描从其后继续。
fn strip_links(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(open_index) = rest.find('[') {
        let after_open = &rest[open_index + 1..];
        let line_end = after_open.find('\n').unwrap_or(after_open.len());
        let line = &after_open[..line_end];
        let Some(bracket_close) = line.find("](") else {
            result.push_str(&rest[..open_index + 1]);
            rest = after_open;
            continue;
        };
        let after_bracket = &line[bracket_close + 2..];
        let Some(paren_close) = after_bracket.find(')') else {
            result.push_str(&rest[..open_index + 1]);
            rest = after_open;
            continue;
        };
        result.push_str(&rest[..open_index]);
        result.push_str(&line[..bracket_close]);
        rest = &after_open[bracket_close + 2 + paren_close + 1..];
    }
    result.push_str(rest);
    result
}

/// `normalizeNotificationPlainText`: markdown-like cleanup to one plain
/// line, preserving the JS pass order.
/// 中文：完整流水线（顺序不可换，与 JS 相同）：代码围栏 → 行内代码 →
/// 列表标记 → 标题标记 → 强调/斜体（** __ * _）→ 链接 → 空白折叠。
pub fn normalize_notification_plain_text(text: &str) -> String {
    let text = replace_spans(text, "```", "```", Some(" "), true);
    let text = replace_spans(&text, "`", "`", None, true);
    let text = strip_list_markers(&text);
    let text = strip_heading_markers(&text);
    let mut text = text;
    for marker in ["**", "__", "*", "_"] {
        text = replace_spans(&text, marker, marker, None, false);
    }
    let text = strip_links(&text);
    collapse_whitespace(&text)
}

/// `truncateNotificationText`: `text.slice(0, maxLength)` + `...`.
/// Character-counted (JS counts UTF-16 units; identical for BMP text).
/// 中文：按字符（Unicode scalar）计数，超限截取后补 `...`；
/// 非正/非有限上限回退默认 250。
pub fn truncate_notification_text(text: &str, max_length: Option<f64>) -> String {
    let safe_max_length =
        resolve_positive_number(max_length, DEFAULT_NOTIFICATION_MESSAGE_MAX_LENGTH);
    if text.chars().count() <= safe_max_length {
        return text.to_string();
    }
    let truncated: String = text.chars().take(safe_max_length).collect();
    format!("{truncated}...")
}

/// `prepareNotificationLastMessage`: normalize, then truncate to the
/// caller's `maxLastMessageLength` (default 250).
/// 中文：空消息直接返回空串，不做任何归一化；其余先压平再截断，
/// 是通知正文进推送/原生通道前的最后一道处理。
pub fn prepare_notification_last_message(
    message: &str,
    max_last_message_length: Option<f64>,
) -> String {
    if message.is_empty() {
        return String::new();
    }
    let plain = normalize_notification_plain_text(message);
    truncate_notification_text(&plain, max_last_message_length)
}

/// 文案处理单元测试：截断边界、回退默认值与归一化 pass 顺序。
#[cfg(test)]
mod tests {
    use super::*;

/// 验证超长截断补省略号、恰好等长与空串均原样返回。
    #[test]
    fn truncates_oversized_notification_text() {
        assert_eq!(truncate_notification_text("abcdef", Some(3.0)), "abc...");
        assert_eq!(truncate_notification_text("abc", Some(3.0)), "abc");
        assert_eq!(truncate_notification_text("", Some(3.0)), "");
    }

/// 验证缺失/0/NaN 上限回退默认 250，正小数按整数部分截断。
    #[test]
    fn invalid_max_length_falls_back_to_the_default() {
        let expected = format!("{}...", "x".repeat(250));
        assert_eq!(truncate_notification_text(&"x".repeat(260), None), expected);
        assert_eq!(
            truncate_notification_text(&"x".repeat(260), Some(0.0)),
            expected
        );
        assert_eq!(
            truncate_notification_text(&"x".repeat(260), Some(f64::NAN)),
            expected
        );
        // Positive non-integer lengths truncate to integers.
        assert_eq!(truncate_notification_text("abcdef", Some(3.9)), "abc...");
    }

/// 验证 maxLastMessageLength 正常生效（旧 summarization 设置已退役，不再参与）。
    #[test]
    fn ignores_retired_summarization_settings_and_truncates() {
        assert_eq!(
            prepare_notification_last_message("0123456789", Some(4.0)),
            "0123..."
        );
    }

/// 验证完整 markdown（粗体、列表、行内代码）压平为单行纯文本。
    #[test]
    fn normalizes_markdown_message_to_plain_text() {
        assert_eq!(
            prepare_notification_last_message(
                "**Committed.**\n\n- Commit: `85924b9d`\n- Message: `fix desktop notifications`",
                Some(200.0)
            ),
            "Committed. Commit: 85924b9d Message: fix desktop notifications"
        );
    }

/// 验证标题、链接、代码围栏与多种强调标记的剥离结果。
    #[test]
    fn normalizes_headings_links_and_fenced_blocks() {
        assert_eq!(
            normalize_notification_plain_text("### Title\nsee [docs](https://x.dev/a) now"),
            "Title see docs now"
        );
        assert_eq!(
            normalize_notification_plain_text("before ```code\nblock``` after"),
            "before after"
        );
        assert_eq!(
            normalize_notification_plain_text("*italic* and _under_ and __bold__"),
            "italic and under and bold"
        );
    }

/// 验证未配对标记（`---`、`#hashtag`、`5#6`）保持字面量不被误删。
    #[test]
    fn non_matching_markers_stay_literal() {
        assert_eq!(
            normalize_notification_plain_text("---\n#hashtag 5#6 x"),
            "--- #hashtag 5#6 x"
        );
        assert_eq!(normalize_notification_plain_text(""), "");
    }

/// 验证截断按字符边界进行，不切出半个字符。
    #[test]
    fn unicode_text_truncates_on_char_boundaries() {
        assert_eq!(
            truncate_notification_text("héllö wörld héllö wörld", Some(7.0)),
            "héllö w..."
        );
    }
}
