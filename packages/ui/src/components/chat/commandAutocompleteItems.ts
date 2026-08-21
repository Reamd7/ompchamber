import { fuzzyMatch } from '@/lib/utils';

export interface CommandAutocompleteSearchItem {
  name: string;
  description?: string;
  searchAliases?: string[];
  isBuiltIn?: boolean;
  isSkill?: boolean;
  /** omp discovery layer (GET /api/omp/commands — spec 08 §5.4). */
  isOmp?: boolean;
  /** The omp layer displaced a lower OpenChamber layer with this name (collision notice). */
  ompOverrides?: boolean;
}

function addSearchAliases<T extends CommandAutocompleteSearchItem>(winner: T, duplicate: T): T {
  const existingAliases = winner.searchAliases ?? [];
  const aliases = [
    ...existingAliases,
    ...(winner.name === duplicate.name ? [] : [duplicate.name]),
    ...(duplicate.description ? [duplicate.description] : []),
    ...(duplicate.searchAliases ?? []),
  ].filter((alias, index, values) => alias !== winner.description && values.indexOf(alias) === index);
  const unchanged = aliases.length === existingAliases.length
    && aliases.every((alias, index) => alias === existingAliases[index]);

  return unchanged ? winner : { ...winner, searchAliases: aliases };
}

const withOmpOverride = <T extends CommandAutocompleteSearchItem>(item: T, flag: boolean): T =>
  flag ? { ...item, ompOverrides: true } : item;

/**
 * Precedence is the omp discovery layer, local command, discovered skill,
 * OpenCode skill-command, then custom/plugin command (spec 08 §5.4 three-layer
 * pipeline: omp → custom → magic). Identity matches session.command's
 * case-sensitive lookup. An omp winner that displaced a lower layer keeps that
 * layer's search aliases and is flagged `ompOverrides` for the collision badge.
 */
export function mergeCommandAutocompleteItems<T extends CommandAutocompleteSearchItem>(
  builtIns: T[],
  commands: T[],
  skills: T[],
  ompCommands: T[] = [],
): T[] {
  const merged: T[] = [];
  const byName = new Map<string, { index: number; item: T; precedence: number; omp: boolean }>();

  const addItems = (items: T[], getPrecedence: (item: T) => number, omp: boolean) => {
    for (const item of items) {
      const precedence = getPrecedence(item);
      const identity = item.name;
      const existing = byName.get(identity);
      if (!existing) {
        byName.set(identity, { index: merged.length, item, precedence, omp });
        merged.push(item);
        continue;
      }

      const incomingWins = precedence > existing.precedence;
      const winner = incomingWins
        ? withOmpOverride(addSearchAliases(item, existing.item), omp && !existing.omp)
        : // The omp layer is added first, so a surviving omp row overrode the
          // incoming lower layer — that displacement is the collision notice.
          withOmpOverride(addSearchAliases(existing.item, item), existing.omp && !omp);
      merged[existing.index] = winner;
      byName.set(identity, {
        index: existing.index,
        item: winner,
        precedence: Math.max(existing.precedence, precedence),
        omp: incomingWins ? omp : existing.omp,
      });
    }
  };

  addItems(ompCommands, () => 4, true);
  addItems(builtIns, () => 3, false);
  addItems(commands, (item) => item.isBuiltIn ? 3 : item.isSkill ? 1 : 0, false);
  addItems(skills, () => 2, false);
  return merged;
}

export function commandMatchesSearch(command: CommandAutocompleteSearchItem, query: string): boolean {
  return fuzzyMatch(command.name, query)
    || Boolean(command.description && fuzzyMatch(command.description, query))
    || Boolean(command.searchAliases?.some((alias) => fuzzyMatch(alias, query)));
}
