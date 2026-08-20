import React from 'react';
import type { Message } from '@/lib/opencode/wire';

import { useI18n } from '@/lib/i18n';
import { useSessionUIStore } from '@/sync/session-ui-store';
import { useOmpTelemetry } from '@/sync/useOmpSessionStore';
import { formatCompactTokenCount, resolveTurnUsage } from './turnUsage';

/**
 * Per-assistant-turn usage line + prompt-cache miss divider (spec
 * docs/omp-parity/05 §5.9, GAP-E12/E13 P2).
 *
 * `TurnUsageRow` mounts at the turn tail (the `isLastAssistantInTurn`
 * anchor, MessageBody.tsx `isLastAssistantInTurn`). Data preference:
 * omp telemetry (`omp.usage.turn`, joined by messageID) for tokens + ttft /
 * duration / timestamp; degrade to tokens-only from wire `info.tokens` when
 * the omp channel is off. Cost never appears — the SDK reports no per-message
 * cost and we do not fabricate one.
 */

export const TurnUsageRow: React.FC<{
    sessionId?: string;
    messageId: string;
    /** Wire message info — the tokens-only degradation source. */
    wireInfo: Message;
}> = ({ sessionId, messageId, wireInfo }) => {
    const getDirectoryForSession = useSessionUIStore((state) => state.getDirectoryForSession);
    const directory = sessionId ? getDirectoryForSession(sessionId) : null;
    const telemetry = useOmpTelemetry(directory ?? '', sessionId ?? undefined);
    const telemetryEntry = React.useMemo(
        () => (telemetry ? telemetry.find((entry) => entry.messageID === messageId) : undefined),
        [telemetry, messageId],
    );
    const resolved = resolveTurnUsage(telemetryEntry, wireInfo);
    if (!resolved) return null;
    return (
        <div
            className="mt-1 mb-2 text-xs whitespace-pre-wrap text-muted-foreground/60 tabular-nums"
            data-turn-usage={messageId}
            data-turn-usage-degraded={resolved.hasTelemetry ? undefined : 'true'}
        >
            {resolved.line}
        </div>
    );
};

/**
 * Slim left-aligned divider rendered above an assistant turn whose request
 * lost the prompt cache: `────── ⊘ cache miss · N tokens` (TUI
 * CacheInvalidationMarkerComponent, 05 §7.3 baseline).
 */
export const CacheMissDivider: React.FC<{ reprocessedTokens: number }> = ({ reprocessedTokens }) => {
    const { t } = useI18n();
    return (
        <div
            className="my-2 flex items-center gap-2 text-xs text-muted-foreground/70"
            data-omp-cache-miss={reprocessedTokens}
        >
            <span className="h-px w-10 shrink-0 bg-border/60" aria-hidden="true" />
            <span className="whitespace-nowrap">
                ⊘ {t('chat.omp.cacheMiss.label')} ·{' '}
                {t('chat.omp.cacheMiss.tokens', { tokens: formatCompactTokenCount(reprocessedTokens) })}
            </span>
        </div>
    );
};
