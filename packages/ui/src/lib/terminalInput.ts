export type TerminalModifier = 'ctrl' | 'alt' | 'shift';

export type TerminalQuickKey =
  | 'esc' | 'tab' | 'enter' | 'space' | 'backspace' | 'delete'
  | 'arrow-up' | 'arrow-down' | 'arrow-left' | 'arrow-right'
  | 'home' | 'end' | 'page-up' | 'page-down' | 'insert'
  | 'f1' | 'f2' | 'f3' | 'f4' | 'f5' | 'f6' | 'f7' | 'f8' | 'f9' | 'f10' | 'f11' | 'f12';

const sequences: Record<TerminalQuickKey, string> = {
  esc: '\u001b', tab: '\t', enter: '\r', space: ' ', backspace: '\u007f',
  delete: '\u001b[3~', insert: '\u001b[2~',
  'arrow-up': '\u001b[A', 'arrow-down': '\u001b[B', 'arrow-left': '\u001b[D', 'arrow-right': '\u001b[C',
  home: '\u001b[H', end: '\u001b[F', 'page-up': '\u001b[5~', 'page-down': '\u001b[6~',
  f1: '\u001bOP', f2: '\u001bOQ', f3: '\u001bOR', f4: '\u001bOS',
  f5: '\u001b[15~', f6: '\u001b[17~', f7: '\u001b[18~', f8: '\u001b[19~',
  f9: '\u001b[20~', f10: '\u001b[21~', f11: '\u001b[23~', f12: '\u001b[24~',
};

// CSI modifier parameter: 1=shift 2=alt 3=alt+shift 5=ctrl 6=ctrl+shift 7=ctrl+alt 8=ctrl+alt+shift
const modifierParam = (mods: Set<TerminalModifier>): number => {
  const c = mods.has('ctrl'); const a = mods.has('alt'); const s = mods.has('shift');
  if (c && a && s) return 8;
  if (c && a) return 7;
  if (c && s) return 6;
  if (c) return 5;
  if (a && s) return 3;
  if (a) return 2;
  if (s) return 1;
  return 0;
};

// Keys that can take a CSI modifier parameter (CSI-final or CSI-tilde forms)
const csiModifiable: ReadonlySet<string> = new Set([
  'arrow-up', 'arrow-down', 'arrow-left', 'arrow-right',
  'home', 'end', 'page-up', 'page-down', 'insert', 'delete',
]);

// F5-F12 use CSI-tilde form and can take modifiers; F1-F4 use SS3 and cannot
const tildeKeys: ReadonlySet<string> = new Set([
  'f5', 'f6', 'f7', 'f8', 'f9', 'f10', 'f11', 'f12',
]);

const tildeFinals: Record<string, string> = {
  'f5': '15~', 'f6': '17~', 'f7': '18~', 'f8': '19~',
  'f9': '20~', 'f10': '21~', 'f11': '23~', 'f12': '24~',
  'page-up': '5~', 'page-down': '6~', 'insert': '2~', 'delete': '3~',
};

export const terminalSequenceForKey = (
  key: TerminalQuickKey,
  ...modifiers: Array<TerminalModifier | null | undefined>
): string => {
  const mods = new Set(modifiers.filter((m): m is TerminalModifier => m !== null && m !== undefined));
  if (mods.size === 0) return sequences[key];

  const param = modifierParam(mods);
  if (!param) return sequences[key];

  if (csiModifiable.has(key)) {
    const finals: Partial<Record<TerminalQuickKey, string>> = {
      'arrow-up': 'A', 'arrow-down': 'B', 'arrow-right': 'C', 'arrow-left': 'D',
      home: 'H', end: 'F',
    };
    const final = finals[key];
    if (final) return `\u001b[1;${param}${final}`;
  }

  if (tildeKeys.has(key) || key === 'page-up' || key === 'page-down' || key === 'insert' || key === 'delete') {
    // CSI-tilde keys: \e[<tilde-final> → \e[<n>;<param><tilde-final>
    const tf = tildeFinals[key];
    if (tf) {
      const tildeNum = parseInt(tf, 10);
      return `\u001b[${tildeNum};${param}~`;
    }
  }

  // Tab with shift = backtab
  if (key === 'tab' && mods.has('shift') && mods.size === 1) return '\u001b[Z';

  // Enter with alt = \e\r (bracketed-paste newline); Enter with ctrl = \n
  if (key === 'enter') {
    if (mods.has('alt') && mods.size === 1) return '\u001b\r';
    if (mods.has('ctrl') && mods.size === 1) return '\n';
  }

  // Backspace with ctrl = DEL variant
  if (key === 'backspace' && mods.has('ctrl')) return '\u001b\u007f';

  // No modifier encoding available for this key+modifier combination
  return sequences[key];
};

export const terminalControlCharacter = (value: string): string | null => {
  const character = value[0]?.toUpperCase();
  if (!character || character < 'A' || character > 'Z') return null;
  return String.fromCharCode(character.charCodeAt(0) & 0b11111);
};

export const applyTerminalModifier = (value: string, modifier: TerminalModifier): string => {
  if (!value) return value;
  if (modifier === 'ctrl') return terminalControlCharacter(value) ?? value;
  if (modifier === 'shift') return value.length === 1 ? value.toUpperCase() : value;
  return value.length === 1 && value !== '\u001b' ? `\u001b${value}` : value;
};

// Convenience: build a scroll-arrow sequence for alt-screen TUI scrolling
export const terminalScrollArrows = (lines: number, direction: 'up' | 'down'): string => {
  const seq = direction === 'up' ? '\u001b[A' : '\u001b[B';
  return seq.repeat(Math.max(1, Math.min(lines, 32)));
};
