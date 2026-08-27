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

const MAX_CARRY = 4096;

/** State that persists across chunks for one PTY session. */
export class Osc133Scanner {
  #carry = '';
  #lastCommandExit = null;

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

  reset() {
    this.#carry = '';
    this.#lastCommandExit = null;
  }
}

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
