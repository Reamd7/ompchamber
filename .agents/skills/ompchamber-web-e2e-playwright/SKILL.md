---
name: ompchamber-web-e2e-playwright
description: Use when reproducing or verifying OpenChamber web UI behavior end-to-end with Playwright (real model turns, screenshots, video), including which browser engine works and the bootstrap pitfalls.
---

# OpenChamber Web UI E2E with Playwright

End-to-end flow for reproducing/verifying OpenChamber web UI behavior (real model turns, numbered screenshots, video). This skill is the canonical owner of app-side facts: engine verdict, bootstrap pitfalls, dev runtime, verdict baselines. Methodology and the toolkit live in the `e2e-redgreen-playwright` skill.

## Browser engine choice (read first)

- **Use system Chromium**: `chromium.launch({ executablePath: CHROMIUM_BIN, headless: true })` — harness.mjs auto-probes common system paths (`CHROMIUM_BIN` overrides).
- **obscura 0.2.5 cannot be used for interactive repro of this app** (validated 2026-10-18): the app shell/DOM/network chain loads fine, but the interactive layer is fully inert — DOM byte-identical before/after clicks and keyboard, zero React commits; a minimal React 19 app works in the same instance (engine compatibility gap for this app's boot path; evidence in `references/e2e` REPORT §0 of that session). obscura specifics if you still smoke it: run `obscura serve --port 9223 --allow-private-network`, Playwright `connectOverCDP`; its CDP `--v8-flags` is a global arg that must precede the subcommand; Playwright must run under node (Bun's WS client hangs on the obscura handshake).
- Video needs Playwright's ffmpeg: look up the ffmpeg revision in `node_modules/playwright-core/browsers.json` and symlink the system ffmpeg to `~/.cache/ms-playwright/ffmpeg-<rev>/ffmpeg-linux` if missing (this symlink has silently disappeared before — check it first when recording fails).

## Runtime

1. Repo root `bun run dev` → UI http://127.0.0.1:5180, API http://127.0.0.1:3902 (server code changes require a restart).
2. The scratch project lives **one level under the home dir** (e.g. `~/omp-e2e-proj`, containing `.opencode/commands/*.md` test commands): the Add-project dialog's directory tree filters gitignored dirs, so a scratch under `references/` is not selectable.

## Bootstrap pitfalls (fresh contexts hit these every time)

- Boot always opens the "Add project directory" dialog (fullscreen overlay, blocks all clicks): walk `[role=option]` level by level to the scratch dir → click the "add project" button (not "Add"); re-adding is a no-op and Escape alone often fails to dismiss — loop Escape + `[data-slot=dialog-close]` until gone.
- The "New session" button is inside `aside` (not the hover "+" on session rows); selecting a command = inserts text and submits immediately.
- The composer is a `contenteditable` div, not a textarea (locate via `[role=textbox]`).
- The mode selector is the "Default" chip in the composer footer; **disabled in draft state** (hint: "Start the session to change its mode") — send a message to start the session first, then switch modes.
- The plan-review overlay and ask dialogs render in a portal (`[role=dialog]`), **not inside `<main>`**: transcript assertions using main.innerText miss them — query `[role=dialog]` separately.
- The ask dialog is a radio option list (not a text input): click an option label, then submit.
- Waiting for turn end: poll transcriptText until 3 consecutive identical samples (not a fixed sleep).

## Scenario scripts and toolkit

The canonical toolkit is the `e2e-redgreen-playwright` skill's `tools/` (harness.mjs: newRun with video + numbered shots + settle + verdict; bootstrap.mjs: the pitfalls above, codified; tree.mjs: DOM structure dump; react-app.mjs: engine control experiment; scenario.example.mjs: fingerprint-assertion template; fixtures/e2e-echo.md: marker command). Copy it into a gitignored working dir to use it. The `references/e2e/` instance from the 2026-10-18 session holds all scenarios and is a disposable copy: red1-slash (slash command channel, criterion `E2E-ECHO-OK [args]`), red2-plan (full plan chain: plan → proposal → overlay → approve → file lands), red3-ask (ask dual variant: normal UI flow vs headless materialization then UI attach, API POST /session + prompt_async first). Green re-run: `RUN_NAME=green1-slash node red1-slash.mjs`.

## Verdict baselines

- slash Green: user bubble = expanded template, answer `E2E-ECHO-OK [hello world]`; Red: bubble degrades to the bare command name + empty args.
- plan Green: NOTES.md only lands after approve; Red: file written directly under the Plan badge.
- ask Green: both variants show the dialog and the answer round-trips; Red: BOTH-BROKEN (the model itself reports no ask tool).
