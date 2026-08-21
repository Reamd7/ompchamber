/**
 * Pure decision helpers for the PWA install toast.
 *
 * Extracted from `usePwaInstallPrompt.ts` so the dedup decision can be
 * unit-tested without a DOM, storage, or React. The React surface remains the
 * sole owner of side effects (storage writes, `toast.info`, event listeners).
 * This module only answers the question "given these inputs, should we show
 * the toast?".
 *
 * Exposed for unit testing. Not part of the stable consumer surface.
 */

export interface PwaInstallToastDecisionInput {
  /** Persistent localStorage entry: `'true'` when the user dismissed once. */
  readonly dismissed: string | null;
  /** Session-scoped sessionStorage flag set the first time the toast is shown in this tab. */
  readonly sessionShown: string | null;
  /** Whether the current React effect already holds a toast id. */
  readonly hasActiveToast: boolean;
}

/**
 * Returns `true` if the PWA install prompt toast should be shown for the
 * incoming `beforeinstallprompt` event.
 *
 * The decision composes three gates (any failure short-circuits):
 *  1. Persistent dismissal wins for all future visits.
 *  2. Per-tab dedup avoids re-showing inside the same browsing session.
 *  3. Re-entrancy guard prevents stacking when the effect already owns one.
 */
export const shouldShowPwaInstallToast = (input: PwaInstallToastDecisionInput): boolean => {
  if (input.dismissed === 'true') return false;
  if (input.sessionShown === 'true') return false;
  if (input.hasActiveToast) return false;
  return true;
};
