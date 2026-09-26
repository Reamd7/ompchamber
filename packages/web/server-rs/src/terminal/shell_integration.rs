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

const MAX_CARRY: usize = 4096;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Osc133Event {
    CommandStarted,
    CommandFinished { exit_code: Option<i32> },
}

#[derive(Default)]
pub struct Osc133Scanner {
    carry: Vec<u8>,
}

impl Osc133Scanner {
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

    pub fn reset(&mut self) {
        self.carry.clear();
    }
}

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

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn reset_clears_pending_state() {
        let mut scanner = Osc133Scanner::new();
        assert!(scanner.scan(b"\x1b]133;C").is_empty());
        scanner.reset();
        assert!(scanner.scan(b"\x07").is_empty());
    }

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
