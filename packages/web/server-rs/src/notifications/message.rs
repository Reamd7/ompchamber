//! Port of `server/lib/notifications/message.js`: notification text
//! truncation and plain-text normalization. The JS regex chain is
//! hand-rolled (no regex crate) with identical pass order and match
//! semantics (non-greedy pairs, single-line vs crossing-newline spans).

pub const DEFAULT_NOTIFICATION_MESSAGE_MAX_LENGTH: usize = 250;

/// `resolvePositiveNumber`: missing / non-finite / non-positive numbers
/// fall back to the provided default.
fn resolve_positive_number(value: Option<f64>, fallback: usize) -> usize {
    match value {
        Some(number) if number.is_finite() && number > 0.0 => number.trunc() as usize,
        _ => fallback,
    }
}

fn js_whitespace(character: char) -> bool {
    character.is_whitespace()
}

/// Replace every non-greedy `opener…closer` span. `fixed` mirrors a literal
/// replacement; `None` keeps the span content (`$1`). `cross_newline`
/// mirrors `[\s\S]*?` versus `.*?` (which cannot cross a newline).
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncates_oversized_notification_text() {
        assert_eq!(truncate_notification_text("abcdef", Some(3.0)), "abc...");
        assert_eq!(truncate_notification_text("abc", Some(3.0)), "abc");
        assert_eq!(truncate_notification_text("", Some(3.0)), "");
    }

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

    #[test]
    fn ignores_retired_summarization_settings_and_truncates() {
        assert_eq!(
            prepare_notification_last_message("0123456789", Some(4.0)),
            "0123..."
        );
    }

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

    #[test]
    fn non_matching_markers_stay_literal() {
        assert_eq!(
            normalize_notification_plain_text("---\n#hashtag 5#6 x"),
            "--- #hashtag 5#6 x"
        );
        assert_eq!(normalize_notification_plain_text(""), "");
    }

    #[test]
    fn unicode_text_truncates_on_char_boundaries() {
        assert_eq!(
            truncate_notification_text("héllö wörld héllö wörld", Some(7.0)),
            "héllö w..."
        );
    }
}
