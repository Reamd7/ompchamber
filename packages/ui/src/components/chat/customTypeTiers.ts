/**
 * omp customType tier classification (spec docs/omp-parity/05 §5.8.2, GAP-E11).
 *
 * The server projection writes every harness-injected transcript entry as a
 * synthetic text part whose text starts with `[omp:<customType>] `
 * (omp-host projection.js `projectCustomMessage`/`projectDividerMessage`).
 * That prefix is our own projection format — not an external contract — so the
 * UI parses it locally to classify rendering without a server round-trip.
 * Structured `details` for T1 cards arrive separately through the omp event
 * stream / custom-messages endpoint and are joined by wireMessageID
 * (`useOmpSessionStore.customDetails`).
 *
 * Tier semantics (TUI baseline compaction-summary-message.ts, chat-transcript
 * -builder.ts `#appendCustomMessage`):
 * - T1 visible cards — types that need their own visual identity
 *   (advisor severity rail, irc author, async-result jobId, …).
 * - T2 folding dividers — history-collapse points rendered as one slim
 *   collapsible divider instead of a boxed note.
 * - T3 hidden — engine marks these `display:false`; the projection already
 *   drops them, so the UI normally never sees them. Classified defensively so
 *   an unfiltered path (or an older projection) renders nothing, matching the
 *   "hidden but never re-shown" rule (05 §5.8.2).
 * - T4 fallback — unknown / extension-registered types keep the plain
 *   `[omp:<type>]` text rendering.
 */

export type OmpCustomTier = 'T1' | 'T2' | 'T3' | 'T4';

/** T1 — visible cards with dedicated chrome (05 §5.8.2 tier table). */
const T1_CUSTOM_TYPES: Record<string, true> = {
    advisor: true,
    'irc:incoming': true,
    'irc:autoreply': true,
    'irc:relay': true,
    'async-result': true,
    'skill-prompt': true,
    'lsp-late-diagnostic': true,
    'live-delegation': true,
    'collab-prompt': true,
    'background-tan-dispatch': true,
};

/** T2 — history-collapse points rendered as slim collapsible dividers. */
const T2_CUSTOM_TYPES: Record<string, true> = {
    compactionSummary: true,
    branchSummary: true,
    handoff: true,
    modelChange: true,
    modeChange: true,
};

/**
 * T3 — hidden preludes and notices the engine stamps `display:false`.
 * Exact names from the 05 §5.8.2 table; the `prewalk-*` / `plan-mode-*`
 * families are matched by prefix because the SDK mints suffixed variants.
 */
const T3_CUSTOM_TYPES: Record<string, true> = {
    'eager-todo-prelude': true,
    'mid-run-todo-nudge': true,
    'todo-error-reminder': true,
    'ultrathink-notice': true,
    'orchestrate-notice': true,
    'workflow-notice': true,
    'goal-mode-context': true,
    'vibe-mode-context': true,
    'checkpoint-active-reminder': true,
    'interrupted-thinking': true,
    'resolve-reminder': true,
    'tool-call-loop-redirect': true,
    'thinking-loop-redirect': true,
    'image-attachment-description': true,
    'ttsr-injection': true,
    'goal-continuation': true,
    'session-stop-continuation': true,
    'gemini-tool-call-reminder': true,
};

const T3_CUSTOM_TYPE_PREFIXES: readonly string[] = ['prewalk-', 'plan-mode-'];

/** Classify a customType; unregistered types (and extensions) fall back to T4. */
export function tierFor(customType: string): OmpCustomTier {
    if (T1_CUSTOM_TYPES[customType]) return 'T1';
    if (T2_CUSTOM_TYPES[customType]) return 'T2';
    if (T3_CUSTOM_TYPES[customType]) return 'T3';
    for (const prefix of T3_CUSTOM_TYPE_PREFIXES) {
        if (customType.startsWith(prefix)) return 'T3';
    }
    return 'T4';
}

export interface ParsedOmpCustomText {
    customType: string;
    /** Text after the `[omp:<type>] ` label. */
    body: string;
}

/**
 * Parse the `[omp:<customType>] ` prefix our projection writes. Returns null
 * for anything else (including the label-less `[omp] ` fallback), leaving the
 * default rendering path untouched.
 */
export function parseOmpCustomText(text: string): ParsedOmpCustomText | null {
    if (!text.startsWith('[omp:')) return null;
    const close = text.indexOf(']', '[omp:'.length);
    if (close <= '[omp:'.length) return null;
    const customType = text.slice('[omp:'.length, close);
    if (customType.length === 0 || /\s/.test(customType)) return null;
    const afterClose = close + 1;
    // The label always carries one trailing space; tolerate a missing body.
    if (afterClose >= text.length) return { customType, body: '' };
    if (text[afterClose] !== ' ') return null;
    return { customType, body: text.slice(afterClose + 1) };
}
