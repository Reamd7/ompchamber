---
name: e2e-redgreen-playwright
description: Use when fixing OpenChamber UI bugs that require Playwright screenshot/video Red-Green repro and delivery — root-cause first, fingerprint assertions, dual-layer red-green, evidence packaging.
---

# Playwright Red-Green repro pipeline (OpenChamber)

Validated 2026-10-18 (session 01a14cf5, three bugs closed in one loop). Canonical toolkit: **this skill's `tools/`** (harness / bootstrap / tree / react-app / scenario.example / fixtures). Usage: **one task = one working dir** — copy the whole `tools/` kit into a fresh gitignored dir per task (e.g. `references/e2e-<task-slug>/`, needs `playwright-core` resolvable), plant the fixture command into the scratch project, write scenarios beside it. Two structural guarantees, enforced by harness.mjs at `newRun` time: it refuses to run the kit in place (under `.agents/skills`/`.claude/skills`) and refuses when the artifacts path is inside a git repo but not gitignored; every `newRun` also gets its own wiped `artifacts/<run-name>/` dir, so runs never share state. App-side facts (browser engine verdict, bootstrap pitfalls, dev runtime, verdict baselines) are canonically owned by the `ompchamber-web-e2e-playwright` skill.

## Pipeline (in order, no skipping)

1. **Root cause first**: get file:line root-cause hypotheses from parallel read-only scouts before writing any repro script. A Red script is an encoded hypothesis, not exploration.
2. **Fingerprint assertion**: one cheap-to-check fingerprint per bug, collapsing judgment to a single regex:
   - data-channel bugs: plant a marker command (`fixtures/e2e-echo.md` → `E2E-ECHO-OK [args]` pattern) — dropped arguments become visible at a glance;
   - gating bugs: filesystem sentinel — the target file must NOT exist before approval + the overlay text must exist;
   - interaction bugs: question → answer → model repeats it back (transcript round-trip assertion).
3. **Harness layering**: scenarios contain only "steps + fingerprint"; everything else comes from harness.mjs:
   - `newRun(name, {engine})`: per-run artifacts dir, `shot(step, note)` numbered screenshots, same-context video (renamed `recording.webm` on finish), `settle(sample)` stability polling (3 consecutive identical samples = turn settled; fixed sleeps are forbidden — LLM turn duration is unpredictable), `verdict(tag, ok)` greppable judgment constant.
   - Engine is a parameter, not a commitment: `engine: 'chromium' | 'obscura'`, switching engines never rewrites scenarios.
   - Fixture scratch project lives outside the repo, one level under the home dir (the Add-project dialog tree filters gitignored dirs).
   - Green re-run = `RUN_NAME=green-x node <same script>`; Red/Green comparison = artifacts dir comparison.
4. **Red criterion calibration** (Red is not done when written):
   - tighten the criterion when the actual failure is subtler than expected: the model guessed the marker from files with EMPTY args (`E2E-ECHO-OK []`) → criterion became "marker WITH args";
   - reorder steps when UI state blocks the flow: the mode selector is disabled in draft state → send a first message to start the session, then switch modes;
   - record variants when Red is broader than hypothesized and make the fix cover both (ask broken in BOTH the normal UI flow and headless-materialized-then-attached).
5. **Dual-layer Red-Green**: the E2E layer pins the user-visible contract (artifacts are the delivery evidence, gitignored); the unit-test layer pins the engine contract (e.g. `omp-host.redgreen.test.ts`), reuses the existing mock-SDK-then-dynamic-import seam, is written BEFORE the fix and fails first — it is the only test that enters the repo.
6. **Green**:
   - kill the old dev server before `bun run dev` — server code changes require a restart; Green must run on the new code.
   - multi-channel verification: screenshots via `read <png>?q=<specific judgment question>` (exploration asks decision questions, delivery narrates in the user's language — never open-ended "describe") + grep dev logs + read the engine session JSONL directly (`~/.omp/agent/sessions/-<proj>`). DOM screenshots can lie (optimistic UI); server-side sources of truth cannot.
   - a NEW integration bug surfaced during Green (e.g. missing `initThemeSync()` → `theme.status undefined`) must be fixed and ALL green runs re-run afterwards.
7. **Delivery**: REPORT.md with the engine-incompatibility evidence chain (if engines were switched), root causes with file:line, Red/Green screenshot references + per-step LLM descriptions.

## Negative-path rule (pruning)

When a drop-in browser engine shows **app-wide** interactivity death: run the minimal React control app first (`tools/react-app.mjs`, ~2 min) + byte-compare `document.documentElement.outerHTML.length` before/after a click. Control app works + real app byte-identical = engine compatibility gap → switch `engine:'chromium'` and document the evidence in REPORT. Do NOT start from app-side event semantics (isTrusted / visibilityState / React scheduler).

## Tools (`tools/`, take as needed)

- `harness.mjs`: scenario skeleton (newRun/shot/settle/verdict). Run with node, never bun — Bun's WS client cannot complete the obscura CDP handshake. Env knobs: `APP_URL`, `E2E_SCRATCH`, `CHROMIUM_BIN` (auto-probed system Chromium, falls back to Playwright's bundled), `OBSCURA_URL`. Guards: refuses in-place runs and non-gitignored artifacts paths (the per-task/gitignore guarantees above; this repo's root `.gitignore` has a global `artifacts/` rule, other repos do not). Video needs Playwright's ffmpeg: if recording fails, look up the ffmpeg revision in `node_modules/playwright-core/browsers.json` and symlink the system ffmpeg to `~/.cache/ms-playwright/ffmpeg-<rev>/ffmpeg-linux`.
- `bootstrap.mjs`: OpenChamber boot driver (Add-project dialog, scratch selection, new draft, composer locator). Scratch must live under the home dir — it throws otherwise.
- `tree.mjs`: DOM structure dump (tag.class[role] + rects + text) — a screenshot tells you WHAT is on screen, this tells you HOW to address it.
- `react-app.mjs`: minimal React 19 control app, the decisive experiment of the negative-path rule.
- `scenario.example.mjs`: fingerprint-assertion scenario template (bootstrap → numbered shots → settle → verdict, RUN_NAME Green re-run).
- `fixtures/e2e-echo.md`: marker command to plant as `.opencode/commands/e2e-echo.md` in the scratch project.
