import React from 'react';
import type { LegendListRef } from '@legendapp/list/react';

// Extra settle time after the last scroll or data change before the viewport is
// judged: a jump that is still filling its draw distance must not be nudged.
const BLANK_SETTLE_MS = 350;
// A recompute either restores the window or it does not; repeating it forever
// would turn a broken list into a scroll fight.
const MAX_NUDGES_PER_EPISODE = 2;

interface UseTimelineWindowRecoveryOptions {
    listRef: React.RefObject<LegendListRef | null>;
    /** Identity of the mounted transcript: a new session starts a new episode. */
    sessionKey: string;
    entryCount: number;
    enabled: boolean;
}

const hasRowInViewport = (scrollNode: HTMLElement): boolean => {
    const rows = scrollNode.querySelectorAll('[data-turn-id], [data-message-id]');
    if (rows.length === 0) {
        return false;
    }
    const viewport = scrollNode.getBoundingClientRect();
    for (const row of rows) {
        const rect = row.getBoundingClientRect();
        if (rect.bottom > viewport.top && rect.top < viewport.bottom) {
            return true;
        }
    }
    return false;
};

/**
 * Keeps a virtualized transcript from staying blank.
 *
 * The list can settle with a viewport that falls outside every measured range —
 * long jumps and measurement resets are the documented sources, and nothing
 * re-fills the range until the next data change, so the chat column simply
 * stays white. Re-issuing a non-animated scroll to the offset the reader is
 * already at makes the list recompute the range it should be showing without
 * moving them; rows that are already visible leave it alone entirely.
 */
export const useTimelineWindowRecovery = ({
    listRef,
    sessionKey,
    entryCount,
    enabled,
}: UseTimelineWindowRecoveryOptions): { attachScrollNode: (node: HTMLElement | null) => void } => {
    const stateRef = React.useRef({ enabled, entryCount });
    const timerRef = React.useRef<ReturnType<typeof setTimeout> | null>(null);
    const scrollNodeRef = React.useRef<HTMLElement | null>(null);
    const nudgesRef = React.useRef(0);

    React.useEffect(() => {
        stateRef.current = { enabled, entryCount };
    }, [enabled, entryCount]);

    const clearTimer = React.useCallback(() => {
        if (timerRef.current !== null) {
            clearTimeout(timerRef.current);
            timerRef.current = null;
        }
    }, []);

    const schedule = React.useCallback((delayMs = BLANK_SETTLE_MS) => {
        const { enabled: isEnabled, entryCount: count } = stateRef.current;
        if (!isEnabled || count === 0) {
            clearTimer();
            return;
        }
        clearTimer();
        timerRef.current = setTimeout(() => {
            timerRef.current = null;
            const { enabled: stillEnabled, entryCount: stillCount } = stateRef.current;
            if (!stillEnabled || stillCount === 0) {
                return;
            }

            const list = listRef.current;
            const node = scrollNodeRef.current;
            if (!list || !node) {
                return;
            }
            if (hasRowInViewport(node)) {
                nudgesRef.current = 0;
                return;
            }
            if (nudgesRef.current >= MAX_NUDGES_PER_EPISODE) {
                return;
            }

            nudgesRef.current += 1;
            // Same offset: recompute the range, do not move the reader.
            void list.scrollToOffset({ offset: node.scrollTop, animated: false });
            schedule();
        }, delayMs);
    }, [clearTimer, listRef]);

    const handleScroll = React.useCallback(() => {
        schedule();
    }, [schedule]);

    const attachScrollNode = React.useCallback((node: HTMLElement | null) => {
        const previous = scrollNodeRef.current;
        if (previous === node) {
            return;
        }
        if (previous) {
            previous.removeEventListener('scroll', handleScroll);
        }
        scrollNodeRef.current = node;
        if (node) {
            node.addEventListener('scroll', handleScroll, { passive: true });
        }
    }, [handleScroll]);

    // A new transcript (or new rows) can land the viewport in a blank range
    // right away: check once the change has settled.
    React.useEffect(() => {
        nudgesRef.current = 0;
        if (!enabled || entryCount === 0) {
            clearTimer();
            return;
        }
        schedule();
    }, [clearTimer, enabled, entryCount, schedule, sessionKey]);

    React.useEffect(() => () => {
        clearTimer();
        if (scrollNodeRef.current) {
            scrollNodeRef.current.removeEventListener('scroll', handleScroll);
            scrollNodeRef.current = null;
        }
    }, [clearTimer, handleScroll]);

    return { attachScrollNode };
};
