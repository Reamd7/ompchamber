// ColdTranscriptReader (docs/plan.md §7, phase 3).
//
// Cold GET paths (session info, message history, telemetry, structured
// entries, transcript context, entry tree) must not leave a writable
// SessionManager behind. The SDK's writer is lazy (opened on first append),
// so an open→read→close manager is OS-level read-only; the residual cost is
// the in-memory `#entries` mirror, which `releaseRetainedEntries()` drops.
// Every cold read therefore runs through `withColdManager`, which:
//   - closes the manager in `finally` (flush+writer teardown),
//   - releases the retained entries mirror in `finally`,
//   - counts opens/closes/releases so tests can prove no cold GET leaks a
//     manager or its retained entry mirror.
import { SessionManager, loadSessionMessagesReadOnly } from '@oh-my-pi/pi-coding-agent';

/** Cold readers consume the SDK SessionManager surface structurally. */
export type ColdManager = Awaited<ReturnType<typeof SessionManager.open>>;
/** Observation counters (plan §9.1: O(1) numbers, never content). */
interface ColdReaderStats {
  opens: number;
  closes: number;
  releases: number;
  failedCloses: number;
}

const stats: ColdReaderStats = {
  opens: 0,
  closes: 0,
  releases: 0,
  failedCloses: 0,
};

export const coldReaderStats = (): ColdReaderStats => ({ ...stats });

/** Test seam: reset counters between suites. */
export const resetColdReaderStats = (): void => {
  stats.opens = 0;
  stats.closes = 0;
  stats.releases = 0;
  stats.failedCloses = 0;
};

/**
 * Open a cold manager, run `consume`, and guarantee release: close (flush +
 * writer teardown) and releaseRetainedEntries (drop the in-memory entry
 * mirror) both run in `finally` on every path, including consume throws
 * (plan §7.1). Returns consume's value.
 *
 * Order is contractual: the SDK documents releaseRetainedEntries as
 * "only call from session dispose, after the final close()" — it seals the
 * manager and drops pending writes, so close must run first.
 */
export async function withColdManager<T>(
  filePath: string,
  consume: (manager: ColdManager) => Promise<T> | T,
): Promise<T> {
  stats.opens += 1;
  const manager = await SessionManager.open(filePath);
  try {
    return await consume(manager);
  } finally {
    try {
      await manager.close();
      stats.closes += 1;
    } catch {
      stats.failedCloses += 1;
    }
    try {
      manager.releaseRetainedEntries();
      stats.releases += 1;
    } catch {
      // Release is best-effort; the manager is discarded regardless.
    }
  }
}

// Note: the SDK's `loadSessionMessagesReadOnly` (no manager at all) is the
// labeled full-materialization fallback for message-only consumers. The
// engine's message-history arm needs entries too (turn-state stampers and
// dividers), so it uses withColdManager; a message-only route should call
// the SDK loader directly and release its result promptly (plan §7.1).
