#!/usr/bin/env node
/**
 * Point git at the in-repo hooks (scripts/hooks) via core.hooksPath.
 *
 * Runs from the root postinstall so every checkout — main repo and every
 * worktree — gets the guards automatically. Relative hooksPath resolves
 * against each worktree's own top level, so worktrees pick up their own
 * checked-out copy. Fails soft (warning, exit 0) where git is unavailable
 * so CI/install flows never break on this.
 */
import { execFileSync } from 'node:child_process';
import { existsSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const hooksDir = path.join(root, 'scripts', 'hooks');

if (!existsSync(path.join(hooksDir, 'pre-commit')) || !existsSync(path.join(hooksDir, 'pre-push'))) {
  console.warn('[git-hooks] scripts/hooks is incomplete; skipping installation.');
  process.exit(0);
}

try {
  const current = execFileSync('git', ['config', 'core.hooksPath'], { cwd: root, encoding: 'utf8' }).trim();
  if (current === 'scripts/hooks') {
    console.log('[git-hooks] core.hooksPath already set to scripts/hooks.');
    process.exit(0);
  }
  if (current) {
    console.warn(`[git-hooks] core.hooksPath is already "${current}"; leaving it untouched.`);
    process.exit(0);
  }
} catch {
  /* unset — proceed to install */
}

try {
  execFileSync('git', ['config', 'core.hooksPath', 'scripts/hooks'], { cwd: root });
  console.log('[git-hooks] installed: core.hooksPath=scripts/hooks');
} catch (error) {
  console.warn(`[git-hooks] could not set core.hooksPath (${String(error).split('\n')[0]}); skipping.`);
}
