/**
 * Per-turn usage formatting and prompt-cache invalidation detection (spec
 * docs/omp-parity/05 §5.9, GAP-E12/E13).
 *
 * Both are ports of the TUI reference implementations — the acceptance
 * baseline (05 §7.3):
 * - `formatUsageRow` — usage-row.ts:19-42 (thresholds: cacheRead>0, ttft>0,
 *   duration>100ms && output>0; glyphs ⤵/⤴/💾/⏱/⚡ from the TUI theme).
 * - `detectCacheInvalidation` — cache-invalidation-marker.ts:49-66 (four
 *   conditions; MIN_CACHE_FOOTPRINT 2048; only explicit prefix caches).
 *
 * Zero server changes: inputs are the wire `info.tokens` the projection
 * already emits (`projectUsage`, projection.js) plus the optional omp
 * `omp.usage.turn` telemetry entry for ttft/duration/timestamp. When telemetry
 * is missing the row degrades to tokens-only (05 §5.9).
 */

import type { Message } from '@/lib/opencode/wire';
import type { OmpTelemetryTurn } from '@/sync/omp-event-reducer';

/** Below this the tok/s rate is nonsense (cached/instant responses). */
const MIN_DURATION_MS = 100;

/** Minimum prior warm-cache read before a cold turn counts as an invalidation. */
export const MIN_CACHE_FOOTPRINT = 2048;

/** The token counts a turn's usage line and cache detection need. */
export interface TurnUsageTokens {
    input: number;
    output: number;
    cacheRead: number;
    cacheWrite: number;
}

export interface CacheInvalidation {
    /** Prompt tokens the cold turn had to (re)process instead of reading from cache. */
    reprocessedTokens: number;
}

/**
 * Decide whether `current` turn lost a *working* prompt cache that `prev` was
 * reusing (cache-invalidation-marker.ts:49-66, conditions unchanged):
 * 1. a predecessor must have READ a meaningful prefix (prev.cacheRead ≥ 2048);
 * 2. this turn read nothing back (current.cacheRead === 0);
 * 3. the prefix was explicitly rebuilt (current.cacheWrite > 0) — implicit
 *    caches (Google/OpenAI) report 0 and must not be flagged;
 * 4. the reprocessed prompt is non-trivial (cacheWrite + input ≥ 2048).
 */
export function detectCacheInvalidation(
    prev: TurnUsageTokens | undefined,
    current: TurnUsageTokens,
): CacheInvalidation | undefined {
    if (!prev) return undefined;
    if (prev.cacheRead < MIN_CACHE_FOOTPRINT) return undefined;
    if (current.cacheRead > 0) return undefined;
    if (current.cacheWrite <= 0) return undefined;
    const reprocessedTokens = current.cacheWrite + current.input;
    if (reprocessedTokens < MIN_CACHE_FOOTPRINT) return undefined;
    return { reprocessedTokens };
}

/**
 * Whether a turn's usage reflects billed work (TUI
 * `assistantUsageIsBilled`): all-zero provider rows collapse to false so
 * automated empty turns render no line.
 */
export function turnUsageHasActivity(tokens: TurnUsageTokens): boolean {
    return tokens.input > 0
        || tokens.output > 0
        || tokens.cacheRead > 0
        || tokens.cacheWrite > 0;
}

/** Compact token counts the way the TUI does (pi-utils formatNumber). */
export function formatCompactTokenCount(n: number): string {
    if (n < 1_000) return String(n);
    if (n < 10_000) return `${trimDecimal1(n / 1_000)}K`;
    if (n < 1_000_000) return `${Math.round(n / 1_000)}K`;
    if (n < 10_000_000) return `${trimDecimal1(n / 1_000_000)}M`;
    if (n < 1_000_000_000) return `${Math.round(n / 1_000_000)}M`;
    return `${Math.round(n / 1_000_000_000)}B`;
}

const trimDecimal1 = (n: number): string => {
    const s = n.toFixed(1);
    return s.endsWith('.0') ? s.slice(0, -2) : s;
};

const pad2 = (n: number): string => String(n).padStart(2, '0');

/** Local `YYYY-MM-DD HH:mm:ss` stamp (usage-row.ts:10-16). */
export function formatUsageTimestamp(ms: number): string {
    const d = new Date(ms);
    const date = `${d.getFullYear()}-${pad2(d.getMonth() + 1)}-${pad2(d.getDate())}`;
    const time = `${pad2(d.getHours())}:${pad2(d.getMinutes())}:${pad2(d.getSeconds())}`;
    return `${date} ${time}`;
}

export interface TurnUsageLineInput {
    tokens: TurnUsageTokens;
    ttftMs?: number;
    durationMs?: number;
    timestamp?: number;
}

/**
 * Format the per-turn usage line, field order and thresholds identical to
 * usage-row.ts:19-42:
 * `YYYY-MM-DD HH:MM:SS ⤵ in(input+cacheWrite) ⤴ out 💾 cacheRead ⏱ ttft ⚡ tok/s`
 * with cacheRead shown only when >0, ttft only when >0, and tok/s only when
 * duration > 100ms and output > 0.
 */
export function formatTurnUsageLine(input: TurnUsageLineInput): string {
    const { tokens, ttftMs, durationMs, timestamp } = input;
    const totalInput = tokens.input + tokens.cacheWrite;
    const parts: string[] = [];
    if (typeof timestamp === 'number' && Number.isFinite(timestamp) && timestamp > 0) {
        parts.push(formatUsageTimestamp(timestamp));
    }
    parts.push(`⤵ ${formatCompactTokenCount(totalInput)}`);
    parts.push(`⤴ ${formatCompactTokenCount(tokens.output)}`);
    if (tokens.cacheRead > 0) {
        parts.push(`💾 ${formatCompactTokenCount(tokens.cacheRead)}`);
    }
    if (typeof ttftMs === 'number' && ttftMs > 0) {
        parts.push(`⏱ ${(ttftMs / 1000).toFixed(1)}s`);
    }
    if (typeof durationMs === 'number' && durationMs > MIN_DURATION_MS && tokens.output > 0) {
        // TPS over the total request duration — the post-TTFT window undercounts
        // generation time when reasoning tokens precede the first visible byte.
        const tokPerSec = (tokens.output / durationMs) * 1000;
        parts.push(`⚡ ${tokPerSec.toFixed(1)}/s`);
    }
    return parts.join('  ');
}

const readCount = (value: unknown): number => (typeof value === 'number' && Number.isFinite(value) && value >= 0 ? value : 0);

/** Wire `info.tokens` shape (`projectUsage`): flat counts + nested cache. */
export function tokensFromWireInfo(info: Message): TurnUsageTokens | null {
    const tokens = (info as { tokens?: unknown }).tokens as
        | { input?: unknown; output?: unknown; cache?: { read?: unknown; write?: unknown } }
        | undefined;
    if (!tokens) return null;
    return {
        input: readCount(tokens.input),
        output: readCount(tokens.output),
        cacheRead: readCount(tokens.cache?.read),
        cacheWrite: readCount(tokens.cache?.write),
    };
}

/**
 * Tokens from a store telemetry entry (`omp.usage.turn` payload: SDK `usage`
 * record + ttft/duration/timestamp). Returns null when the entry carries no
 * usable usage record — the caller then degrades to wire tokens.
 */
function usageFromTelemetry(entry: OmpTelemetryTurn): TurnUsageLineInput | null {
    const usage = entry.usage as Record<string, unknown> | undefined;
    if (!usage) return null;
    return {
        tokens: {
            input: readCount(usage.input),
            output: readCount(usage.output),
            cacheRead: readCount(usage.cacheRead),
            cacheWrite: readCount(usage.cacheWrite),
        },
        ...(typeof entry.ttftMs === 'number' ? { ttftMs: entry.ttftMs } : {}),
        ...(typeof entry.durationMs === 'number' ? { durationMs: entry.durationMs } : {}),
        ...(typeof entry.timestamp === 'number' ? { timestamp: entry.timestamp } : {}),
    };
}

export interface ResolvedTurnUsage {
    /** Formatted usage line ready to render. */
    line: string;
    /** True when ttft/duration/timestamp came through (omp telemetry present). */
    hasTelemetry: boolean;
}

/**
 * Resolve the turn-tail usage line: prefer the omp telemetry entry (joined by
 * messageID) for tokens + ttft/duration/timestamp; degrade to tokens-only
 * from wire `info.tokens` when telemetry is missing (05 §5.9).
 * Returns null when neither source reports billed activity.
 */
export function resolveTurnUsage(
    telemetryEntry: OmpTelemetryTurn | undefined,
    wireInfo: Message,
): ResolvedTurnUsage | null {
    const fromTelemetry = telemetryEntry ? usageFromTelemetry(telemetryEntry) : null;
    if (fromTelemetry && turnUsageHasActivity(fromTelemetry.tokens)) {
        return { line: formatTurnUsageLine(fromTelemetry), hasTelemetry: true };
    }
    const wireTokens = tokensFromWireInfo(wireInfo);
    if (!wireTokens || !turnUsageHasActivity(wireTokens)) {
        return null;
    }
    return { line: formatTurnUsageLine({ tokens: wireTokens }), hasTelemetry: false };
}
