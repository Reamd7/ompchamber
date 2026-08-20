/**
 * Wire residue contract guard (spec docs/omp-parity/07 §5.15, Step C).
 *
 * The vendored OpenCode wire gen (`wire/gen/`) stays untouched (master D1),
 * so the deletion contract is enforced on the CONSUMER side instead: the
 * namespaces this module bans must never re-enter non-test source. Each rule
 * names the GAP that owns the ban; a new mapping requirement must first amend
 * the rule list here (= an explicit disposition revision, 07 §5.15).
 *
 * Scanned roots:
 *   - packages/ui/src                 (UI consumer surface)
 *   - packages/web/server/lib/omp-host (producer surface; engine/projection)
 *
 * Exemptions:
 *   - `wire/gen/` (vendored types, leave per 07 §5.0)
 *   - `*.test.*` / `*.spec.*` files (this guard's own fixtures)
 *   - a line ending in `// wire-residue-allow` (historical comment quotes)
 *
 * `message.part.removed` is deliberately NOT banned anywhere (master D6-R5:
 * it is the ch05 P2 retry-supersession carrier); the producer rule anchors
 * both quotes so only the exact `message.removed` literal matches.
 */

import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const MODULE_DIR = path.dirname(fileURLToPath(import.meta.url));
const PACKAGES_DIR = path.resolve(MODULE_DIR, '..', '..', '..', '..');

export const WIRE_RESIDUE_ROOTS = {
  ui: path.join(PACKAGES_DIR, 'ui', 'src'),
  ompHost: path.join(PACKAGES_DIR, 'web', 'server', 'lib', 'omp-host'),
} as const;

const ALLOW_MARKER = 'wire-residue-allow';
const SOURCE_EXTENSION = /\.(ts|tsx|js|jsx|mjs|cjs)$/;
const TEST_FILE = /\.(test|spec)\./;
// Static skip table (never mutated at runtime).
const SKIP_DIR: Record<string, true> = {
  node_modules: true,
  gen: true,
  dist: true,
  build: true,
  out: true,
  '.git': true,
};

export type WireResidueGap = 'G01' | 'G06' | 'G07' | 'G08';

export type WireResidueRule = {
  gap: WireResidueGap;
  /** What the rule bans; printed in failure messages. */
  reason: string;
  pattern: RegExp;
  /** Restrict to these roots (keys of WIRE_RESIDUE_ROOTS); omit for all. */
  roots?: ReadonlyArray<keyof typeof WIRE_RESIDUE_ROOTS>;
};

export const WIRE_RESIDUE_RULES: readonly WireResidueRule[] = [
  {
    gap: 'G06',
    reason: 'tui-bridge events have no producer and no consumer (07 §5.6)',
    pattern: /["'`]tui\.(command\.execute|toast\.show|prompt\.append|session\.select)\b/,
  },
  {
    gap: 'G07',
    reason: 'session.next.* durable stream is not adopted; omp events project through RuntimeAPIs/wire (07 §5.7)',
    pattern: /["'`]session\.next\./,
  },
  {
    gap: 'G01',
    reason: 'session share call surface deleted; share is an OpenCode cloud feature with no omp equivalent (07 §5.1)',
    pattern: /\b(shareSession|unshareSession)\b|\.session\.(share|unshare)\(/,
  },
  {
    gap: 'G08',
    reason: 'message.removed is never produced (05 §5.3.2); the UI consumer chain stays until the G08 P3 sweep',
    pattern: /["']message\.removed["']/,
    roots: ['ompHost'],
  },
];

export type WireResidueViolation = {
  gap: WireResidueGap;
  reason: string;
  /** Repo-relative path for readable failure output. */
  file: string;
  line: number;
  text: string;
};

const listSourceFiles = (root: string): string[] => {
  const found: string[] = [];
  const walk = (directory: string): void => {
    for (const entry of fs.readdirSync(directory, { withFileTypes: true })) {
      if (entry.isDirectory()) {
        if (SKIP_DIR[entry.name]) continue;
        walk(path.join(directory, entry.name));
        continue;
      }
      if (!SOURCE_EXTENSION.test(entry.name) || TEST_FILE.test(entry.name)) continue;
      found.push(path.join(directory, entry.name));
    }
  };
  walk(root);
  return found.sort();
};

/** True when the rule applies to at least one of the scanned roots. */
const ruleAppliesToRoots = (
  rule: WireResidueRule,
  roots: ReadonlyArray<keyof typeof WIRE_RESIDUE_ROOTS>,
): boolean => rule.roots === undefined || rule.roots.some((root) => roots.includes(root));

/** Applies the rule list to one line of source; exported for the guard's own tests. */
export const findWireResidueInLine = (
  line: string,
  rules: readonly WireResidueRule[] = WIRE_RESIDUE_RULES,
  roots: ReadonlyArray<keyof typeof WIRE_RESIDUE_ROOTS> = Object.keys(WIRE_RESIDUE_ROOTS) as Array<
    keyof typeof WIRE_RESIDUE_ROOTS
  >,
): WireResidueRule[] => {
  if (line.includes(ALLOW_MARKER)) return [];
  return rules.filter((rule) => ruleAppliesToRoots(rule, roots) && rule.pattern.test(line));
};

export const scanWireResidue = (): WireResidueViolation[] => {
  const violations: WireResidueViolation[] = [];
  for (const [rootKey, root] of Object.entries(WIRE_RESIDUE_ROOTS) as Array<
    [keyof typeof WIRE_RESIDUE_ROOTS, string]
  >) {
    if (!fs.existsSync(root)) continue;
    for (const file of listSourceFiles(root)) {
      const lines = fs.readFileSync(file, 'utf8').split(/\r?\n/);
      lines.forEach((text, index) => {
        for (const rule of findWireResidueInLine(text, WIRE_RESIDUE_RULES, [rootKey])) {
          violations.push({
            gap: rule.gap,
            reason: rule.reason,
            file: path.relative(PACKAGES_DIR, file),
            line: index + 1,
            text: text.trim(),
          });
        }
      });
    }
  }
  return violations;
};
