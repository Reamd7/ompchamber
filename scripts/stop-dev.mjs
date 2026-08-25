#!/usr/bin/env node
// `bun run stop` — kill whatever is listening on this checkout's dev ports.
//
// scripts/worktree-ports.mjs owns the target set: the shared defaults
// (5180/3902), this checkout's .dev-ports.json pair, and every worktree pair
// recorded under .worktrees/. Every occupied port's listener PID(s) are killed
// with their process trees, then each port is re-probed; a port that stays
// occupied fails the run.

import { spawnSync } from 'node:child_process';
import { realpathSync } from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { parseArgs } from 'node:util';
import * as clack from '@clack/prompts';
import { collectDevPorts } from './worktree-ports.mjs';

/** Windows kernel sockets report System Idle (0) / System (4); never signal them. */
const SYSTEM_PIDS = new Set([0, 4]);

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

function runCapture(command, args) {
  try {
    return spawnSync(command, args, { encoding: 'utf8', timeout: 15_000, windowsHide: true });
  } catch {
    return null;
  }
}

/**
 * Windows `netstat -ano -p TCP` rows for LISTENING sockets whose local port is
 * wanted, as `port → Set(pid)`. IPv6 locals look like `[::]:5180`, so the port
 * is everything after the last colon.
 */
export function parseNetstatListeningPids(output, ports) {
  const wanted = new Set(ports);
  const pidsByPort = new Map();
  for (const line of String(output).split(/\r?\n/)) {
    const match = /^\s*TCP\s+(\S+)\s+\S+\s+LISTENING\s+(\d+)\s*$/.exec(line);
    if (!match) continue;
    const local = match[1];
    const port = Number(local.slice(local.lastIndexOf(':') + 1));
    if (!wanted.has(port)) continue;
    const pid = Number(match[2]);
    if (!Number.isInteger(pid) || pid <= 0) continue;
    if (!pidsByPort.has(port)) pidsByPort.set(port, new Set());
    pidsByPort.get(port).add(pid);
  }
  return pidsByPort;
}

/** port → Set(pid) for listeners on `ports`. Throws when the platform tool is missing. */
export function findPortListeners(ports) {
  if (ports.length === 0) return new Map();
  if (process.platform === 'win32') {
    const result = runCapture('netstat', ['-ano', '-p', 'TCP']);
    if (!result || result.error || typeof result.stdout !== 'string') {
      throw new Error('netstat is unavailable; cannot resolve dev-port listeners');
    }
    return parseNetstatListeningPids(result.stdout, ports);
  }
  const listeners = new Map();
  for (const port of ports) {
    const result = runCapture('lsof', ['-t', '-nP', `-iTCP:${port}`, '-sTCP:LISTEN']);
    if (!result || result.error) {
      throw new Error('lsof is unavailable; cannot resolve dev-port listeners');
    }
    const pids = new Set(
      String(result.stdout || '')
        .split(/\s+/)
        .map((value) => Number(value))
        .filter((pid) => Number.isInteger(pid) && pid > 0),
    );
    if (pids.size > 0) listeners.set(port, pids);
  }
  return listeners;
}

function killPidTree(pid) {
  if (process.platform === 'win32') {
    const result = runCapture('taskkill', ['/PID', String(pid), '/T', '/F']);
    return result !== null && result.status === 0;
  }
  try {
    process.kill(pid, 'SIGTERM');
  } catch (error) {
    return error?.code === 'ESRCH'; // already gone
  }
  return true;
}

const isPidAlive = (pid) => {
  try {
    process.kill(pid, 0);
    return true;
  } catch (error) {
    return error?.code === 'EPERM'; // exists, just not ours to probe
  }
};

/** POSIX listeners got SIGTERM in killPidTree; SIGKILL what outlives the grace. */
async function escalatePosixKill(pid, graceMs) {
  for (let waited = 0; waited < graceMs && isPidAlive(pid); waited += 100) {
    await sleep(100);
  }
  if (!isPidAlive(pid)) return;
  try {
    process.kill(pid, 'SIGKILL');
  } catch {
    // ESRCH means it died between the check and the signal — done either way.
  }
}

/**
 * A port settles when no process holds a LISTEN socket on it. Verified
 * against the OS listener table, not a bind probe: on Windows, binding a
 * specific address can succeed while another process holds the wildcard,
 * so a probe would call a still-occupied dev port free.
 */
async function waitUntilNoListener(port, timeoutMs) {
  const occupied = () => (findPortListeners([port]).get(port) ?? new Set()).size > 0;
  for (let waited = 0; waited < timeoutMs && occupied(); waited += 200) {
    await sleep(200);
  }
  return !occupied();
}

/**
 * Kill every listener on `targets` (`{ port, origin }` from collectDevPorts),
 * then re-probe each port. Returns `{ ok, error?, results }` where each result
 * carries outcome `free` (nothing listened), `stopped` (killed, now free), or
 * `busy` (still occupied — run failed).
 */
export async function stopDevPortListeners(targets, { killGraceMs = 3000, settleMs = 5000 } = {}) {
  let listeners;
  try {
    listeners = findPortListeners(targets.map((target) => target.port));
  } catch (error) {
    return { ok: false, error: error instanceof Error ? error.message : String(error), results: [] };
  }

  // One pid can hold several target ports; kill each distinct pid exactly once.
  const killablePids = new Set();
  for (const pids of listeners.values()) {
    for (const pid of pids) {
      if (pid === process.pid || SYSTEM_PIDS.has(pid)) continue;
      killablePids.add(pid);
    }
  }
  for (const pid of killablePids) {
    killPidTree(pid);
  }
  if (process.platform !== 'win32') {
    await Promise.all([...killablePids].map((pid) => escalatePosixKill(pid, killGraceMs)));
  }

  const results = [];
  for (const target of targets) {
    const pids = listeners.get(target.port) ?? new Set();
    const wasOccupied = pids.size > 0;
    const free = await waitUntilNoListener(target.port, settleMs);
    results.push({
      port: target.port,
      origin: target.origin,
      pids: wasOccupied ? [...pids] : [],
      outcome: free ? (wasOccupied ? 'stopped' : 'free') : 'busy',
    });
  }
  return { ok: results.every((result) => result.outcome !== 'busy'), results };
}

function formatResult(result) {
  const pidSuffix = result.pids.length > 0 ? ` pid=${result.pids.join(',')}` : '';
  return `port ${result.port} ${result.outcome}${pidSuffix}`;
}

/**
 * Render a stop outcome for one of the three output modes into
 * `{ out, err, exitCode }` streams for main() to write. `--json` emits one
 * JSON payload only; `--quiet` emits one stable `port <n> <outcome>` line per
 * port, with busy ports and errors on stderr; the default mode prints framed
 * Clack output directly.
 */
export function renderReport(outcome, { jsonMode = false, quietMode = false } = {}) {
  const results = outcome.results ?? [];
  const stoppedCount = results.filter((result) => result.outcome === 'stopped').length;
  const busyCount = results.filter((result) => result.outcome === 'busy').length;
  const exitCode = outcome.ok ? 0 : 1;

  if (jsonMode) {
    return {
      out: `${JSON.stringify({
        status: outcome.ok ? 'ok' : 'error',
        stoppedCount,
        busyCount,
        ...(outcome.error ? { error: outcome.error } : {}),
        results: results.map(({ port, origin, pids, outcome: resultOutcome }) => ({
          port,
          origin,
          pids,
          outcome: resultOutcome,
        })),
      })}\n`,
      err: '',
      exitCode,
    };
  }

  if (quietMode) {
    const out = results
      .filter((result) => result.outcome !== 'busy')
      .map((result) => formatResult(result))
      .join('\n');
    const err = [
      ...results.filter((result) => result.outcome === 'busy').map((result) => formatResult(result)),
      ...(outcome.error ? [outcome.error] : []),
    ].join('\n');
    return {
      out: out.length > 0 ? `${out}\n` : '',
      err: err.length > 0 ? `${err}\n` : '',
      exitCode,
    };
  }

  clack.intro('stop dev servers');
  for (const result of results) {
    if (result.outcome === 'stopped') {
      clack.log.success(`port ${result.port} stopped (pid ${result.pids.join(', ')}) — ${result.origin}`);
    } else if (result.outcome === 'free') {
      clack.log.info(`port ${result.port} already free — ${result.origin}`);
    } else {
      clack.log.error(`port ${result.port} still occupied after kill — ${result.origin}`);
    }
  }
  if (outcome.error) clack.log.error(outcome.error);
  clack.outro(
    busyCount > 0
      ? `${busyCount} port(s) still busy`
      : stoppedCount > 0
        ? `stopped ${stoppedCount} dev server(s)`
        : 'all dev ports already free',
  );
  return { out: '', err: '', exitCode };
}

async function main() {
  const { values } = parseArgs({
    args: process.argv.slice(2),
    options: {
      json: { type: 'boolean' },
      quiet: { type: 'boolean' },
    },
  });
  const jsonMode = values.json === true;
  const quietMode = values.quiet === true;

  const repoRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
  const outcome = await stopDevPortListeners(collectDevPorts(repoRoot));
  const report = renderReport(outcome, { jsonMode, quietMode });
  if (report.out) process.stdout.write(report.out);
  if (report.err) process.stderr.write(report.err);
  process.exitCode = report.exitCode;
}

const invokedDirectly = (() => {
  try {
    return realpathSync(process.argv[1]) === realpathSync(fileURLToPath(import.meta.url));
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
