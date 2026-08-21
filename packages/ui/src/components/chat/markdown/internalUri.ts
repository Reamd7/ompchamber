/**
 * Internal-URI link recognition for the chat markdown pipeline
 * (spec 04 §5.2.5, GAP-01).
 *
 * `local://` (P1) and the other internal schemes the omp URI bridge knows are
 * NOT linkified unconditionally: every parse runs inside a synchronous
 * dynamic scope (`withInternalUriSchemes`) carrying the capability-enabled
 * scheme set, and both the marked link renderer and the DOMPurify href hook
 * consult that scope. Nothing outside a scope sees an enabled scheme, so a
 * capability-off transcript renders exactly as before and no sanitizer
 * whitelist changes survive past the gated render.
 */

/** Internal URI schemes the omp URI bridge recognizes (spec 04 §5.2.5). */
export const INTERNAL_URI_SCHEMES = [
  'local',
  'agent',
  'history',
  'artifact',
  'skill',
  'memory',
  'rule',
  'omp',
  'issue',
  'pr',
  'ssh',
  'vault',
  'security',
  'mcp',
  'xd',
] as const;

export type InternalUriScheme = (typeof INTERNAL_URI_SCHEMES)[number];

/**
 * Schemes linkified while the `uri.v1` capability is on — mirrors the
 * server's P1 read set (`uriCapabilities().read`, domain-uri.js).
 */
export const URI_V1_ENABLED_SCHEMES: readonly InternalUriScheme[] = ['local'];

const INTERNAL_SCHEME_PREFIX_RE = /^(local|agent|history|artifact|skill|memory|rule|omp|issue|pr|ssh|vault|security|mcp|xd):\/\//;

/** The value's internal scheme, or null when it is not an internal URI. */
export const internalUriSchemeOf = (value: string): InternalUriScheme | null => {
  const match = INTERNAL_SCHEME_PREFIX_RE.exec(value);
  return match === null ? null : (match[1] as InternalUriScheme);
};

let activeSchemes: ReadonlySet<string> | null = null;

/**
 * Runs `fn` with `schemes` as the active internal-URI set. `fn` must stay
 * synchronous — the scope rides the single-threaded parse/sanitize call, so
 * concurrent renders (streaming blocks) cannot leak schemes into each other.
 */
export const withInternalUriSchemes = <T>(schemes: readonly string[] | null, fn: () => T): T => {
  const previous = activeSchemes;
  activeSchemes = schemes !== null && schemes.length > 0 ? new Set(schemes) : null;
  try {
    return fn();
  } finally {
    activeSchemes = previous;
  }
};

/**
 * The value's internal scheme when that scheme is enabled for the current
 * parse scope, else null. This is the single gate the link renderer and the
 * sanitizer hook share — a `local://` href is keepable ONLY inside a scope
 * that enabled it.
 */
export const activeInternalUriSchemeOf = (value: string): InternalUriScheme | null => {
  if (activeSchemes === null) return null;
  const scheme = internalUriSchemeOf(value);
  return scheme !== null && activeSchemes.has(scheme) ? scheme : null;
};

export interface InternalUriTextMatch {
  url: string;
  start: number;
  end: number;
}

const SCHEME_BOUNDARY_CHAR = /[A-Za-z0-9+.-]/;
const escapeRegExp = (value: string): string => value.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');

/**
 * Finds enabled internal-URI substrings in plain text (spec 04 §5.2.5:
 * `scheme://[A-Za-z0-9_./-]+`, only for schemes in the enabled set). A match
 * must start at a scheme boundary (`xlocal://a` does not match); trailing
 * dots drop as sentence punctuation; a bare `scheme://` with no ref does not
 * match (explicit markdown links cover the directory-listing form).
 */
export const findInternalUriMatches = (text: string, schemes: readonly string[]): InternalUriTextMatch[] => {
  if (schemes.length === 0 || text.indexOf('://') === -1) return [];
  const pattern = new RegExp(`(?:${schemes.map(escapeRegExp).join('|')}):\\/\\/[A-Za-z0-9_./-]+`, 'g');
  const matches: InternalUriTextMatch[] = [];
  let match: RegExpExecArray | null;
  while ((match = pattern.exec(text)) !== null) {
    const start = match.index;
    if (start > 0 && SCHEME_BOUNDARY_CHAR.test(text[start - 1])) continue;
    let end = start + match[0].length;
    while (end > start && text[end - 1] === '.') end -= 1;
    if (text.slice(start, end).endsWith('://')) continue;
    matches.push({ url: text.slice(start, end), start, end });
  }
  return matches;
};
