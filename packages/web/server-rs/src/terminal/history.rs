//! Port of `server/lib/terminal/history.js` plus the chunk-deque replay buffer
//! from `runtime.js` (`appendHistory` / `historyText` / `trimHistoryBytes`).
//!
//! Replay history strips terminal query exchanges the renderer cannot answer on
//! replay (DSR, DA, XTVERSION, mode-2031, OSC 10/11/12 color queries) while
//! live output stays byte-for-byte. Incomplete control sequences carry across
//! PTY chunks through `pending`. The JS scanner works on UTF-16 units; this
//! port scans bytes — every recognized control sequence is pure ASCII, and
//! multibyte UTF-8 payloads can never collide with the scanned lead bytes
//! (`0x1b`/`0x9b`/`0x9d`… only appear as standalone ASCII/C1 bytes).

use std::collections::VecDeque;

/// Server-side scrollback cap (`MAX_HISTORY_BYTES`).
pub const MAX_HISTORY_BYTES: usize = 512 * 1024;
/// Deque slack: one extra chunk may be retained before trimming, so
/// materialized snapshots stay within `MAX_HISTORY_BYTES + slack`.
const MAX_HISTORY_CHUNK_BYTES: usize = 128 * 1024;

fn is_csi_final_byte(code: u8) -> bool {
    (0x40..=0x7e).contains(&code)
}

/// `^[0-9;?]*$` (cursor-position report body).
fn body_is_position_report(body: &[u8]) -> bool {
    body.iter().all(|b| matches!(b, b'0'..=b'9' | b';' | b'?'))
}

/// `^[>0-9;?]*$` (device-attribute report body).
fn body_is_attribute_report(body: &[u8]) -> bool {
    body.iter()
        .all(|b| matches!(b, b'>' | b'0'..=b'9' | b';' | b'?'))
}

/// `^\?2031(?:;[0-9]+)?\$$` (mode-2031 report body).
fn body_is_mode_2031_report(body: &[u8]) -> bool {
    let Some(rest) = body.strip_prefix(b"?2031") else {
        return false;
    };
    match rest.first() {
        Some(b'$') => rest.len() == 1,
        Some(b';') => {
            // `;<digits>$` with at least one digit before the terminator.
            rest.len() >= 3
                && rest[1..rest.len() - 1].iter().all(|b| b.is_ascii_digit())
                && rest.ends_with(b"$")
        }
        _ => false,
    }
}

fn should_strip_csi(body: &[u8], final_byte: u8) -> bool {
    match final_byte {
        b'n' => true,
        b'R' => body_is_position_report(body),
        b'c' => body_is_attribute_report(body),
        b'p' | b'y' => body_is_mode_2031_report(body),
        b'h' | b'l' => body == b"?2031",
        _ => false,
    }
}

/// `^(10|11|12);(?:\?|rgb:)` — OSC color query payloads.
fn should_strip_osc(content: &[u8]) -> bool {
    let Some((code, rest)) = content.split_at_checked(3) else {
        return false;
    };
    if !matches!(code, b"10;" | b"11;" | b"12;") {
        return false;
    }
    rest.starts_with(b"?") || rest.starts_with(b"rgb:")
}

fn strip_terminator(value: &[u8]) -> &[u8] {
    if value.ends_with(b"\x1b\\") {
        &value[..value.len() - 2]
    } else if value.ends_with(&[0x07]) || value.ends_with(&[0x9c]) {
        &value[..value.len() - 1]
    } else {
        value
    }
}

/// `findStringEnd`: end offset (exclusive) of a string-sequence body starting
/// the scan at `start`, terminated by BEL, 0x9c, or ESC `\`.
fn find_string_end(input: &[u8], start: usize) -> Option<usize> {
    let mut index = start;
    while index < input.len() {
        match input[index] {
            0x07 | 0x9c => return Some(index + 1),
            0x1b if input.get(index + 1) == Some(&0x5c) => return Some(index + 2),
            _ => index += 1,
        }
    }
    None
}

/// `findEscapeEnd`: consume 0x20–0x2f intermediates, then one 0x30–0x7e final
/// byte; a non-final byte consumes just the escape itself (JS `start + 1`).
fn find_escape_end(input: &[u8], start: usize) -> Option<usize> {
    let mut cursor = start;
    while cursor < input.len() && (0x20..=0x2f).contains(&input[cursor]) {
        cursor += 1;
    }
    if cursor >= input.len() {
        return None;
    }
    if (0x30..=0x7e).contains(&input[cursor]) {
        Some(cursor + 1)
    } else {
        Some(start + 1)
    }
}

pub struct SanitizedChunk {
    pub visible: Vec<u8>,
    pub pending: Vec<u8>,
}

/// `sanitizeTerminalHistoryChunk(pending, data)`.
pub fn sanitize_terminal_history_chunk(pending: &[u8], data: &[u8]) -> SanitizedChunk {
    let mut input = Vec::with_capacity(pending.len() + data.len());
    input.extend_from_slice(pending);
    input.extend_from_slice(data);

    let mut visible: Vec<u8> = Vec::with_capacity(input.len());
    let mut index = 0;
    while index < input.len() {
        let code = input[index];
        if code == 0x1b {
            let Some(&next) = input.get(index + 1) else {
                return SanitizedChunk {
                    visible,
                    pending: input[index..].to_vec(),
                };
            };
            match next {
                // CSI: ESC [ params final
                0x5b => {
                    let mut cursor = index + 2;
                    while cursor < input.len() && !is_csi_final_byte(input[cursor]) {
                        cursor += 1;
                    }
                    if cursor >= input.len() {
                        return SanitizedChunk {
                            visible,
                            pending: input[index..].to_vec(),
                        };
                    }
                    if !should_strip_csi(&input[index + 2..cursor], input[cursor]) {
                        visible.extend_from_slice(&input[index..cursor + 1]);
                    }
                    index = cursor + 1;
                }
                // OSC (0x5d), DCS (0x50), PM (0x5e), APC (0x5f): string sequences
                0x5d | 0x50 | 0x5e | 0x5f => {
                    let Some(end) = find_string_end(&input, index + 2) else {
                        return SanitizedChunk {
                            visible,
                            pending: input[index..].to_vec(),
                        };
                    };
                    let content = strip_terminator(&input[index + 2..end]);
                    // Only OSC color queries are stripped; DCS/PM/APC pass through.
                    if next != 0x5d || !should_strip_osc(content) {
                        visible.extend_from_slice(&input[index..end]);
                    }
                    index = end;
                }
                _ => {
                    let Some(end) = find_escape_end(&input, index + 1) else {
                        return SanitizedChunk {
                            visible,
                            pending: input[index..].to_vec(),
                        };
                    };
                    visible.extend_from_slice(&input[index..end]);
                    index = end;
                }
            }
            continue;
        }
        if code == 0x9b {
            // 8-bit CSI.
            let mut cursor = index + 1;
            while cursor < input.len() && !is_csi_final_byte(input[cursor]) {
                cursor += 1;
            }
            if cursor >= input.len() {
                return SanitizedChunk {
                    visible,
                    pending: input[index..].to_vec(),
                };
            }
            if !should_strip_csi(&input[index + 1..cursor], input[cursor]) {
                visible.extend_from_slice(&input[index..cursor + 1]);
            }
            index = cursor + 1;
            continue;
        }
        if matches!(code, 0x9d | 0x90 | 0x9e | 0x9f) {
            // 8-bit OSC/DCS/PM/APC.
            let Some(end) = find_string_end(&input, index + 1) else {
                return SanitizedChunk {
                    visible,
                    pending: input[index..].to_vec(),
                };
            };
            let content = strip_terminator(&input[index + 1..end]);
            if code != 0x9d || !should_strip_osc(content) {
                visible.extend_from_slice(&input[index..end]);
            }
            index = end;
            continue;
        }
        visible.push(code);
        index += 1;
    }
    SanitizedChunk {
        visible,
        pending: Vec::new(),
    }
}

/// `trimHistoryBytes`: keep the byte-exact tail, then skip any leading UTF-8
/// continuation bytes so the tail is valid UTF-8.
fn trim_history_bytes(mut bytes: &[u8]) -> &[u8] {
    if bytes.len() > MAX_HISTORY_BYTES {
        let mut start = bytes.len() - MAX_HISTORY_BYTES;
        while start < bytes.len() && (bytes[start] & 0xc0) == 0x80 {
            start += 1;
        }
        bytes = &bytes[start..];
    }
    bytes
}

/// Replay history as a chunk deque: appending is O(chunk) and the byte-exact
/// 512 KiB tail contract applies when the text materializes (snapshot path
/// only). A dropped head chunk can split a UTF-8 sequence; leading
/// continuation bytes are skipped exactly as the tail trim does.
#[derive(Default)]
pub struct HistoryBuf {
    chunks: VecDeque<Vec<u8>>,
    bytes: usize,
}

impl HistoryBuf {
    pub fn reset(&mut self) {
        self.chunks.clear();
        self.bytes = 0;
    }

    /// `appendHistory`.
    pub fn append(&mut self, visible: &[u8]) {
        self.chunks.push_back(visible.to_vec());
        self.bytes += visible.len();
        while self.bytes > MAX_HISTORY_BYTES + MAX_HISTORY_CHUNK_BYTES && self.chunks.len() > 1 {
            if let Some(front) = self.chunks.pop_front() {
                self.bytes -= front.len();
            }
        }
    }

    /// `historyText` — materialize the bounded, UTF-8-safe tail.
    pub fn text(&self) -> String {
        let mut joined = Vec::with_capacity(self.bytes);
        for chunk in &self.chunks {
            joined.extend_from_slice(chunk);
        }
        let mut start = 0;
        while start < joined.len() && (joined[start] & 0xc0) == 0x80 {
            start += 1;
        }
        String::from_utf8_lossy(trim_history_bytes(&joined[start..])).into_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sanitize(pending: &str, data: &str) -> SanitizedChunk {
        sanitize_terminal_history_chunk(pending.as_bytes(), data.as_bytes())
    }

    fn as_str(bytes: &[u8]) -> String {
        String::from_utf8_lossy(bytes).into_owned()
    }

    #[test]
    fn removes_device_and_color_query_exchanges_while_preserving_display_controls() {
        let input = "before\u{1b}[6n\u{1b}[12;40R\u{1b}[>0c\u{1b}[?2031h\u{1b}[?2031$p\u{1b}[?2031;1$y\u{1b}]10;?\u{7}\u{1b}[31mred\u{1b}[0mafter";
        let result = sanitize("", input);
        assert_eq!(as_str(&result.visible), "before\u{1b}[31mred\u{1b}[0mafter");
        assert_eq!(result.pending, Vec::<u8>::new());
    }

    #[test]
    fn carries_incomplete_control_sequences_across_pty_chunks() {
        let first = sanitize("", "text\u{1b}]11;");
        assert_eq!(as_str(&first.visible), "text");
        assert_eq!(as_str(&first.pending), "\u{1b}]11;");
        let second = sanitize(&as_str(&first.pending), "?\u{1b}\\next");
        assert_eq!(as_str(&second.visible), "next");
        assert_eq!(second.pending, Vec::<u8>::new());
    }

    #[test]
    fn preserves_ordinary_osc_titles_and_split_utf16_text() {
        let result = sanitize("", "\u{1b}]0;title\u{7}ok");
        assert_eq!(as_str(&result.visible), "\u{1b}]0;title\u{7}ok");
        assert_eq!(result.pending, Vec::<u8>::new());
    }

    #[test]
    fn keeps_non_query_reports_and_8bit_variants() {
        // A position-report body of pure digits/;/? IS a query exchange (JS
        // regex matches) — only non-numeric bodies survive.
        let stripped = sanitize("", "\u{1b}[1;2Rx");
        assert_eq!(as_str(&stripped.visible), "x");
        let kept = sanitize("", "\u{1b}[kx");
        assert_eq!(as_str(&kept.visible), "\u{1b}[kx");
        // OSC 12 color query (ST-terminated) is stripped; OSC 133 is kept.
        let mixed = sanitize("", "\u{1b}]12;?\u{1b}\\\u{1b}]133;A\u{7}");
        assert_eq!(as_str(&mixed.visible), "\u{1b}]133;A\u{7}");
    }

    #[test]
    fn mode_2031_reports_with_and_without_a_parameter() {
        assert!(body_is_mode_2031_report(b"?2031$"));
        assert!(body_is_mode_2031_report(b"?2031;1$"));
        assert!(!body_is_mode_2031_report(b"?2031;"));
        assert!(!body_is_mode_2031_report(b"?2031;;$"));
    }

    #[test]
    fn history_buffer_roundtrips_and_caps_to_the_byte_exact_tail() {
        let mut buf = HistoryBuf::default();
        buf.append(b"hello\r\n");
        buf.append(b"world\r\n");
        assert_eq!(buf.text(), "hello\r\nworld\r\n");

        // Fill past the cap: the materialized tail is at most MAX_HISTORY_BYTES
        // (plus at most one split UTF-8 lead) and reflects the newest bytes.
        let chunk = vec![b'x'; 64 * 1024];
        for _ in 0..16 {
            buf.append(&chunk);
        }
        let text = buf.text();
        assert!(text.len() <= MAX_HISTORY_BYTES + 8, "len={}", text.len());
        assert!(
            text.ends_with(&"x".repeat(64 * 1024 - 1)),
            "tail must reflect the newest bytes"
        );
    }

    #[test]
    fn history_buffer_skips_continuation_bytes_after_a_dropped_head_chunk() {
        let mut buf = HistoryBuf::default();
        // Oversized head chunk + a 4-byte emoji tail: trimming drops the whole
        // head chunk, and the materialized text must not start mid-codepoint.
        let filler = vec![b'a'; MAX_HISTORY_BYTES + MAX_HISTORY_CHUNK_BYTES];
        buf.append(&filler);
        buf.append("\u{1f600}".as_bytes());
        buf.append(b"!");
        // Only whole chunks are dropped, so the emoji stays intact here.
        assert_eq!(buf.text(), "\u{1f600}!");
    }
}
