/**
 * enabledModels pattern matching (spec 01 GAP-10).
 *
 * Mirrors the SDK's `filterAvailableModelsByEnabledPatterns` selector surface
 * for the UI picker: exact `provider/id`, glob wildcards (`*`, `?`) matched
 * against `provider/id`, bare model ids matching any provider, and a
 * `:thinkingLevel` suffix that is stripped before matching. An empty pattern
 * list disables filtering (all models allowed).
 */

const globToRegExp = (glob: string): RegExp => {
  const escaped = glob.replace(/[.+^${}()|[\]\\]/g, '\\$&');
  return new RegExp(`^${escaped.replace(/\*/g, '.*').replace(/\?/g, '.')}$`, 'i');
};

export const normalizeEnabledModelPattern = (pattern: string): string => {
  const withoutThinking = pattern.includes(':') ? pattern.slice(0, pattern.indexOf(':')) : pattern;
  return withoutThinking.trim().toLowerCase();
};

export interface EnabledModelsMatcher {
  /** True when `providerID/modelID` is allowed by the configured patterns. */
  allows: (providerID: string, modelID: string) => boolean;
}

export const createEnabledModelsMatcher = (patterns: readonly string[]): EnabledModelsMatcher | null => {
  const normalized = patterns
    .map(normalizeEnabledModelPattern)
    .filter((pattern) => pattern.length > 0);
  if (normalized.length === 0) return null;

  const exact = new Set<string>();
  const bareIds = new Set<string>();
  const globs: RegExp[] = [];
  for (const pattern of normalized) {
    if (pattern.includes('*') || pattern.includes('?')) {
      globs.push(globToRegExp(pattern));
    } else if (pattern.includes('/')) {
      exact.add(pattern);
    } else {
      bareIds.add(pattern);
    }
  }

  return {
    allows: (providerID, modelID) => {
      const provider = providerID.toLowerCase();
      const model = modelID.toLowerCase();
      const full = `${provider}/${model}`;
      if (exact.has(full)) return true;
      if (bareIds.has(model)) return true;
      return globs.some((re) => re.test(full));
    },
  };
};
