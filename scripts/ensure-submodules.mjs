#!/usr/bin/env node
/**
 * Ensure the ghostty-web submodule is present before dependency resolution.
 *
 * The workspace depends on the vendored fork via `file:references/ghostty-web`.
 * On a fresh clone the submodule directory is empty, and `bun install` would
 * fail (or silently resolve nothing) before this hook runs — so `bun install`
 * must first trigger this via the root `preinstall` script. `git submodule
 * update --init` is idempotent: an already-initialized submodule is a no-op.
 */
import { spawnSync } from 'node:child_process';
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const marker = path.join(root, 'references', 'ghostty-web', 'package.json');

if (fs.existsSync(marker)) {
  process.exit(0);
}

const result = spawnSync(
  'git',
  ['submodule', 'update', '--init', '--recursive', 'references/ghostty-web'],
  { cwd: root, stdio: 'inherit' },
);

if (result.status !== 0) {
  console.error(
    '[ensure-submodules] failed to initialize references/ghostty-web. ' +
      'Run manually: git submodule update --init --recursive',
  );
  process.exit(result.status ?? 1);
}
