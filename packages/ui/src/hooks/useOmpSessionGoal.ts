/**
 * useOmpSessionGoal — the goal indicator's read model (spec 02 §5.6 P1 /
 * GAP-B08).
 *
 * The event stream (`omp.goal.updated` via useOmpSessionStore) is the live
 * authority. When the store has no answer yet — fresh mount, idle session,
 * events not yet replayed — one authoritative `GET /api/omp/sessions/{id}/mode`
 * seeds the record from the snapshot's `goal` field. A failed or absent seed
 * reads as no goal (null); it never fabricates one.
 *
 * P1 is a pure projection: the bridge exposes no goal create/update/clear
 * endpoints yet (domain-modes.js registers none), so this hook is read-only
 * by design — the composer's "Craft a Goal" starter stays a magic prompt.
 */

import React from 'react';
import { z } from 'zod';

import { useRuntimeAPIs } from '@/hooks/useRuntimeAPIs';
import { useOmpGoalState } from '@/sync/useOmpSessionStore';

/** Goal fields the indicator renders (SDK goals/state.ts `Goal`). */
export interface OmpSessionGoalRecord {
  status?: string;
  objective?: string;
  tokenBudget?: number;
  tokensUsed?: number;
  timeUsedSeconds?: number;
}

const GoalRecordSchema = z.looseObject({
  status: z.string().optional(),
  objective: z.string().optional(),
  tokenBudget: z.number().optional(),
  tokensUsed: z.number().optional(),
  timeUsedSeconds: z.number().optional(),
});

/** GET …/mode snapshot's `goal` passthrough (loose snapshot keeps unknown keys). */
const SnapshotGoalSchema = z.object({ goal: z.unknown().optional() });

/** Defensive parse at the boundary — `goal` arrives as `unknown` off the wire. */
export const parseOmpGoalRecord = (value: unknown): OmpSessionGoalRecord | null => {
  const parsed = GoalRecordSchema.safeParse(value);
  return parsed.success ? parsed.data : null;
};

export const useOmpSessionGoal = (
  directory: string | null | undefined,
  sessionID: string | null | undefined,
  modesEnabled: boolean,
): OmpSessionGoalRecord | null => {
  // TUI parity (interactive-mode.ts goal_updated): the freshest goal rides
  // GoalModeState.goal on every goal-mode commit (incl. drops/pauses);
  // the top-level payload.goal is the fallback when no state arrived.
  const storeRecord = useOmpGoalState(directory ?? '', sessionID ?? undefined);
  const storeGoal = storeRecord?.state?.goal ?? storeRecord?.goal ?? null;
  const storeHasGoal = storeGoal !== null && parseOmpGoalRecord(storeGoal) !== null;
  const { ompModes } = useRuntimeAPIs();
  const [seeded, setSeeded] = React.useState<OmpSessionGoalRecord | null>(null);
  const seededKeyRef = React.useRef<string | null>(null);

  const directoryKey = directory ?? null;
  const activeSessionID = sessionID ?? null;
  const seedKey = modesEnabled && activeSessionID !== null && !storeHasGoal
    ? `${directoryKey ?? ''}::${activeSessionID}`
    : null;

  React.useEffect(() => {
    if (seedKey === null) {
      setSeeded(null);
      seededKeyRef.current = null;
      return;
    }
    if (seededKeyRef.current === seedKey) return;
    if (!directoryKey || activeSessionID === null) return;
    seededKeyRef.current = seedKey;
    let cancelled = false;
    void ompModes.getMode(activeSessionID, { directory: directoryKey }).then((result) => {
      if (cancelled) return;
      const snapshot = SnapshotGoalSchema.safeParse(result.ok ? result.data : null);
      setSeeded(snapshot.success ? parseOmpGoalRecord(snapshot.data.goal ?? null) : null);
    });
    return () => {
      cancelled = true;
    };
  }, [ompModes, seedKey, activeSessionID, directoryKey]);

  // A real store answer always outranks the seed.
  if (storeHasGoal) return parseOmpGoalRecord(storeGoal);
  return seeded;
};
