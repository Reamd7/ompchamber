import { describe, expect, test } from 'bun:test';
import { applyTerminalModifier, terminalControlCharacter, terminalSequenceForKey } from './terminalInput';

describe('terminal input translation', () => {
  test('translates navigation, editing, and control keys', () => {
    expect(terminalSequenceForKey('arrow-up')).toBe('\u001b[A');
    expect(terminalSequenceForKey('arrow-left', 'ctrl')).toBe('\u001b[1;5D');
    expect(terminalSequenceForKey('arrow-right', 'alt')).toBe('\u001b[1;2C');
    expect(terminalSequenceForKey('enter')).toBe('\r');
    expect(terminalControlCharacter('c')).toBe('\u0003');
    expect(terminalControlCharacter('[')).toBeNull();
    expect(applyTerminalModifier('c', 'ctrl')).toBe('\u0003');
    expect(applyTerminalModifier('b', 'alt')).toBe('\u001bb');
    expect(applyTerminalModifier('\u001b[1;3C', 'alt')).toBe('\u001b[1;3C');
  });

  test('full key coverage: 26 special keys encode to escape sequences', () => {
    expect(terminalSequenceForKey('home')).toBe('\u001b[H');
    expect(terminalSequenceForKey('end')).toBe('\u001b[F');
    expect(terminalSequenceForKey('page-up')).toBe('\u001b[5~');
    expect(terminalSequenceForKey('page-down')).toBe('\u001b[6~');
    expect(terminalSequenceForKey('insert')).toBe('\u001b[2~');
    expect(terminalSequenceForKey('delete')).toBe('\u001b[3~');
    expect(terminalSequenceForKey('backspace')).toBe('\u007f');
    expect(terminalSequenceForKey('space')).toBe(' ');
    expect(terminalSequenceForKey('f1')).toBe('\u001bOP');
    expect(terminalSequenceForKey('f5')).toBe('\u001b[15~');
    expect(terminalSequenceForKey('f12')).toBe('\u001b[24~');
  });

  test('modifier combinations use correct CSI parameters', () => {
    expect(terminalSequenceForKey('arrow-right', 'ctrl')).toBe('\u001b[1;5C');
    expect(terminalSequenceForKey('arrow-right', 'alt')).toBe('\u001b[1;2C');
    expect(terminalSequenceForKey('arrow-right', 'shift')).toBe('\u001b[1;1C');
    expect(terminalSequenceForKey('arrow-right', 'ctrl', 'alt')).toBe('\u001b[1;7C');
    expect(terminalSequenceForKey('home', 'ctrl')).toBe('\u001b[1;5H');
    expect(terminalSequenceForKey('page-up', 'ctrl')).toBe('\u001b[5;5~');
    expect(terminalSequenceForKey('tab', 'shift')).toBe('\u001b[Z');
    expect(terminalSequenceForKey('enter', 'alt')).toBe('\u001b\r');
    expect(terminalSequenceForKey('enter', 'ctrl')).toBe('\n');
  });
});
