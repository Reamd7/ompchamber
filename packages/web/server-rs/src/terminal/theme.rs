//! Port of `server/lib/terminal/theme-response.js`.
//!
//! The PTY itself answers theme/capability queries emitted by shells and TUIs —
//! including queries that arrive before any WebSocket attachment exists — so
//! startup handshakes (Fish's DA1 probe, OpenTUI's mode-2031/OSC 10/11 cycle,
//! kitty keyboard probes) never block on a renderer. Byte-level port: every
//! recognized sequence is ASCII.

const MODE_SET: &[u8] = b"\x1b[?2031h";
const MODE_RESET: &[u8] = b"\x1b[?2031l";
const CAPABILITY_QUERY: &[u8] = b"\x1b[?2031$p";
const MODE_QUERIES: [&[u8]; 2] = [b"\x1b[?996n", b"\x1b[?997n"];
/// Fish asks this before an unattached browser terminal can reply.
const PRIMARY_DEVICE_ATTRIBUTE_QUERIES: [&[u8]; 2] = [b"\x1b[c", b"\x1b[0c"];
const PRIMARY_DEVICE_ATTRIBUTE_RESPONSE: &str = "\x1b[?1;2c";
/// Kitty keyboard protocol queries: answer flags=0 so clients fall back at once.
const KITTY_PRIMARY_QUERY: &[u8] = b"\x1b[?u";
const KITTY_PRIMARY_RESPONSE: &str = "\x1b[?0u";
const KITTY_SECONDARY_QUERY: &[u8] = b"\x1b[?>u";
const KITTY_SECONDARY_RESPONSE: &str = "\x1b[?>0;0u";

/// OSC 10/11 queries with both BEL and ST terminators.
const OSC_QUERIES: [(u16, &[u8]); 4] = [
    (10, b"\x1b]10;?\x07"),
    (10, b"\x1b]10;?\x1b\\"),
    (11, b"\x1b]11;?\x07"),
    (11, b"\x1b]11;?\x1b\\"),
];

/// Sequences whose prefixes may straddle PTY chunks (pending carry).
const CONTROL_SEQUENCES: [&[u8]; 11] = [
    MODE_SET,
    MODE_RESET,
    CAPABILITY_QUERY,
    MODE_QUERIES[0],
    MODE_QUERIES[1],
    PRIMARY_DEVICE_ATTRIBUTE_QUERIES[0],
    PRIMARY_DEVICE_ATTRIBUTE_QUERIES[1],
    OSC_QUERIES[0].1,
    OSC_QUERIES[1].1,
    OSC_QUERIES[2].1,
    OSC_QUERIES[3].1,
];

/// The appearance half of a terminal session (creation + `applyAppearance`).
#[derive(Debug, Clone, Default)]
pub struct Appearance {
    pub theme_mode: String,
    pub terminal_background: Option<String>,
    pub terminal_foreground: Option<String>,
    pub mode_enabled: bool,
}

/// `parseColor`: `#rgb`/`#rrggbb` (case-insensitive) or an `rgb()`/`rgba()`
/// prefix match; each channel clamped to 255 on the rgb() path.
fn parse_color(value: Option<&str>) -> Option<(u8, u8, u8)> {
    let value = value?;
    let Some(hex) = value.strip_prefix('#') else {
        return rgb_prefix(value);
    };
    let expanded: (u8, u8, u8) = if hex.len() == 3 && hex.chars().all(|c| c.is_ascii_hexdigit()) {
        let digit = |c: char| c.to_digit(16).unwrap_or(0) as u8;
        let chars: Vec<char> = hex.chars().collect();
        let r = digit(chars[0]);
        let g = digit(chars[1]);
        let b = digit(chars[2]);
        (r * 17, g * 17, b * 17)
    } else if hex.len() == 6 && hex.chars().all(|c| c.is_ascii_hexdigit()) {
        let byte = |range: &str| u8::from_str_radix(range, 16).unwrap_or(0);
        let bytes = hex.as_bytes();
        (
            byte(std::str::from_utf8(&bytes[0..2]).ok()?),
            byte(std::str::from_utf8(&bytes[2..4]).ok()?),
            byte(std::str::from_utf8(&bytes[4..6]).ok()?),
        )
    } else {
        return rgb_prefix(value);
    };
    Some(expanded)
}

/// `^rgba?\(\s*(\d+)\s*,\s*(\d+)\s*,\s*(\d+)` — prefix match, channels capped.
fn rgb_prefix(value: &str) -> Option<(u8, u8, u8)> {
    let lower = value.to_ascii_lowercase();
    let rest = lower
        .strip_prefix("rgb(")
        .or_else(|| lower.strip_prefix("rgba("))?;
    let rest = rest.trim_start();
    let parts: Vec<&str> = rest.split(',').collect();
    let mut channels = [0u8; 3];
    for (index, slot) in channels.iter_mut().enumerate() {
        let part = parts.get(index)?.trim_start();
        let digits: String = part.chars().take_while(|c| c.is_ascii_digit()).collect();
        if digits.is_empty() {
            return None;
        }
        *slot = digits.parse::<u32>().ok()?.min(255) as u8;
        // JS `\s*,` between channels; the regex is a prefix match, so the
        // final channel may carry anything after its digits.
        if index + 1 < parts.len() && !part[digits.len()..].trim().is_empty() {
            return None;
        }
    }
    Some((channels[0], channels[1], channels[2]))
}

/// `colorReport`: `ESC ] <code> ; rgb:rrrr/gggg/bbbb ST` — each channel is its
/// hex byte repeated (xterm reports 16-bit channels as doubled 8-bit values).
fn color_report(code: u16, color: Option<&str>) -> Option<String> {
    let (r, g, b) = parse_color(color)?;
    let channel = |v: u8| format!("{v:02x}{v:02x}");
    Some(format!(
        "\x1b]{code};rgb:{}/{}/{}\x1b\\",
        channel(r),
        channel(g),
        channel(b)
    ))
}

/// `terminalThemeModeReport`: light reports XTWINMODE 2, dark 1.
pub fn terminal_theme_mode_report(theme_mode: &str) -> String {
    format!("\x1b[?997;{}n", if theme_mode == "light" { 2 } else { 1 })
}

pub struct ThemeQueriesConsumed {
    pub pending: Vec<u8>,
    pub responses: Vec<String>,
    pub mode_enabled: bool,
}

fn starts_with_at(input: &[u8], index: usize, sequence: &[u8]) -> bool {
    input.len() >= index + sequence.len() && &input[index..index + sequence.len()] == sequence
}

/// `consumeTerminalThemeQueries(pending, data, appearance, { respondToPrimaryDeviceAttributes })`.
pub fn consume_terminal_theme_queries(
    pending: &[u8],
    data: &[u8],
    appearance: &Appearance,
    respond_to_primary_device_attributes: bool,
) -> ThemeQueriesConsumed {
    if pending.is_empty() && !data.contains(&0x1b) {
        return ThemeQueriesConsumed {
            pending: Vec::new(),
            responses: Vec::new(),
            mode_enabled: appearance.mode_enabled,
        };
    }
    let mut input = Vec::with_capacity(pending.len() + data.len());
    input.extend_from_slice(pending);
    input.extend_from_slice(data);

    let mut responses = Vec::new();
    let mut mode_enabled = appearance.mode_enabled;

    let mut index = 0;
    while index < input.len() {
        if starts_with_at(&input, index, MODE_SET) {
            mode_enabled = true;
            index += MODE_SET.len() - 1;
            continue;
        }
        if starts_with_at(&input, index, MODE_RESET) {
            mode_enabled = false;
            index += MODE_RESET.len() - 1;
            continue;
        }
        if starts_with_at(&input, index, CAPABILITY_QUERY) {
            responses.push(format!("\x1b[?2031;{}$y", if mode_enabled { 1 } else { 2 }));
            index += CAPABILITY_QUERY.len() - 1;
            continue;
        }
        if let Some(mode_query) = MODE_QUERIES
            .iter()
            .find(|q| starts_with_at(&input, index, q))
        {
            responses.push(terminal_theme_mode_report(&appearance.theme_mode));
            index += mode_query.len() - 1;
            continue;
        }
        if starts_with_at(&input, index, KITTY_PRIMARY_QUERY) {
            responses.push(KITTY_PRIMARY_RESPONSE.to_string());
            index += KITTY_PRIMARY_QUERY.len() - 1;
            continue;
        }
        if starts_with_at(&input, index, KITTY_SECONDARY_QUERY) {
            responses.push(KITTY_SECONDARY_RESPONSE.to_string());
            index += KITTY_SECONDARY_QUERY.len() - 1;
            continue;
        }
        let primary_da = PRIMARY_DEVICE_ATTRIBUTE_QUERIES
            .iter()
            .find(|q| starts_with_at(&input, index, q));
        if let Some(query) = primary_da
            && respond_to_primary_device_attributes
        {
            // A shell can ask before any browser terminal is attached.
            // Answer with a conservative VT100 DA1 response so Fish does
            // not block startup on its ten-second query timeout.
            responses.push(PRIMARY_DEVICE_ATTRIBUTE_RESPONSE.to_string());
            index += query.len() - 1;
            continue;
        }
        // JS falls through without skipping the sequence when the
        // fallback is off; the bytes are then ordinary output.
        let osc_query = OSC_QUERIES
            .iter()
            .find(|(_, seq)| starts_with_at(&input, index, seq));
        if let Some((code, sequence)) = osc_query {
            let color = if *code == 10 {
                appearance.terminal_foreground.as_deref()
            } else {
                appearance.terminal_background.as_deref()
            };
            if let Some(response) = color_report(*code, color) {
                responses.push(response);
            }
            index += sequence.len() - 1;
        }
        index += 1;
    }

    // Longest suffix of the input that is a proper prefix of a recognized
    // control sequence — it may complete on the next chunk.
    let max_length = CONTROL_SEQUENCES
        .iter()
        .map(|seq| seq.len())
        .max()
        .unwrap_or(0);
    let mut next_pending = Vec::new();
    let limit = (input.len() + 1).min(max_length);
    for length in 1..limit {
        let suffix = &input[input.len() - length..];
        if CONTROL_SEQUENCES
            .iter()
            .any(|seq| seq.len() > length && seq.starts_with(suffix))
        {
            next_pending = suffix.to_vec();
        }
    }
    ThemeQueriesConsumed {
        pending: next_pending,
        responses,
        mode_enabled,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn light_appearance() -> Appearance {
        Appearance {
            theme_mode: "light".to_string(),
            terminal_foreground: Some("#1b1b1b".to_string()),
            terminal_background: Some("#faf8f0".to_string()),
            mode_enabled: false,
        }
    }

    fn consume(pending: &str, data: &str, appearance: &Appearance) -> ThemeQueriesConsumed {
        consume_terminal_theme_queries(pending.as_bytes(), data.as_bytes(), appearance, false)
    }

    #[allow(dead_code)]
    fn unused_marker() {}

    #[test]
    fn answers_the_complete_opentui_startup_handshake() {
        let result = consume(
            "",
            "\u{1b}[?2031h\u{1b}]10;?\u{1b}\\\u{1b}]11;?\u{1b}\\\u{1b}[?2031$p",
            &light_appearance(),
        );
        assert_eq!(result.pending, Vec::<u8>::new());
        assert!(result.mode_enabled);
        assert_eq!(
            result.responses,
            vec![
                "\u{1b}]10;rgb:1b1b/1b1b/1b1b\u{1b}\\".to_string(),
                "\u{1b}]11;rgb:fafa/f8f8/f0f0\u{1b}\\".to_string(),
                "\u{1b}[?2031;1$y".to_string(),
            ]
        );
    }

    #[test]
    fn handles_a_query_split_across_pty_output_chunks_without_duplicating_it() {
        let mut appearance = light_appearance();
        appearance.theme_mode = "dark".to_string();
        let first = consume("", "\u{1b}]11;", &appearance);
        appearance.mode_enabled = first.mode_enabled;
        let second = consume(
            std::str::from_utf8(&first.pending).unwrap_or(""),
            "?\u{1b}\\",
            &appearance,
        );
        appearance.mode_enabled = second.mode_enabled;
        let third = consume("", "x", &appearance);
        assert_eq!(
            second.responses,
            vec!["\u{1b}]11;rgb:fafa/f8f8/f0f0\u{1b}\\".to_string()]
        );
        assert_eq!(second.pending, Vec::<u8>::new());
        assert!(third.responses.is_empty());
    }

    #[test]
    fn answers_every_repeated_query_in_wire_order() {
        let result = consume(
            "",
            "\u{1b}[?996n\u{1b}[?996n\u{1b}]10;?\u{7}\u{1b}]10;?\u{7}",
            &light_appearance(),
        );
        assert_eq!(
            result.responses,
            vec![
                "\u{1b}[?997;2n".to_string(),
                "\u{1b}[?997;2n".to_string(),
                "\u{1b}]10;rgb:1b1b/1b1b/1b1b\u{1b}\\".to_string(),
                "\u{1b}]10;rgb:1b1b/1b1b/1b1b\u{1b}\\".to_string(),
            ]
        );
    }

    #[test]
    fn answers_a_primary_device_attribute_query_only_when_the_fallback_is_enabled() {
        let appearance = light_appearance();
        let attached = consume_terminal_theme_queries(b"", b"\x1b[0c", &appearance, false);
        let unattached = consume_terminal_theme_queries(b"", b"\x1b[0c", &appearance, true);
        assert!(attached.responses.is_empty());
        assert_eq!(unattached.responses, vec!["\u{1b}[?1;2c".to_string()]);
    }

    #[test]
    fn answers_a_primary_device_attribute_query_split_across_pty_chunks() {
        let appearance = light_appearance();
        let first = consume_terminal_theme_queries(b"", b"\x1b[0", &appearance, true);
        let mut next = appearance.clone();
        next.mode_enabled = first.mode_enabled;
        let mut pending = first.pending.clone();
        pending.push(b'c');
        let second = consume_terminal_theme_queries(&[], &pending, &next, true);
        assert_eq!(first.pending, b"\x1b[0".to_vec());
        assert_eq!(second.responses, vec!["\u{1b}[?1;2c".to_string()]);
    }

    #[test]
    fn answers_kitty_keyboard_protocol_queries_with_flags_zero_so_clients_fall_back() {
        let result = consume("", "\u{1b}[?u\u{1b}[?>u", &light_appearance());
        assert_eq!(
            result.responses,
            vec!["\u{1b}[?0u".to_string(), "\u{1b}[?>0;0u".to_string()]
        );
        assert_eq!(result.pending, Vec::<u8>::new());
    }

    #[test]
    fn capability_query_reports_disabled_before_mode_set() {
        let result = consume("", "\u{1b}[?2031$p", &light_appearance());
        assert_eq!(result.responses, vec!["\u{1b}[?2031;2$y".to_string()]);
        assert!(!result.mode_enabled);
    }

    #[test]
    fn skips_fast_path_when_no_escape_byte_is_present() {
        let result = consume("", "plain output", &light_appearance());
        assert_eq!(result.pending, Vec::<u8>::new());
        assert!(result.responses.is_empty());
        assert!(!result.mode_enabled);
    }

    #[test]
    fn parses_rgb_function_and_three_digit_hex_colors() {
        let mut appearance = light_appearance();
        appearance.terminal_background = Some("rgb(10, 300, 42)".to_string());
        let report = color_report(11, appearance.terminal_background.as_deref()).unwrap();
        assert_eq!(report, "\u{1b}]11;rgb:0a0a/ffff/2a2a\u{1b}\\");
        assert_eq!(
            color_report(10, Some("#abc")).unwrap(),
            "\u{1b}]10;rgb:aaaa/bbbb/cccc\u{1b}\\"
        );
        assert!(color_report(10, Some("not-a-color")).is_none());
        assert!(color_report(10, None).is_none());
    }

    #[test]
    fn theme_mode_report_follows_the_light_dark_split() {
        assert_eq!(terminal_theme_mode_report("light"), "\u{1b}[?997;2n");
        assert_eq!(terminal_theme_mode_report("dark"), "\u{1b}[?997;1n");
        // Anything else reports dark, like the JS ternary.
        assert_eq!(terminal_theme_mode_report("solarized"), "\u{1b}[?997;1n");
    }
}
