/**
 * Wire residue contract guard test (spec docs/omp-parity/07 §5.15, §7).
 *
 * Two jobs:
 *   1. Repo contract — the scanned surfaces (packages/ui/src, omp-host)
 *      contain zero references to the banned wire namespaces.
 *   2. Matcher self-test — fixture lines for every banned literal must be
 *      flagged, while `message.part.removed` (05 §5.3.4 retry carrier,
 *      master D6-R5) and allow-marked lines must pass (07 §7 guard unit).
 */

import { describe, expect, test } from 'bun:test';

import {
  WIRE_RESIDUE_RULES,
  findWireResidueInLine,
  scanWireResidue,
} from './wire-residue-guard';

describe('findWireResidueInLine', () => {
  test('flags tui-bridge event literals (G06)', () => {
    for (const line of [
      'case "tui.command.execute":',
      "type: 'tui.toast.show';",
      'if (event.type === `tui.prompt.append`) return;',
      'handleTuiEvent("tui.session.select");',
    ]) {
      expect(findWireResidueInLine(line).map((rule) => rule.gap)).toContain('G06');
    }
  });

  test('flags session.next.* literals (G07)', () => {
    for (const line of [
      'case "session.next.step.started":',
      "emit('session.next.text.delta');",
    ]) {
      expect(findWireResidueInLine(line).map((rule) => rule.gap)).toContain('G07');
    }
  });

  test('flags the deleted share call surface (G01)', () => {
    for (const line of [
      'const result = await sdk().session.share({ sessionID });',
      'await sdk().session.unshare({ sessionID });',
      'const shareSession = useSessionUIStore((state) => state.shareSession);',
      'void unshareSession(id);',
    ]) {
      expect(findWireResidueInLine(line).map((rule) => rule.gap)).toContain('G01');
    }
  });

  test('flags exact message.removed literals only on the producer root (G08)', () => {
    expect(
      findWireResidueInLine('emit({ type: "message.removed" });', WIRE_RESIDUE_RULES, ['ompHost'])
        .map((rule) => rule.gap),
    ).toContain('G08');
    // The UI consumer chain is live until the G08 P3 sweep — not banned there.
    expect(
      findWireResidueInLine('case "message.removed":', WIRE_RESIDUE_RULES, ['ui']),
    ).toEqual([]);
  });

  test('never flags message.part.removed (05 P2 retry carrier, D6-R5)', () => {
    for (const roots of [undefined, ['ui'], ['ompHost']] as const) {
      expect(findWireResidueInLine('case "message.part.removed":', WIRE_RESIDUE_RULES, roots)).toEqual([]);
    }
  });

  test('exempts allow-marked historical comment lines', () => {
    expect(findWireResidueInLine('// was "tui.toast.show" once // wire-residue-allow')).toEqual([]);
  });
});

describe('wire residue repo contract (07 §5.15)', () => {
  test('scanned surfaces contain zero banned wire references', () => {
    const violations = scanWireResidue();
    if (violations.length > 0) {
      const detail = violations
        .map((violation) => `${violation.gap} ${violation.file}:${violation.line} ${violation.text}`)
        .join('\n');
      throw new Error(
        `Deleted wire namespaces re-entered consumer code (docs/omp-parity/07 §5):\n${detail}`,
      );
    }
    expect(violations).toEqual([]);
  });
});
