/**
 * Initial-value precedence for the Behavior page's global-instructions
 * editor. The AGENTS.md file is authoritative once it exists: edits made
 * outside the page (TUI, text editor, profile switch) must surface in the
 * editor instead of the settings.json copy. The copy only seeds the editor
 * while the file has never been created (fresh installs, pre-migration
 * upgrades).
 */
export const resolveInitialPrompt = (
  storedCopy: string | undefined,
  agentsMd: { exists: boolean; content: string } | null,
): string => (
  agentsMd?.exists ? agentsMd.content : (storedCopy ?? '')
);
