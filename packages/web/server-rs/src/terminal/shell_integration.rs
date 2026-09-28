//! Port of `server/lib/terminal/shell-integration.js`.
//!
//! OSC 133 shell integration: injected zsh/bash wrappers emit command boundary
//! markers in PTY output so the runtime can publish `command-finished` events
//! (unread badges and other OSC 133 consumers). The scanner is chunk-boundary
//! safe: incomplete sequences carry across chunks, BEL and ST terminators are
//! both accepted, and the carry is bounded so a stream of unmatched bytes
//! cannot grow it without limit.
//!
//! The JS scanner drives a regex (`/\x1b\]133;([CD])(?:;(\d+))?(?:\x07|\x1b\\)/g`)
//! with remove-and-restart semantics; this port hand-rolls the same state
//! machine byte-wise (no regex crate in this workspace).
//!
//! 中文说明：`Osc133Scanner` 逐 chunk 消费 PTY 输出，提取 `ESC ] 133 ; C`
//! 与 `ESC ] 133 ; D ; <exit>` 标记并产出事件，供运行时发布
//! `command-finished`（未读角标等 OSC 133 消费方使用）。匹配即从 carry
//! 中移除并回到同一偏移继续，等价于 JS 正则的 remove-and-restart；
//! 不完整序列留在 carry 等下一 chunk，carry 超过 `MAX_CARRY` 即从头部
//! 截断，防止无匹配字节无限累积。

/// carry 缓冲上限（字节）：超过即从头截断，只保留最近的待匹配尾部。
const MAX_CARRY: usize = 4096;

/// 从 PTY 输出中识别到的一条 OSC 133 命令边界事件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Osc133Event {
    /// `OSC 133 ; C`：用户按下回车、命令开始执行（preexec 钩子发出）。
    CommandStarted,
    /// `OSC 133 ; D [; exit]`：命令执行结束（precmd 钩子发出）；
    /// `exit_code` 缺失时为 `None`，溢出/非法数字同样解析为 `None`。
    CommandFinished { exit_code: Option<i32> },
}

/// 有状态的 OSC 133 扫描器：跨 chunk 拼接半截标记，重复 scan 复用
/// 同一 carry 缓冲。
#[derive(Default)]
pub struct Osc133Scanner {
    /// 上一次 scan 遗留的未消费字节，与新 chunk 拼接后重新扫描。
    carry: Vec<u8>,
}

/// 扫描、构造与重置。
impl Osc133Scanner {
    /// 构造空扫描器（`Default` 的显式形式）。
    pub fn new() -> Self {
        Self::default()
    }

    /// Scan a chunk of PTY output for OSC 133 markers.
    pub fn scan(&mut self, chunk: &[u8]) -> Vec<Osc133Event> {
        self.carry.extend_from_slice(chunk);
        let mut events = Vec::new();
        let mut search = 0;
        while let Some(offset) = find_subslice(&self.carry[search..], b"\x1b]133;") {
            let start = search + offset;
            let mut cursor = start + b"\x1b]133;".len();
            let Some(&kind) = self.carry.get(cursor) else {
                // Incomplete marker: wait for the next chunk.
                break;
            };
            if kind != b'C' && kind != b'D' {
                search = start + 1;
                continue;
            }
            cursor += 1;
            // Optional `;<digits>` payload (required to be non-empty when ';'
            // is present, exactly like the JS `(?:;(\d+))?` group).
            let mut exit_code: Option<i32> = None;
            if self.carry.get(cursor) == Some(&b';') {
                let digits_start = cursor + 1;
                let mut digits_end = digits_start;
                while self
                    .carry
                    .get(digits_end)
                    .is_some_and(|b| b.is_ascii_digit())
                {
                    digits_end += 1;
                }
                if digits_end == digits_start {
                    // `;` without digits can never match; try the next ESC.
                    search = start + 1;
                    continue;
                }
                exit_code = std::str::from_utf8(&self.carry[digits_start..digits_end])
                    .ok()
                    .and_then(|digits| digits.parse::<i32>().ok());
                cursor = digits_end;
            }
            // Terminator: BEL or ESC \.
            let end = match self.carry.get(cursor) {
                Some(&0x07) => cursor + 1,
                Some(&0x1b) if self.carry.get(cursor + 1) == Some(&0x5c) => cursor + 2,
                Some(_) => {
                    search = start + 1;
                    continue;
                }
                None => break, // terminator may still arrive
            };
            if kind == b'C' {
                events.push(Osc133Event::CommandStarted);
            } else {
                events.push(Osc133Event::CommandFinished { exit_code });
            }
            self.carry.drain(start..end);
            // Continue from the same offset: the removal shifted later bytes
            // into this position (JS resets lastIndex to 0 after removal).
            search = start;
        }
        if self.carry.len() > MAX_CARRY {
            let drop = self.carry.len() - MAX_CARRY;
            self.carry.drain(..drop);
        }
        events
    }

    /// 清空 carry，丢弃所有未匹配状态（会话重置时使用）。
    pub fn reset(&mut self) {
        self.carry.clear();
    }
}

/// 朴素子串查找：返回 `needle` 在 `haystack` 中首次出现的偏移，
/// 等价于 `str::find` 的字节版（避免引入 regex 依赖）。
fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Build the zsh wrapper script that emits OSC 133 markers. Written to a temp
/// `.zshenv` that hands ZDOTDIR back to the user immediately, then installs
/// precmd/preexec hooks.
pub fn build_zsh_osc133_wrapper(user_zdotdir: &str) -> String {
    format!(
        r##"# OpenChamber shell integration: OSC 133 command boundary markers
# Give ZDOTDIR back to the user immediately (prevents HISTFILE bugs)
export ZDOTDIR="{user_zdotdir}"

# OSC 133 hooks
__oc_osc133_precmd() {{
  local exit_code=$?
  if [[ -n "${{__oc_in_command:-}}" ]]; then
    builtin printf '\x1b]133;D;%s\x07' "$exit_code"
    builtin unset __oc_in_command
  fi
  builtin printf '\x1b]133;A\x07'
}}
__oc_osc133_preexec() {{
  builtin printf '\x1b]133;C\x07'
  builtin typeset -g __oc_in_command=1
}}

# Register hooks (after user's config loads via ZDOTDIR handback)
autoload -Uz add-zsh-hook
add-zsh-hook precmd __oc_osc133_precmd
add-zsh-hook preexec __oc_osc133_preexec

# Ready marker
builtin printf '\x1b]777;oc-shell-ready\x07'
"##
    )
}

/// Build the bash rcfile snippet that emits OSC 133 markers.
pub fn build_bash_osc133_rc(user_bashrc: &str) -> String {
    format!(
        r##"# OpenChamber shell integration: OSC 133 command boundary markers
__oc_osc133_preexec() {{ builtin printf '\x1b]133;C\x07'; builtin typeset -g __oc_in_command=1; }}
__oc_osc133_precmd() {{
  local exit_code=$?
  if [[ -n "${{__oc_in_command:-}}" ]]; then
    builtin printf '\x1b]133;D;%s\x07' "$exit_code"
    builtin unset __oc_in_command
  fi
  builtin printf '\x1b]133;A\x07'
}}
# Append to existing PROMPT_COMMAND (bash >=5.1 uses array)
if [[ -n "${{PROMPT_COMMAND[*]:-}}" ]]; then
  PROMPT_COMMAND+=('__oc_osc133_precmd')
else
  PROMPT_COMMAND='__oc_osc133_precmd'
fi
trap '__oc_osc133_preexec' DEBUG
builtin printf '\x1b]777;oc-shell-ready\x07'
# Source user's bashrc
[[ -f "{user_bashrc}" ]] && source "{user_bashrc}"
"##
    )
}

/// OSC 133 扫描器与 wrapper 脚本生成的单元测试。
#[cfg(test)]
mod tests {
    use super::*;

    /// 验证：同一 chunk 内的 C 与 D;0 标记按顺序产出开始/结束事件。
    #[test]
    fn detects_command_started_and_finished_markers() {
        let mut scanner = Osc133Scanner::new();
        let events = scanner.scan(b"pre\x1b]133;C\x07$\x1b]133;D;0\x07");
        assert_eq!(
            events,
            vec![
                Osc133Event::CommandStarted,
                Osc133Event::CommandFinished { exit_code: Some(0) },
            ]
        );
    }

    /// 验证：BEL 与 ST 两种终止符都接受，D 标记缺省 exit 时为 `None`。
    #[test]
    fn accepts_st_terminators_and_missing_exit_codes() {
        let mut scanner = Osc133Scanner::new();
        let events = scanner.scan(b"\x1b]133;C\x1b\\\x1b]133;D\x07");
        assert_eq!(
            events,
            vec![
                Osc133Event::CommandStarted,
                Osc133Event::CommandFinished { exit_code: None },
            ]
        );
    }

    /// 验证：标记拆散在多个 chunk 时经 carry 拼接，最终仍产出事件。
    #[test]
    fn carries_incomplete_sequences_across_chunks() {
        let mut scanner = Osc133Scanner::new();
        assert!(scanner.scan(b"\x1b]133").is_empty());
        assert!(scanner.scan(b";C").is_empty());
        assert_eq!(scanner.scan(b"\x07"), vec![Osc133Event::CommandStarted]);
        // Split inside the exit digits.
        assert!(scanner.scan(b"\x1b]133;D;4").is_empty());
        assert_eq!(
            scanner.scan(b"2\x07"),
            vec![Osc133Event::CommandFinished {
                exit_code: Some(42)
            }]
        );
    }

    /// 验证：payload 字母不符、分号后无数字的标记与普通 OSC 都被忽略。
    #[test]
    fn ignores_unrelated_and_malformed_markers() {
        let mut scanner = Osc133Scanner::new();
        // Wrong payload letter, semicolon without digits, and an ordinary OSC.
        assert!(
            scanner
                .scan(b"\x1b]133;A\x07\x1b]133;D;\x07\x1b]0;title\x07")
                .is_empty()
        );
        assert_eq!(
            scanner.scan(b"\x1b]133;D;7\x07"),
            vec![Osc133Event::CommandFinished { exit_code: Some(7) }]
        );
    }

    /// 验证：无匹配噪声会把 carry 截到上限之内，且截断后仍能解析
    /// 后续完整标记。
    #[test]
    fn bounds_the_carry_buffer() {
        let mut scanner = Osc133Scanner::new();
        let noise = vec![b'x'; MAX_CARRY * 2];
        assert!(scanner.scan(&noise).is_empty());
        assert!(scanner.carry.len() <= MAX_CARRY);
        // A marker following the trimmed noise still parses.
        assert_eq!(
            scanner.scan(b"\x1b]133;C\x07"),
            vec![Osc133Event::CommandStarted]
        );
    }

    /// 验证：`reset` 清掉挂起的半截标记，残留字节不再拼成事件。
    #[test]
    fn reset_clears_pending_state() {
        let mut scanner = Osc133Scanner::new();
        assert!(scanner.scan(b"\x1b]133;C").is_empty());
        scanner.reset();
        assert!(scanner.scan(b"\x07").is_empty());
    }

    /// 验证：zsh/bash wrapper 脚本逐字对齐上游模板，`format!` 的
    /// `${...}` 转义未被破坏。
    #[test]
    fn wrapper_scripts_match_the_upstream_templates() {
        let zsh = build_zsh_osc133_wrapper("/Users/dev");
        assert!(zsh.starts_with("# OpenChamber shell integration"));
        assert!(zsh.contains("export ZDOTDIR=\"/Users/dev\"\n"));
        assert!(zsh.contains("builtin printf '\\x1b]133;D;%s\\x07' \"$exit_code\""));
        assert!(zsh.contains("builtin printf '\\x1b]777;oc-shell-ready\\x07'"));

        let bash = build_bash_osc133_rc("/Users/dev/.bashrc");
        assert!(bash.contains("trap '__oc_osc133_preexec' DEBUG"));
        assert!(bash.contains("[[ -f \"/Users/dev/.bashrc\" ]] && source \"/Users/dev/.bashrc\""));
        // The ${...} shell expansions must survive the format! escaping.
        assert!(bash.contains("if [[ -n \"${PROMPT_COMMAND[*]:-}\" ]]; then"));
    }
}
