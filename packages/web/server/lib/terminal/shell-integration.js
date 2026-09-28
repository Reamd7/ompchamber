/**
 * （中文模块说明）OSC 133 shell 集成：扫描 PTY 输出中的命令边界标记，
 * 并生成 zsh/bash 的注入脚本，使无原生集成能力的 shell 也能上报
 * 命令开始/结束（配合 runtime 的 command-finished 事件）。
 */
/**
 * OSC 133 shell integration: detect command boundary markers in PTY output.
 *
 * Shells emit these when shell-integration is active:
 *   \e]133;C\a  — command started (preexec)
 *   \e]133;D;<exit>\a  — command finished (precmd after execution)
 *
 * The scanner is chunk-boundary safe: incomplete sequences carry across
 * chunks via a pending buffer. BEL (\x07) and ST (\x1b\\) terminators are
 * both accepted.
 */

/** 跨 chunk 缓冲上限：超过即只保留尾部，防止无标记输出无限累积。 */
const MAX_CARRY = 4096;

/** （中文说明）每个 PTY 会话一个扫描器实例，跨 chunk 保存未完成序列。 */
/** State that persists across chunks for one PTY session. */
export class Osc133Scanner {
  // 待续写缓冲：可能包含未终结的 OSC 序列前缀。
  #carry = '';
  // 最近一次 command-finished 的退出码（当前未被外部消费）。
  #lastCommandExit = null;

  /**
   * （中文说明）逐块扫描；匹配后从缓冲中删除已消费序列并重置正则游标。
   */
  /**
   * Scan a chunk of PTY output for OSC 133 markers.
   * @param {string} chunk — raw PTY output
   * @returns {Array<{kind: 'command-started'} | {kind: 'command-finished', exitCode: number | null}>}
   */
  scan(chunk) {
    this.#carry += chunk;
    const events = [];
    // Match \e]133;C\a or \e]133;D;<digits>\a (also ST terminator \e\e\\)
    const re = /\x1b\]133;([CD])(?:;(\d+))?(?:\x07|\x1b\\)/g;
    let match;
    while ((match = re.exec(this.#carry)) !== null) {
      if (match[1] === 'C') {
        events.push({ kind: 'command-started' });
      } else if (match[1] === 'D') {
        const exitCode = match[2] !== undefined ? Number.parseInt(match[2], 10) : null;
        this.#lastCommandExit = exitCode;
        events.push({ kind: 'command-finished', exitCode });
      }
      // Remove the matched sequence from carry
      this.#carry = this.#carry.slice(0, match.index) + this.#carry.slice(match.index + match[0].length);
      re.lastIndex = 0; // restart since we modified the string
    }
    // Keep only a bounded tail (potential incomplete sequence prefix)
    if (this.#carry.length > MAX_CARRY) {
      this.#carry = this.#carry.slice(-MAX_CARRY);
    }
    return events;
  }

  /** 清空跨 chunk 缓冲与上次退出码（会话重启复用 scanner 前调用）。 */
  reset() {
    this.#carry = '';
    this.#lastCommandExit = null;
  }
}

/**
 * （中文说明）生成写入临时 .zshenv 的 wrapper 脚本内容：
 * 先把 ZDOTDIR 还给用户配置，再挂 precmd/preexec 钩子发标记。
 */
/**
 * Build the zsh wrapper script that emits OSC 133 markers.
 * The wrapper is written to a temp .zshenv that gives ZDOTDIR back
 * to the user immediately, then sets up precmd/preexec hooks.
 */
export function buildZshOsc133Wrapper(userZdotdir) {
  return `# OpenChamber shell integration: OSC 133 command boundary markers
# Give ZDOTDIR back to the user immediately (prevents HISTFILE bugs)
export ZDOTDIR="${userZdotdir}"

# OSC 133 hooks
__oc_osc133_precmd() {
  local exit_code=$?
  if [[ -n "\${__oc_in_command:-}" ]]; then
    builtin printf '\x1b]133;D;%s\x07' "$exit_code"
    builtin unset __oc_in_command
  fi
  builtin printf '\x1b]133;A\x07'
}
__oc_osc133_preexec() {
  builtin printf '\x1b]133;C\x07'
  builtin typeset -g __oc_in_command=1
}

# Register hooks (after user's config loads via ZDOTDIR handback)
autoload -Uz add-zsh-hook
add-zsh-hook precmd __oc_osc133_precmd
add-zsh-hook preexec __oc_osc133_preexec

# Ready marker
builtin printf '\x1b]777;oc-shell-ready\x07'
`;
}

/**
 * （中文说明）生成 bash --rcfile 用的 rc 片段：链回用户 .bashrc 并挂钩子。
 */
/**
 * Build the bash rcfile snippet that emits OSC 133 markers.
 */
export function buildBashOsc133Rc(userBashrc) {
  return `# OpenChamber shell integration: OSC 133 command boundary markers
__oc_osc133_preexec() { builtin printf '\x1b]133;C\x07'; builtin typeset -g __oc_in_command=1; }
__oc_osc133_precmd() {
  local exit_code=$?
  if [[ -n "\${__oc_in_command:-}" ]]; then
    builtin printf '\x1b]133;D;%s\x07' "$exit_code"
    builtin unset __oc_in_command
  fi
  builtin printf '\x1b]133;A\x07'
}
# Append to existing PROMPT_COMMAND (bash >=5.1 uses array)
if [[ -n "\${PROMPT_COMMAND[*]:-}" ]]; then
  PROMPT_COMMAND+=('__oc_osc133_precmd')
else
  PROMPT_COMMAND='__oc_osc133_precmd'
fi
trap '__oc_osc133_preexec' DEBUG
builtin printf '\x1b]777;oc-shell-ready\x07'
# Source user's bashrc
[[ -f "${userBashrc}" ]] && source "${userBashrc}"
`;
}
