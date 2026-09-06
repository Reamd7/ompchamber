import { useUIStore } from '@/stores/useUIStore';
import { useSessionUIStore } from '@/sync/session-ui-store';
import { isEmbeddedSessionChat } from '@/components/layout/contextPanelEmbeddedChat';
import { isVSCodeRuntime } from '@/lib/desktop';

/**
 * Scroll the chat to the task-card row that dispatched one run — only when
 * the card is already rendered (maintainer ruling: no history loading to
 * hunt for it; unrendered cards simply don't scroll). Pin centered for two
 * seconds against scroll re-anchoring with a brief highlight ring.
 */
export const revealRunCard = async (runId: string): Promise<boolean> => {
  const deadline = Date.now() + 2000;
  const findCard = () => document.querySelector<HTMLElement>(`[data-run-id="${CSS.escape(runId)}"]`);
  while (Date.now() < deadline) {
    const el = findCard();
    if (el) {
      el.scrollIntoView({ block: 'center' });
      el.style.transition = 'box-shadow 0.8s ease';
      el.style.boxShadow = '0 0 0 3px var(--primary)';
      window.setTimeout(() => {
        el.style.boxShadow = '';
      }, 1800);
      const pinUntil = Date.now() + 2000;
      const pin = () => {
        if (Date.now() > pinUntil) return;
        const live = findCard() ?? el;
        const rect = live.getBoundingClientRect();
        if (rect.top < 60 || rect.top > window.innerHeight * 0.75) {
          live.scrollIntoView({ block: 'center' });
        }
        window.requestAnimationFrame(pin);
      };
      window.requestAnimationFrame(pin);
      return true;
    }
    const { promise: nap, resolve: napDone } = Promise.withResolvers<void>();
    window.setTimeout(napDone, 100);
    await nap;
  }
  return false;
};

/**
 * Canonical subagent-run open — ONE behavior for every entry point
 * (sidebar child row, task-card run row, work-status subagent row):
 * desktop shows the dispatch site (main chat scrolls to the task card in
 * the parent session when rendered) plus the run's read-only child-session
 * chat in the side panel; constrained surfaces (embedded session chat,
 * mobile, VS Code) navigate to the child session in place.
 */
export const openSubagentRun = (options: {
  childSessionID: string;
  parentSessionID: string;
  runId: string;
  label: string;
  directory: string;
}): void => {
  const { childSessionID, parentSessionID, runId, label, directory } = options;
  const ui = useUIStore.getState();
  if (isEmbeddedSessionChat() || ui.isMobile === true || isVSCodeRuntime()) {
    useSessionUIStore.getState().setCurrentSession(childSessionID, directory);
    return;
  }
  const sessionUi = useSessionUIStore.getState();
  // Switch the main pane to the parent only when it is not already current —
  // a redundant switch would re-anchor the chat and fight the card reveal.
  if (sessionUi.currentSessionId !== parentSessionID) {
    sessionUi.setCurrentSession(parentSessionID, directory);
  }
  void revealRunCard(runId);
  ui.openContextPanelTab(directory, {
    mode: 'chat',
    dedupeKey: `session:${childSessionID}`,
    label,
    readOnly: true,
  });
};
