//! clack-compatible human output rendering (bin/cli-output.js adapter).
//!
//! @clack/prompts 1.7 glyphs: `┌`/`│`/`└` frame, `◆` success, `▲` warn,
//! `■` error, `●` info. ANSI gray on the frame/symbols when stdout is a TTY
//! (matching styleText("gray", …)); plain bytes otherwise — captured output
//! is what the parity harness diffs.

const S_BAR_START: &str = "\u{250C}"; // ┌
const S_BAR: &str = "\u{2502}"; // │
const S_BAR_END: &str = "\u{2514}"; // └
const S_SUCCESS: &str = "\u{25C6}"; // ◆
const S_WARN: &str = "\u{25B2}"; // ▲
const S_ERROR: &str = "\u{25A0}"; // ■
const S_INFO: &str = "\u{25CF}"; // ●

const GRAY: &str = "\x1b[90m";
const RESET: &str = "\x1b[0m";

pub fn tty_enabled() -> bool {
    tty()
}

fn tty() -> bool {
    // SAFETY: read-only isatty check.
    unsafe { libc_isatty() }
}

#[cfg(unix)]
unsafe fn libc_isatty() -> bool {
    // Avoid a libc dep: /dev/tty reachability is a close proxy, but clack
    // checks fd 1 directly. Use the TTY env contract instead — clack's own
    // isTTY on process.stdout.
    std::env::var("TERM").map(|t| t != "dumb").unwrap_or(false)
        && !std::env::var("CI")
            .map(|v| v == "1" || v == "true")
            .unwrap_or(false)
}

#[cfg(not(unix))]
unsafe fn libc_isatty() -> bool {
    false
}

fn gray(text: &str) -> String {
    if tty() {
        format!("{GRAY}{text}{RESET}")
    } else {
        text.to_string()
    }
}

/// `intro(title)`: `┌  title\n`
pub fn intro(title: &str) {
    println!("{}  {title}", gray(S_BAR_START));
}

/// intro rendered to a String (for output-collecting call sites).
pub fn intro_line(title: &str) -> String {
    format!("{}  {title}\n", gray(S_BAR_START))
}

/// info line rendered to a String.
pub fn info_line(message: &str) -> String {
    format!("{}\n{}  {message}\n", gray(S_BAR), gray(S_INFO))
}

/// outro rendered to a String.
pub fn outro_line(message: &str) -> String {
    format!("{}\n{}  {message}\n\n", gray(S_BAR), gray(S_BAR_END))
}

/// Bare bar prefix for detail lines: `│  `.
pub fn bar_line() -> String {
    format!("{}  ", gray(S_BAR))
}

/// success/warn/error blocks rendered to a String.
pub fn success_line(message: &str) -> String {
    format!("{}\n{}  {message}", gray(S_BAR), gray(S_SUCCESS))
}
pub fn warn_line(message: &str) -> String {
    format!("{}\n{}  {message}", gray(S_BAR), gray(S_WARN))
}
pub fn error_line(message: &str) -> String {
    format!("{}\n{}  {message}", gray(S_BAR), gray(S_ERROR))
}

/// `outro(message)`: `│\n└  message\n\n`
pub fn outro(message: &str) {
    println!("{}\n{}  {message}\n", gray(S_BAR), gray(S_BAR_END));
}

/// Multi-line clack log messages: continuation lines carry the bar prefix
/// (`│  detail`) — clack's `wrapTextWithPrefix` behavior for embedded \n.
fn render_block(glyph: &str, message: &str) {
    // clack renders the first line with the glyph; embedded \n continuation
    // lines (logStatus detail) get the `│  ` bar prefix.
    let mut lines = message.split('\n');
    if let Some(first) = lines.next() {
        println!("{}\n{}  {first}", gray(S_BAR), gray(glyph));
    }
    for line in lines {
        println!("{}  {line}", gray(S_BAR));
    }
}

/// `log.success/message`: `│\n◆  message\n`
pub fn success(message: &str) {
    render_block(S_SUCCESS, message);
}

/// `log.warn`: `│\n▲  message\n`
pub fn warn(message: &str) {
    render_block(S_WARN, message);
}

/// `log.error`: `│\n■  message\n`
pub fn error(message: &str) {
    render_block(S_ERROR, message);
}

/// `log.info` (and neutral): `│\n●  message\n`
pub fn info(message: &str) {
    render_block(S_INFO, message);
}

/// `logStatus(status, message, detail)`: detail appends as a second line in
/// the same block (clack renders the \n inside the message).
pub fn log_status(status: &str, message: &str, detail: Option<&str>) {
    let full = match detail {
        Some(detail) => format!("{message}\n{detail}"),
        None => message.to_string(),
    };
    match status {
        "success" => success(&full),
        "warning" => warn(&full),
        "error" => error(&full),
        _ => info(&full),
    }
}

/// `cancel(message)`: `│\n└  message` without the leading bar newline.
pub fn cancel(message: &str) {
    println!("{}\n{}  {message}", gray(S_BAR), gray(S_BAR_END));
}

#[cfg(test)]
mod tests {
    #[test]
    fn glyphs_match_clack() {
        assert_eq!(super::S_BAR_START, "┌");
        assert_eq!(super::S_BAR, "│");
        assert_eq!(super::S_BAR_END, "└");
        assert_eq!(super::S_SUCCESS, "◆");
        assert_eq!(super::S_WARN, "▲");
        assert_eq!(super::S_ERROR, "■");
        assert_eq!(super::S_INFO, "●");
    }
}
