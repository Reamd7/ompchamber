// Per-worktree dev port allocation.
//
// The web dev stack is fully port-parameterized (OPENCHAMBER_HMR_UI_PORT /
// OPENCHAMBER_HMR_API_PORT, see scripts/dev-web-hmr.mjs). A worktree created
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

function recordedWorktreePorts(repoRoot) {
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
    if (ports) recorded.push(ports);
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
  for (const recorded of recordedWorktreePorts(repoRoot)) {
    taken.add(recorded.uiPort);
    taken.add(recorded.apiPort);
  }
  const uiPort = await findFreePortAbove(DEFAULT_UI_PORT + 1, taken, host);
  if (!uiPort) return null;
  taken.add(uiPort);
  const apiPort = await findFreePortAbove(DEFAULT_API_PORT + 1, taken, host);
  if (!apiPort) return null;
  return { uiPort, apiPort };
}
