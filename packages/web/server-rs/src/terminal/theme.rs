//! Port of `server/lib/terminal/theme-response.js`.
//!
//! The PTY itself answers theme/capability queries emitted by shells and TUIs —
//! including queries that arrive before any WebSocket attachment exists — so
//! startup handshakes (Fish's DA1 probe, OpenTUI's mode-2031/OSC 10/11 cycle,
//! kitty keyboard probes) never block on a renderer. Byte-level port: every
//! recognized sequence is ASCII.
//!
//! 中文说明：本模块在 PTY 侧直接应答 shell/TUI 发出的主题与能力查询，
//! 让启动握手（Fish 的 DA1 探测、OpenTUI 的 mode-2031 与 OSC 10/11 循环、
//! kitty keyboard 探测）不依赖任何浏览器渲染器。核心入口是
//! `consume_terminal_theme_queries`：把 pending 与新数据拼接后逐字节扫描，
//! 识别到的查询按当前 `Appearance` 生成应答串，输入尾部可能跨 chunk 的
//! 半截序列作为新的 pending 返回。所有被识别的序列均为 ASCII，多字节
//! UTF-8 输出不会与扫描的引导字节碰撞。

/// mode-2031 置位序列（DECSM）：开启主题同步开关，更新 `mode_enabled`。
const MODE_SET: &[u8] = b"\x1b[?2031h";
/// mode-2031 复位序列（DECRM）：关闭主题同步开关。
const MODE_RESET: &[u8] = b"\x1b[?2031l";
/// mode-2031 能力查询（DECRQM 的 `$p` 形式）：按当前开关状态回
/// 1（已置位）或 2（已复位），对应 DECRPM 应答。
const CAPABILITY_QUERY: &[u8] = b"\x1b[?2031$p";
/// XTWINMODE 996/997 主题模式查询：统一以 `terminal_theme_mode_report`
/// 应答（light=2、dark=1）。
const MODE_QUERIES: [&[u8]; 2] = [b"\x1b[?996n", b"\x1b[?997n"];
/// Fish asks this before an unattached browser terminal can reply.
const PRIMARY_DEVICE_ATTRIBUTE_QUERIES: [&[u8]; 2] = [b"\x1b[c", b"\x1b[0c"];
/// DA1 应答：保守的 VT100 能力串（`?1;2c`），只宣告基础光标控制，
/// 避免无浏览器终端 attach 时 Fish 卡在查询超时上。
const PRIMARY_DEVICE_ATTRIBUTE_RESPONSE: &str = "\x1b[?1;2c";
/// Kitty keyboard protocol queries: answer flags=0 so clients fall back at once.
const KITTY_PRIMARY_QUERY: &[u8] = b"\x1b[?u";
/// kitty primary 查询应答：flags=0（不支持任何增强标志），客户端应
/// 立即回退到传统键盘协议。
const KITTY_PRIMARY_RESPONSE: &str = "\x1b[?0u";
/// kitty secondary 键盘协议查询（`CSI ?> u`），用于探测实现版本与标志。
const KITTY_SECONDARY_QUERY: &[u8] = b"\x1b[?>u";
/// kitty secondary 查询应答：版本与 flags 均为 0。
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
    /// 当前 UI 主题模式（`"light"` / `"dark"`，其余取值一律按 dark 报告）。
    pub theme_mode: String,
    /// 终端背景色（`#rgb`/`#rrggbb`/`rgb()` 形式），OSC 11 查询的应答来源；
    /// `None` 或无法解析时不作答。
    pub terminal_background: Option<String>,
    /// 终端前景色，OSC 10 查询的应答来源；`None` 或无法解析时不作答。
    pub terminal_foreground: Option<String>,
    /// mode-2031 开关的当前状态，由消费循环中的置位/复位序列推进，
    /// 调用方需把结果回填进下一轮的 `Appearance`。
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

/// `consumeTerminalThemeQueries` 的消费结果：应答串列表、跨 chunk 携带
/// 的尾部半截序列，以及推进后的 mode-2031 开关状态。
pub struct ThemeQueriesConsumed {
    /// 输入尾部中作为某个受识别序列真前缀的最长后缀，留待与下一个
    /// PTY chunk 拼接后重新扫描。
    pub pending: Vec<u8>,
    /// 按线上出现顺序生成的应答串（自带完整转义序列，可直接写回 PTY）。
    pub responses: Vec<String>,
    /// 消费完本段输入后的 mode-2031 开关状态。
    pub mode_enabled: bool,
}

/// 边界安全的前缀比较：`input` 自 `index` 起是否恰以 `sequence` 开头。
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

/// 主题查询应答的单元测试：完整握手、跨 chunk 拼接与逐条应答顺序。
#[cfg(test)]
mod tests {
    use super::*;

    /// 构造 light 模式的测试外观：深色前景、浅色背景、mode-2031 关闭。
    fn light_appearance() -> Appearance {
        Appearance {
            theme_mode: "light".to_string(),
            terminal_foreground: Some("#1b1b1b".to_string()),
            terminal_background: Some("#faf8f0".to_string()),
            mode_enabled: false,
        }
    }

    /// 测试便捷封装：关闭 DA1 fallback（模拟已有浏览器终端在监听），
    /// 消费一段字符串输入。
    fn consume(pending: &str, data: &str, appearance: &Appearance) -> ThemeQueriesConsumed {
        consume_terminal_theme_queries(pending.as_bytes(), data.as_bytes(), appearance, false)
    }

    /// 无行为的占位函数，仅保持与上游 JS 测试文件的结构对应。
    #[allow(dead_code)]
    fn unused_marker() {}

    /// 验证：OpenTUI 启动握手（mode-2031 置位 + OSC 10/11 + 能力查询）
    /// 逐项作答，且不残留 pending。
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

    /// 验证：OSC 11 查询拆成多个 PTY chunk 时只应答一次，拼齐前的
    /// 字节不会在后续轮次被重复消费。
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

    /// 验证：同一 chunk 内重复出现的查询各自作答，顺序与线上顺序一致。
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

    /// 验证：DA1 查询仅在 fallback 开启（无浏览器终端 attach）时作答。
    #[test]
    fn answers_a_primary_device_attribute_query_only_when_the_fallback_is_enabled() {
        let appearance = light_appearance();
        let attached = consume_terminal_theme_queries(b"", b"\x1b[0c", &appearance, false);
        let unattached = consume_terminal_theme_queries(b"", b"\x1b[0c", &appearance, true);
        assert!(attached.responses.is_empty());
        assert_eq!(unattached.responses, vec!["\u{1b}[?1;2c".to_string()]);
    }

    /// 验证：拆在两个 chunk 的 DA1 查询先挂起为 pending，补齐最后
    /// 一个字节后才作答。
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

    /// 验证：kitty primary/secondary 查询都以 flags=0 应答，促使客户端
    /// 立即回退。
    #[test]
    fn answers_kitty_keyboard_protocol_queries_with_flags_zero_so_clients_fall_back() {
        let result = consume("", "\u{1b}[?u\u{1b}[?>u", &light_appearance());
        assert_eq!(
            result.responses,
            vec!["\u{1b}[?0u".to_string(), "\u{1b}[?>0;0u".to_string()]
        );
        assert_eq!(result.pending, Vec::<u8>::new());
    }

    /// 验证：mode-2031 置位之前的能力查询按复位状态（2）报告。
    #[test]
    fn capability_query_reports_disabled_before_mode_set() {
        let result = consume("", "\u{1b}[?2031$p", &light_appearance());
        assert_eq!(result.responses, vec!["\u{1b}[?2031;2$y".to_string()]);
        assert!(!result.mode_enabled);
    }

    /// 验证：不含 ESC 字节的普通输出走快速路径，直接返回空结果。
    #[test]
    fn skips_fast_path_when_no_escape_byte_is_present() {
        let result = consume("", "plain output", &light_appearance());
        assert_eq!(result.pending, Vec::<u8>::new());
        assert!(result.responses.is_empty());
        assert!(!result.mode_enabled);
    }

    /// 验证：`rgb()` 通道截断到 255、三位 hex 按 17 倍展开、非法或
    /// 缺失的颜色输入返回 `None`。
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

    /// 验证：主题模式报告 light=2、dark=1，未知取值一律按 dark 处理。
    #[test]
    fn theme_mode_report_follows_the_light_dark_split() {
        assert_eq!(terminal_theme_mode_report("light"), "\u{1b}[?997;2n");
        assert_eq!(terminal_theme_mode_report("dark"), "\u{1b}[?997;1n");
        // Anything else reports dark, like the JS ternary.
        assert_eq!(terminal_theme_mode_report("solarized"), "\u{1b}[?997;1n");
    }
}
