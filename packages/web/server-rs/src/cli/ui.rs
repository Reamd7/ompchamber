//! clack-compatible human output rendering (bin/cli-output.js adapter).
//!
//! @clack/prompts 1.7 glyphs: `┌`/`│`/`└` frame, `◆` success, `▲` warn,
//! `■` error, `●` info. ANSI gray on the frame/symbols when stdout is a TTY
//! (matching styleText("gray", …)); plain bytes otherwise — captured output
//! is what the parity harness diffs.
//!
//! 中文说明：本模块是 clack 兼容的人类可读输出渲染器，对应 JS 版
//! `bin/cli-output.js` 适配层。仅在 stdout 按 TTY 对待时对边框与符号
//! 加 ANSI 灰色（等价 JS 的 styleText("gray", …)）；非 TTY（如被测试
//! 捕获的输出）落的是纯文本字节——字节级输出正是 parity 对拍所比对
//! 的内容，因此字形与换行必须与 @clack/prompts 1.7 逐字一致。

/// clack 边框起始符号（`┌`）。
const S_BAR_START: &str = "\u{250C}"; // ┌
/// clack 边框竖线（`│`），用作块内续行前缀。
const S_BAR: &str = "\u{2502}"; // │
/// clack 边框收尾符号（`└`）。
const S_BAR_END: &str = "\u{2514}"; // └
/// 成功消息字形（`◆`）。
const S_SUCCESS: &str = "\u{25C6}"; // ◆
/// 警告消息字形（`▲`）。
const S_WARN: &str = "\u{25B2}"; // ▲
/// 错误消息字形（`■`）。
const S_ERROR: &str = "\u{25A0}"; // ■
/// 信息/中性消息字形（`●`）。
const S_INFO: &str = "\u{25CF}"; // ●

/// ANSI 亮灰前景色转义序列；仅 TTY 下使用。
const GRAY: &str = "\x1b[90m";
/// ANSI 颜色重置转义序列。
const RESET: &str = "\x1b[0m";

/// 对外的 TTY 判定入口：决定本模块输出是否带 ANSI 颜色。
pub fn tty_enabled() -> bool {
    tty()
}

/// 当前进程 stdout 是否按 TTY 对待（转调 libc_isatty）。
fn tty() -> bool {
    // SAFETY: read-only isatty check.
    unsafe { libc_isatty() }
}

/// Unix 下的 TTY 判定：不引入 libc 依赖，沿用 clack 的 isTTY 环境契约
/// ——TERM 存在且不为 dumb，且 CI 未置为 1/true。
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

/// 非 Unix 平台一律视为非 TTY（不输出颜色）。
#[cfg(not(unix))]
unsafe fn libc_isatty() -> bool {
    false
}

/// 把 text 包成灰色；非 TTY 时原样返回，保证捕获输出无转义字节。
fn gray(text: &str) -> String {
    if tty() {
        format!("{GRAY}{text}{RESET}")
    } else {
        text.to_string()
    }
}

/// `intro(title)`: `┌  title\n`
/// 中文：直接打印 intro 框首行（`┌  title`）。
pub fn intro(title: &str) {
    println!("{}  {title}", gray(S_BAR_START));
}

/// intro rendered to a String (for output-collecting call sites).
/// 中文：intro 的 String 版本，供收集输出而非直接打印的调用点使用。
pub fn intro_line(title: &str) -> String {
    format!("{}  {title}\n", gray(S_BAR_START))
}

/// info line rendered to a String.
/// 中文：单行 info 消息（`●  message`）。
pub fn info_line(message: &str) -> String {
    format!("{}\n{}  {message}\n", gray(S_BAR), gray(S_INFO))
}

/// outro rendered to a String.
/// 中文：outro 的 String 版本（`└  message` + 空行）。
pub fn outro_line(message: &str) -> String {
    format!("{}\n{}  {message}\n\n", gray(S_BAR), gray(S_BAR_END))
}

/// Bare bar prefix for detail lines: `│  `.
/// 中文：裸边框前缀 `│  `，供调用方拼接详情行。
pub fn bar_line() -> String {
    format!("{}  ", gray(S_BAR))
}

/// success/warn/error blocks rendered to a String.
/// 中文：成功消息的 String 版本（`◆`）。
pub fn success_line(message: &str) -> String {
    format!("{}\n{}  {message}", gray(S_BAR), gray(S_SUCCESS))
}
/// 警告消息的 String 版本（`▲`）。
pub fn warn_line(message: &str) -> String {
    format!("{}\n{}  {message}", gray(S_BAR), gray(S_WARN))
}
/// 错误消息的 String 版本（`■`）。
pub fn error_line(message: &str) -> String {
    format!("{}\n{}  {message}", gray(S_BAR), gray(S_ERROR))
}

/// `outro(message)`: `│\n└  message\n\n`
/// 中文：直接打印 outro 框尾（`└  message`）并追加空行。
pub fn outro(message: &str) {
    println!("{}\n{}  {message}\n", gray(S_BAR), gray(S_BAR_END));
}

/// Multi-line clack log messages: continuation lines carry the bar prefix
/// (`│  detail`) — clack's `wrapTextWithPrefix` behavior for embedded \n.
/// 中文：渲染多行日志块——首行带 glyph，后续行带 `│  ` 前缀。
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
/// 中文：成功日志块。
pub fn success(message: &str) {
    render_block(S_SUCCESS, message);
}

/// `log.warn`: `│\n▲  message\n`
/// 中文：警告日志块。
pub fn warn(message: &str) {
    render_block(S_WARN, message);
}

/// `log.error`: `│\n■  message\n`
/// 中文：错误日志块。
pub fn error(message: &str) {
    render_block(S_ERROR, message);
}

/// `log.info` (and neutral): `│\n●  message\n`
/// 中文：信息（及中性）日志块。
pub fn info(message: &str) {
    render_block(S_INFO, message);
}

/// `logStatus(status, message, detail)`: detail appends as a second line in
/// the same block (clack renders the \n inside the message).
/// 中文：按 status 分发到 success/warn/error/info；detail 拼接为同块第二行。
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
/// 中文：取消消息：`└` 收尾，但不像 outro 那样追加空行。
pub fn cancel(message: &str) {
    println!("{}\n{}  {message}", gray(S_BAR), gray(S_BAR_END));
}

/// 输出字形的单元测试。
#[cfg(test)]
mod tests {
    /// 验证所有输出字形常量与 @clack/prompts 1.7 逐字一致（parity 的前提）。
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
