---
title: Git review and richer file previews
---

## App

### New

- **Git: review published pull requests in the Changes tab and act on each diff hunk.** A PR's diff opens as a review with the changes tree readable as a tree or as a file tree showing one file at a time; each hunk carries inline actions to stage, unstage, discard, or apply just that hunk (thanks to @LABCAT).
- **Files: the viewer now previews what agents produce.** Audio and video play with seeking, fonts show a specimen, CSV/TSV render as tables, Mermaid and SVG render, images get fit and 1:1 controls, and anything else gets a binary card with type, size, and download; Markdown previews resolve relative images and links.
- Mobile: compare two branches or two commits side by side from the sessions drawer, start a session in a project right from the drawer, and close drawers with a reverse edge swipe; a swipe on a session copies its ID (thanks to @claymor333).
- Mobile: the About screen is back and checks for updates of the connected server, and OpenCode's settings pages open from a phone.
- Sessions: rename a session with AI from its recent conversation turns, and find sessions by exact ID in the sidebar and the archive (thanks to @yulia-ivashko).
- Sessions: an opt-in retention setting cleans up archived sessions past their retention window.
- Sidebar: sessions where the agent left a next step open are marked, as are projects whose folder is missing.
- Usage: the usage page tracks ClinePass (thanks to @NemeZZiZZ) and Charm Hyper (thanks to @airtaxi) quotas, with exe.dev usage tracking and its provider logo.
- Settings/Providers: custom provider models can configure reasoning levels, and OpenCode Go sign-in goes through the Console.
- Projects: pick a default agent per project and keep model selections (thanks to @MltStephane); the new-session project picker has search (thanks to @maximtop).
- Worktrees: creating a worktree from a remote branch fetches that branch first (thanks to @jtatum).
- Chat: `/fork <text>` forks the session after its last completed turn and sends the text to the fork.

### Improvements

- Files: upload from the folder menu and the tree toolbar; the file tree persists its expansion and width across restarts with an editor toggle (thanks to @cmdaltctr).
- Files: switching between preview views keeps the scroll position and cursor.
- Diff: collapsed-context separators show a loading state, and PR comparisons can expand collapsed context in full.
- Review: new review sessions inherit the permission auto-accept setting.
- Settings: the third-party plugin integrations are retired — the Claude Code and Cursor install cards are gone, and the page keeps GitHub and Linear.
- Settings: a "Restart OpenCode" action (button and command palette) applies config changes without hunting for a restart.
- UI: the keyboard shortcuts dialog lays out in two columns, sticky headers crossfade instead of jumping, a preference keeps scrollbars always visible, and the Fixel font is available.

## VS Code

### New

- Multi-run: the Agent Manager is replaced by the shared multi-run. Run rows live in the sidebar with a run overview over the chat, the "Run on Several Models" command opens an editor tab with a parallel draft, and run lanes no longer sit on "Loading session".
- The prompt navigator is on by default, with its rail, toggle, and shortcut.
- Turkish localization for extension strings and the loading splash (thanks to @fitzgpt).

### Improvements

- Sidebar: sessions can be found by exact ID (thanks to @yulia-ivashko), and sessions where the agent left a next step open are marked.
- Archive: an opt-in retention setting cleans up archived sessions past their retention window.
- Usage: the usage page tracks ClinePass (thanks to @NemeZZiZZ) and Charm Hyper (thanks to @airtaxi) quotas, with exe.dev usage tracking.
