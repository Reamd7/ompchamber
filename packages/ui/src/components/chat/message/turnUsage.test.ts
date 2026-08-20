import { describe, expect, test } from 'bun:test';
import type { Message } from '@/lib/opencode/wire';

import {
    detectCacheInvalidation,
    formatCompactTokenCount,
    formatTurnUsageLine,
    formatUsageTimestamp,
    MIN_CACHE_FOOTPRINT,
    resolveTurnUsage,
    tokensFromWireInfo,
    turnUsageHasActivity,
    type TurnUsageTokens,
} from './turnUsage';

const tokens = (overrides: Partial<TurnUsageTokens> = {}): TurnUsageTokens => ({
    input: 120,
    output: 340,
    cacheRead: 0,
    cacheWrite: 80,
    ...overrides,
});

// Fixed local time so the timestamp format assertion never depends on the
// machine's timezone: 2026-08-20T09:05:07 local.
const TIMESTAMP = new Date(2026, 7, 20, 9, 5, 7).getTime();

describe('formatTurnUsageLine (usage-row.ts:19-42 parity)', () => {
    test('renders every field above its threshold', () => {
        const line = formatTurnUsageLine({
            tokens: tokens({ input: 900, output: 500, cacheRead: 4_096, cacheWrite: 100 }),
            ttftMs: 1_250,
            durationMs: 2_000,
            timestamp: TIMESTAMP,
        });
        // ↑in counts input+cacheWrite; tok/s = output/duration*1000.
        expect(line).toBe('2026-08-20 09:05:07  ⤵ 1K  ⤴ 500  💾 4.1K  ⏱ 1.3s  ⚡ 250.0/s');
    });

    test('hides cacheRead when zero and ttft when non-positive', () => {
        const line = formatTurnUsageLine({
            tokens: tokens({ cacheRead: 0, cacheWrite: 0, input: 120, output: 340 }),
            ttftMs: 0,
            durationMs: 2_000,
            timestamp: TIMESTAMP,
        });
        expect(line).toBe('2026-08-20 09:05:07  ⤵ 120  ⤴ 340  ⚡ 170.0/s');
    });

    test('hides tok/s when duration <= 100ms (rate is nonsense)', () => {
        for (const durationMs of [100, 40]) {
            const line = formatTurnUsageLine({ tokens: tokens(), durationMs, ttftMs: 50 });
            expect(line.includes('⚡')).toBe(false);
        }
    });

    test('hides tok/s when output is zero even on long turns', () => {
        const line = formatTurnUsageLine({ tokens: tokens({ output: 0 }), durationMs: 5_000 });
        expect(line.includes('⚡')).toBe(false);
    });

    test('omits the timestamp when absent or non-finite', () => {
        expect(formatTurnUsageLine({ tokens: tokens() })).toBe('⤵ 200  ⤴ 340');
        expect(formatTurnUsageLine({ tokens: tokens(), timestamp: Number.NaN })).toBe('⤵ 200  ⤴ 340');
    });

    test('compact counts match the TUI formatNumber scale', () => {
        expect(formatCompactTokenCount(999)).toBe('999');
        expect(formatCompactTokenCount(1_500)).toBe('1.5K');
        expect(formatCompactTokenCount(25_000)).toBe('25K');
        expect(formatCompactTokenCount(1_500_000)).toBe('1.5M');
        expect(formatCompactTokenCount(25_000_000)).toBe('25M');
    });

    test('timestamp is local calendar date + seconds precision', () => {
        expect(formatUsageTimestamp(TIMESTAMP)).toBe('2026-08-20 09:05:07');
    });
});

describe('detectCacheInvalidation (cache-invalidation-marker.ts:49-66 parity)', () => {
    const warmPrev = tokens({ cacheRead: 4_096 });
    const coldCurrent = tokens({ input: 1_900, cacheRead: 0, cacheWrite: 500 });

    test('flags a warm→cold transition with reprocessed = cacheWrite + input', () => {
        expect(detectCacheInvalidation(warmPrev, coldCurrent)).toEqual({ reprocessedTokens: 2_400 });
    });

    test('no marker for the first turn (no predecessor)', () => {
        expect(detectCacheInvalidation(undefined, coldCurrent)).toBe(undefined);
    });

    test('no marker when the predecessor read less than the footprint floor', () => {
        expect(detectCacheInvalidation(tokens({ cacheRead: MIN_CACHE_FOOTPRINT - 1 }), coldCurrent)).toBe(undefined);
    });

    test('no marker when the current turn reused any cache', () => {
        expect(detectCacheInvalidation(warmPrev, tokens({ cacheRead: 1 }))).toBe(undefined);
    });

    test('no marker for implicit caches (cacheWrite must rebuild the prefix)', () => {
        expect(detectCacheInvalidation(warmPrev, tokens({ cacheWrite: 0, input: 9_000 }))).toBe(undefined);
    });

    test('no marker when the reprocessed prompt is trivially small', () => {
        expect(detectCacheInvalidation(warmPrev, tokens({ input: 500, cacheWrite: 500 }))).toBe(undefined);
    });
});

describe('turnUsageHasActivity (assistantUsageIsBilled parity)', () => {
    test('any token activity keeps the row', () => {
        expect(turnUsageHasActivity(tokens({ input: 1, output: 0, cacheRead: 0, cacheWrite: 0 }))).toBe(true);
        expect(turnUsageHasActivity(tokens({ cacheWrite: 1 }))).toBe(true);
    });

    test('all-zero usage renders no row', () => {
        expect(turnUsageHasActivity(tokens({ input: 0, output: 0, cacheRead: 0, cacheWrite: 0 }))).toBe(false);
    });
});

describe('resolveTurnUsage (telemetry join + tokens-only degradation, 05 §5.9)', () => {
    const wireInfo = {
        id: 'msg_1',
        role: 'assistant',
        sessionID: 'ses_1',
        tokens: { input: 100, output: 200, reasoning: 0, cache: { read: 3_000, write: 50 } },
    } as Message;

    test('prefers telemetry: tokens plus ttft/duration/timestamp', () => {
        const resolved = resolveTurnUsage(
            {
                messageID: 'msg_1',
                usage: { input: 900, output: 500, cacheRead: 4_096, cacheWrite: 100 },
                ttftMs: 1_250,
                durationMs: 2_000,
                timestamp: TIMESTAMP,
            },
            wireInfo,
        );
        expect(resolved?.hasTelemetry).toBe(true);
        expect(resolved?.line).toBe('2026-08-20 09:05:07  ⤵ 1K  ⤴ 500  💾 4.1K  ⏱ 1.3s  ⚡ 250.0/s');
    });

    test('degrades to tokens-only from wire info.tokens when telemetry is missing', () => {
        const resolved = resolveTurnUsage(undefined, wireInfo);
        expect(resolved?.hasTelemetry).toBe(false);
        // ↑in = input+cacheWrite = 150; no timestamp, ttft, or tok/s.
        expect(resolved?.line).toBe('⤵ 150  ⤴ 200  💾 3K');
    });

    test('degrades to wire when the telemetry entry carries no usage record', () => {
        const resolved = resolveTurnUsage({ messageID: 'msg_1' }, wireInfo);
        expect(resolved?.hasTelemetry).toBe(false);
        expect(resolved?.line).toBe('⤵ 150  ⤴ 200  💾 3K');
    });

    test('renders nothing when neither source reports billed activity', () => {
        const idleInfo = { ...wireInfo, tokens: { input: 0, output: 0, reasoning: 0, cache: { read: 0, write: 0 } } } as Message;
        expect(resolveTurnUsage(undefined, idleInfo)).toBe(null);
        expect(resolveTurnUsage({ messageID: 'msg_1', usage: {} }, idleInfo)).toBe(null);
    });

    test('ignores telemetry whose usage is all-zero in favor of nothing (wire is zero too)', () => {
        const idleInfo = { ...wireInfo, tokens: undefined } as unknown as Message;
        expect(resolveTurnUsage({ messageID: 'msg_1', usage: { input: 0, output: 0 } }, idleInfo)).toBe(null);
    });
});

describe('tokensFromWireInfo', () => {
    test('maps nested wire cache counts', () => {
        const info = {
            id: 'm',
            role: 'assistant',
            tokens: { input: 10, output: 20, cache: { read: 30, write: 40 } },
        } as Message;
        expect(tokensFromWireInfo(info)).toEqual({ input: 10, output: 20, cacheRead: 30, cacheWrite: 40 });
    });

    test('null when the message reports no tokens (streaming assistant)', () => {
        expect(tokensFromWireInfo({ id: 'm', role: 'assistant' } as Message)).toBe(null);
    });
});
