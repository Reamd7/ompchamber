// Shared Playwright harness for Red/Green E2E repro.
// Usage model: copy this tools/ kit into a gitignored working dir (e.g.
// references/e2e/) and write scenarios beside it — artifacts land in ./artifacts/.
// Run scenarios with node, NEVER bun: Bun's WebSocket client cannot complete
// the obscura CDP handshake (Playwright connectOverCDP hangs).
//
// Env knobs: APP_URL, E2E_SCRATCH, CHROMIUM_BIN, OBSCURA_URL.
import { chromium } from 'playwright-core';
import { spawnSync } from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const HERE = path.dirname(fileURLToPath(import.meta.url));
export const ARTIFACTS = path.join(HERE, 'artifacts');
export const APP_URL = process.env.APP_URL ?? 'http://127.0.0.1:5180';
export const SCRATCH = process.env.E2E_SCRATCH ?? path.join(os.homedir(), 'omp-e2e-proj');
export const OBSCURA_URL = process.env.OBSCURA_URL ?? 'ws://127.0.0.1:9223/devtools/browser';

// System Chromium preferred (no download); fall back to whatever Playwright
// itself has installed when no system binary exists.
function resolveChromium() {
  if (process.env.CHROMIUM_BIN) return process.env.CHROMIUM_BIN;
  for (const p of [
    '/usr/bin/chromium', '/usr/bin/chromium-browser',
    '/usr/bin/google-chrome', '/usr/bin/google-chrome-stable',
  ]) {
    if (fs.existsSync(p)) return p;
  }
  return undefined;
}
export const CHROMIUM_BIN = resolveChromium();

// Structural guarantees: (1) the kit must be COPIED out before running — one
// gitignored working dir per task (e.g. references/e2e-<task-slug>/), so
// scenarios and artifacts stay wholly task-scoped and disposable; (2)
// recordings/screenshots must never land in git-tracked territory — passes
// when the artifacts dir is outside any git repo or gitignored, throws
// otherwise (other repos lack this repo's global `artifacts/` ignore rule).
function assertArtifactsSafe(artifactsDir) {
  if (/[\\/].agents[\\/]skills[\\/]/.test(HERE) || /[\\/].claude[\\/]skills[\\/]/.test(HERE)) {
    throw new Error(
      'refusing to run the kit in place — copy this tools/ dir into a fresh gitignored working dir for the task ' +
      '(e.g. references/e2e-<task-slug>/) and run scenarios from there.',
    );
  }
  // Run git from the kit dir (always exists); the artifacts dir itself may not
  // exist yet, and `git -C` on a nonexistent cwd fails as "not a repo".
  const inRepo = spawnSync('git', ['-C', HERE, 'rev-parse', '--show-toplevel'], { encoding: 'utf8' });
  if (inRepo.status !== 0) return; // not a git repo — nothing to pollute
  const ignored = spawnSync('git', ['-C', HERE, 'check-ignore', '-q', artifactsDir], { encoding: 'utf8' });
  if (ignored.status !== 0) {
    throw new Error(
      `artifacts dir ${artifactsDir} is inside a git repo but NOT gitignored — recordings and screenshots would pollute the worktree. ` +
      'Copy this tools/ kit into a gitignored per-task working dir (e.g. references/e2e-<task-slug>/) and run scenarios from there.',
    );
  }
}

export async function newRun(name, { engine = 'chromium', video = true } = {}) {
  const dir = path.join(ARTIFACTS, name);
  assertArtifactsSafe(dir);
  fs.rmSync(dir, { recursive: true, force: true });
  fs.mkdirSync(dir, { recursive: true });

  // Engine is a parameter, not a commitment: obscura works for static smoke
  // only (interactive layer inert for OpenChamber web — see
  // ompchamber-web-e2e-playwright skill); chromium is the default for Red/Green.
  let browser;
  if (engine === 'obscura') {
    browser = await chromium.connectOverCDP({ endpointURL: OBSCURA_URL, timeout: 20000 });
  } else {
    const launchOpts = { headless: true };
    if (CHROMIUM_BIN) launchOpts.executablePath = CHROMIUM_BIN;
    browser = await chromium.launch(launchOpts);
  }

  const ctxOpts = { viewport: { width: 1440, height: 1000 } };
  if (video) ctxOpts.recordVideo = { dir, size: { width: 1440, height: 1000 } };
  const context = await browser.newContext(ctxOpts);
  const page = await context.newPage();
  page.on('pageerror', (e) => console.log(`[${name}] pageerror:`, e.message.slice(0, 200)));
  const shots = [];
  const shot = async (step, note) => {
    const file = `${dir}/${step}.png`;
    await page.screenshot({ path: file });
    shots.push({ step, note, file });
    console.log(`[${name}] shot ${step}${note ? ' — ' + note : ''}`);
  };
  const log = (...a) => console.log(`[${name}]`, ...a);
  // Wait until a sampled value (e.g. transcript text) stays identical across
  // consecutive polls — LLM turn duration is unpredictable, fixed sleeps flake.
  const settle = async (sample, { interval = 3000, stable = 3, tries = 60 } = {}) => {
    let last = '', same = 0;
    for (let i = 0; i < tries && same < stable; i++) {
      await page.waitForTimeout(interval);
      const v = await sample();
      if (v === last) same++;
      else { same = 0; last = v; }
    }
    return last;
  };
  return {
    browser, context, page, dir, shot, shots, log, settle,
    url: (p) => path.join(dir, p),
    async finish() {
      await page.close();
      await context.close();
      if (engine !== 'obscura') await browser.close();
      // Video file is named lazily on context close — rename to a stable name.
      for (const f of fs.readdirSync(dir)) {
        if (f.endsWith('.webm')) fs.renameSync(path.join(dir, f), path.join(dir, 'recording.webm'));
      }
      return dir;
    },
  };
}

/** Print a greppable verdict constant so Red/Green judgment lives in tool output. */
export function verdict(tag, ok, detail) {
  console.log(`${tag}-VERDICT: ${ok ? 'PASS' : 'FAIL'}${detail ? ' — ' + detail : ''}`);
}
