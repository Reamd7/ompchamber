#!/usr/bin/env node
// `worktree init` — create a parallel git worktree with isolated dev ports.
//
// Usage:
//   bun run worktree init <name> [--branch <b>] [--base <ref>] [--json] [--quiet]
//
// Worktrees live under .worktrees/<name> (gitignored). Each one persists a
// port pair in .dev-ports.json that scripts/dev-web-hmr.mjs prefers over the
// shared defaults, so `bun run dev` in every worktree binds its own UI/API
// ports and parallel checkouts never collide.

import { spawnSync } from 'node:child_process';
import fs, { realpathSync } from 'node:fs';
import { pathToFileURL } from 'node:url';
import path from 'node:path';
import { parseArgs } from 'node:util';
import * as clack from '@clack/prompts';
import { allocateDevPorts, writeDevPorts, WORKTREES_DIRNAME } from './worktree-ports.mjs';

const NAME_PATTERN = /^[A-Za-z0-9][A-Za-z0-9._-]*$/;

export function validateWorktreeName(name) {
  if (typeof name !== 'string' || name.length === 0) {
    return { ok: false, reason: 'name is required' };
  }
  if (name === '.' || name === '..' || name.includes('/') || name.includes('\\')) {
    return { ok: false, reason: `invalid name "${name}"` };
  }
  if (!NAME_PATTERN.test(name)) {
    return { ok: false, reason: `invalid name "${name}" (use letters, digits, ".", "-", "_")` };
  }
  return { ok: true };
}

function git(args, options = {}) {
  return spawnSync('git', args, { encoding: 'utf8', ...options });
}

function fail(message, code = 1) {
  process.exitCode = code;
  return { error: message };
}

async function main() {
  const { values, positionals } = parseArgs({
    allowPositionals: true,
    args: process.argv.slice(2),
    options: {
      branch: { type: 'string' },
      base: { type: 'string' },
      json: { type: 'boolean' },
      quiet: { type: 'boolean' },
    },
  });
  const jsonMode = values.json === true;
  const quietMode = values.quiet === true;
  const interactive = process.stdout.isTTY === true && !jsonMode && !quietMode;
  const emit = (payload) => {
    if (jsonMode) {
      process.stdout.write(`${JSON.stringify(payload)}\n`);
      return;
    }
    if (payload.error) {
      process.stderr.write(`${payload.error}\n`);
      return;
    }
    if (quietMode) {
      process.stdout.write(`${payload.quiet ?? ''}\n`);
      return;
    }
    clack.log.success(payload.human ?? '');
  };

  const [command, positionalName] = positionals;
  if (command !== 'init') {
    emit(fail(
      'usage: bun run worktree init <name> [--branch <b>] [--base <ref>] [--json] [--quiet]',
      2,
    ));
    process.exit(2);
  }

  // ── Policy checks (before any prompt or side effect) ────────────────────
  const repoRootResult = git(['rev-parse', '--show-toplevel']);
  if (repoRootResult.status !== 0) {
    emit(fail('not inside a git repository'));
    process.exit(1);
  }
  const repoRoot = repoRootResult.stdout.trim();
  if (repoRoot.includes(`/${WORKTREES_DIRNAME}/`)) {
    emit(fail(`run from the main checkout, not from inside ${WORKTREES_DIRNAME}/`));
    process.exit(1);
  }

  let name = positionalName;
  if (!name && interactive) {
    name = await clack.text({
      message: 'Worktree name (branch and directory under .worktrees/):',
      validate: (value) => {
        const check = validateWorktreeName(String(value ?? '').trim());
        return check.ok ? undefined : check.reason;
      },
    });
    if (clack.isCancel(name)) {
      clack.cancel('aborted');
      process.exit(1);
    }
  }
  name = String(name ?? '').trim();

  const nameCheck = validateWorktreeName(name);
  if (!nameCheck.ok) {
    emit(fail(nameCheck.reason, 2));
    process.exit(2);
  }

  const branch = values.branch ?? name;
  const base = values.base ?? 'HEAD';
  const worktreePath = path.join(repoRoot, WORKTREES_DIRNAME, name);

  if (fs.existsSync(worktreePath)) {
    emit(fail(`${worktreePath} already exists`, 2));
    process.exit(2);
  }
  if (git(['show-ref', '--verify', `refs/heads/${branch}`]).status === 0) {
    emit(fail(`branch "${branch}" already exists (pick another with --branch)`, 2));
    process.exit(2);
  }
  if (git(['rev-parse', '--verify', `${base}^{commit}`]).status !== 0) {
    emit(fail(`base ref "${base}" not found`, 2));
    process.exit(2);
  }

  const ports = await allocateDevPorts({ repoRoot });
  if (!ports) {
    emit(fail('no free port pair found above the defaults; too many worktrees?', 1));
    process.exit(1);
  }

  if (interactive) clack.intro(`worktree init ${name}`);

  // ── Create worktree, then install; roll both back on failure ────────────
  const addResult = git(['worktree', 'add', '-b', branch, worktreePath, base], {
    stdio: interactive ? 'inherit' : 'pipe',
  });
  if (addResult.status !== 0) {
    emit(fail(`git worktree add failed: ${(addResult.stderr ?? '').trim()}`));
    process.exit(1);
  }
  writeDevPorts(worktreePath, ports);

  const installResult = spawnSync('bun', ['install'], {
    cwd: worktreePath,
    stdio: interactive ? 'inherit' : 'pipe',
    encoding: 'utf8',
  });
  if (installResult.status !== 0) {
    git(['worktree', 'remove', '--force', worktreePath]);
    git(['branch', '-D', branch]);
    emit(fail(`bun install failed (worktree and branch rolled back): ${(installResult.stderr ?? '').trim()}`));
    process.exit(1);
  }

  const relative = path.relative(repoRoot, worktreePath);
  emit({
    name,
    branch,
    base,
    path: worktreePath,
    uiPort: ports.uiPort,
    apiPort: ports.apiPort,
    human: [
      `worktree ${relative} ready (branch ${branch})`,
      `ports: UI ${ports.uiPort} / API ${ports.apiPort} → ${WORKTREES_DIRNAME}/${name}/.dev-ports.json`,
      `next: cd ${relative} && bun run dev`,
    ].join('\n'),
    quiet: `${relative} branch=${branch} ui=${ports.uiPort} api=${ports.apiPort}`,
  });
  if (interactive) clack.outro('done');
  return null;
}

const invokedDirectly = (() => {
  try {
    return import.meta.url === pathToFileURL(realpathSync(process.argv[1])).href;
  } catch {
    return false;
  }
})();

if (invokedDirectly) {
  main().catch((error) => {
    process.stderr.write(`${error instanceof Error ? error.message : String(error)}\n`);
    process.exit(1);
  });
}
