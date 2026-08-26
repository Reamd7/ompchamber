// Per-worktree dev port allocation.
//
// The web dev stack is fully port-parameterized (OMPCHAMBER_HMR_UI_PORT /
// OMPCHAMBER_HMR_API_PORT, see scripts/dev-web-hmr.mjs). A worktree created
// by `worktree init` persists its own pair in `.dev-ports.json` at the
// checkout root, and the dev launcher prefers that file over the shared
// defaults so parallel worktrees never fight over 5180/3902.

import fs from 'node:fs';
import net from 'node:net';
import path from 'node:path';

export const DEV_PORTS_FILENAME = '.dev-ports.json';
export const DEFAULT_UI_PORT = 5180;
export const DEFAULT_API_PORT = 3902;
export const WORKTREES_DIRNAME = '.worktrees';

/**
 * Read `{ uiPort, apiPort }` from a checkout root. Missing or malformed file
 * returns null — callers fall back to the shared defaults.
 */
export function readDevPorts(rootDir) {
  let raw;
  try {
    raw = fs.readFileSync(path.join(rootDir, DEV_PORTS_FILENAME), 'utf8');
  } catch {
    return null;
  }
  try {
    const parsed = JSON.parse(raw);
    const uiPort = Number(parsed?.uiPort);
    const apiPort = Number(parsed?.apiPort);
    if (!Number.isInteger(uiPort) || !Number.isInteger(apiPort)) return null;
    if (uiPort <= 0 || uiPort > 65535 || apiPort <= 0 || apiPort > 65535) return null;
    return { uiPort, apiPort };
  } catch {
    return null;
  }
}

export function writeDevPorts(checkoutDir, ports) {
  fs.writeFileSync(
    path.join(checkoutDir, DEV_PORTS_FILENAME),
    `${JSON.stringify({ uiPort: ports.uiPort, apiPort: ports.apiPort }, null, 2)}\n`,
  );
}

export function isFreePort(port, host = '127.0.0.1') {
  return new Promise((resolve) => {
    const server = net.createServer();
    server.once('error', () => resolve(false));
    server.listen(port, host, () => server.close(() => resolve(true)));
  });
}

function recordedWorktreeEntries(repoRoot) {
  let entries;
  try {
    entries = fs.readdirSync(path.join(repoRoot, WORKTREES_DIRNAME), { withFileTypes: true });
  } catch {
    return [];
  }
  const recorded = [];
  for (const entry of entries) {
    if (!entry.isDirectory()) continue;
    const ports = readDevPorts(path.join(repoRoot, WORKTREES_DIRNAME, entry.name));
    if (ports) recorded.push({ name: entry.name, ports });
  }
  return recorded;
}

async function findFreePortAbove(start, taken, host) {
  for (let port = start; port < start + 500; port += 1) {
    if (taken.has(port)) continue;
    if (await isFreePort(port, host)) return port;
  }
  return null;
}

/**
 * Pick the next free UI/API pair for a new worktree. Never returns the main
 * checkout's defaults and never a pair already recorded by another worktree,
 * whether or not that worktree is currently running. Returns null when the
 * scan window is exhausted.
 */
export async function allocateDevPorts({ repoRoot, host = '127.0.0.1' }) {
  const taken = new Set([DEFAULT_UI_PORT, DEFAULT_API_PORT]);
  for (const { ports } of recordedWorktreeEntries(repoRoot)) {
    taken.add(ports.uiPort);
    taken.add(ports.apiPort);
  }
  const uiPort = await findFreePortAbove(DEFAULT_UI_PORT + 1, taken, host);
  if (!uiPort) return null;
  taken.add(uiPort);
  const apiPort = await findFreePortAbove(DEFAULT_API_PORT + 1, taken, host);
  if (!apiPort) return null;
  return { uiPort, apiPort };
}

/**
 * Every dev port `bun run stop` should free: the shared defaults, this
 * checkout's own pair, and every worktree's recorded pair. Returns
 * `{ port, origin }` targets ordered by allocation order; a port seen twice
 * (impossible after `worktree init`, defensive anyway) keeps its first origin.
 */
export function collectDevPorts(rootDir) {
  const targets = new Map([
    [DEFAULT_UI_PORT, 'default'],
    [DEFAULT_API_PORT, 'default'],
  ]);
  const own = readDevPorts(rootDir);
  if (own) {
    if (!targets.has(own.uiPort)) targets.set(own.uiPort, DEV_PORTS_FILENAME);
    if (!targets.has(own.apiPort)) targets.set(own.apiPort, DEV_PORTS_FILENAME);
  }
  for (const { name, ports } of recordedWorktreeEntries(rootDir)) {
    const origin = `${WORKTREES_DIRNAME}/${name}/${DEV_PORTS_FILENAME}`;
    if (!targets.has(ports.uiPort)) targets.set(ports.uiPort, origin);
    if (!targets.has(ports.apiPort)) targets.set(ports.apiPort, origin);
  }
  return Array.from(targets, ([port, origin]) => ({ port, origin }));
}
