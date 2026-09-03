#!/usr/bin/env node
import { spawn } from 'node:child_process';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { DEFAULT_API_PORT, DEFAULT_UI_PORT, readDevPorts } from './worktree-ports.mjs';

const __filename = fileURLToPath(import.meta.url);
const __dirname = path.dirname(__filename);
const repoRoot = path.resolve(__dirname, '..');
const useDetachedChildren = process.platform === 'darwin';
const webRoot = path.join(repoRoot, 'packages/web');

function run(label, command, args, env = {}, options = {}) {
  return spawn(command, args, {
    cwd: options.cwd || repoRoot,
    stdio: 'inherit',
    env: { ...process.env, ...env },
    detached: useDetachedChildren,
  }).on('error', (error) => {
    console.error(`[dev:web:hmr] Failed to start ${label}:`, error);
  });
}

function waitForExit(child, timeoutMs) {
  return new Promise((resolve) => {
    if (!child || child.exitCode !== null || child.signalCode !== null) {
      resolve();
      return;
    }

    const onExit = () => {
      clearTimeout(timer);
      resolve();
    };

    const timer = setTimeout(() => {
      child.off('exit', onExit);
      resolve();
    }, timeoutMs);

    child.once('exit', onExit);
  });
}

function signalChild(child, signal) {
  if (!child || child.exitCode !== null || child.signalCode !== null) {
    return;
  }

  try {
    if (useDetachedChildren && process.platform !== 'win32') {
      process.kill(-child.pid, signal);
      return;
    }
  } catch {
  }

  try {
    child.kill(signal);
  } catch {
  }
}

async function stopChildTree(child) {
  if (!child || child.exitCode !== null || child.signalCode !== null) {
    return;
  }

  signalChild(child, 'SIGINT');
  await waitForExit(child, 2500);

  if (child.exitCode === null && child.signalCode === null) {
    signalChild(child, 'SIGTERM');
    await waitForExit(child, 2500);
  }

  if (child.exitCode === null && child.signalCode === null) {
    signalChild(child, 'SIGKILL');
    await waitForExit(child, 1000);
  }
}

const persistedDevPorts = readDevPorts(repoRoot);
const uiPort = process.env.OMPCHAMBER_HMR_UI_PORT
  || (persistedDevPorts ? String(persistedDevPorts.uiPort) : String(DEFAULT_UI_PORT));
const backendPort = process.env.OMPCHAMBER_HMR_API_PORT
  || (persistedDevPorts ? String(persistedDevPorts.apiPort) : String(DEFAULT_API_PORT));
const hmrHost = process.env.OMPCHAMBER_HMR_HOST || '0.0.0.0';

function getLanAddresses() {
  const addresses = [];

  for (const networkAddresses of Object.values(os.networkInterfaces())) {
    for (const address of networkAddresses || []) {
      if (address.family !== 'IPv4' || address.internal) continue;
      addresses.push(address.address);
    }
  }

  return addresses;
}

// Rsbuild has no dependency pre-bundling cache to clear (native ESM deps are
// compiled per-module), so there is no equivalent of Vite's --force reset.

const devServer = run(
  'rsbuild',
  'bun',
  ['x', 'rsbuild', 'dev', '--host', hmrHost, '--port', uiPort, '--strict-port'],
  {
    OMPCHAMBER_PORT: backendPort,
  },
  { cwd: webRoot },
);

// Development-only LAN exposure. Production launchers never set this marker;
// the server keeps its authentication guard for all other startup paths.
const api = run('api', process.execPath, ['--run', 'dev:server:watch'], {
  OMPCHAMBER_PORT: backendPort,
  OMPCHAMBER_HOST: hmrHost,
  OMPCHAMBER_DEV_SERVER: 'true',
}, { cwd: webRoot });

if (persistedDevPorts) {
  console.log(`[dev:web:hmr] worktree ports from .dev-ports.json (UI ${persistedDevPorts.uiPort} / API ${persistedDevPorts.apiPort})`);
}
console.log(`[dev:web:hmr] UI with HMR: http://127.0.0.1:${uiPort}`);
if (hmrHost === '0.0.0.0' || hmrHost === '::') {
  const lanAddresses = getLanAddresses();
  if (lanAddresses.length > 0) {
    for (const address of lanAddresses) {
      console.log(`[dev:web:hmr] LAN/mobile UI: http://${address}:${uiPort}`);
    }
  } else {
    console.log('[dev:web:hmr] LAN/mobile UI: no LAN IPv4 address found');
  }
}
console.log(`[dev:web:hmr] API: http://127.0.0.1:${backendPort}`);
console.log('[dev:web:hmr] IMPORTANT: open UI URL above for HMR; backend URL has no HMR');

let shuttingDown = false;

async function shutdown(exitCode = 0) {
  if (shuttingDown) return;
  shuttingDown = true;
  await Promise.all([stopChildTree(api), stopChildTree(devServer)]);
  process.exit(exitCode);
}

function onChildExit(label) {
  return (code, signal) => {
    if (shuttingDown) return;

    if (code !== 0 || signal) {
      console.error(`[dev:web:hmr] ${label} exited unexpectedly (code=${code ?? 'null'} signal=${signal ?? 'none'})`);
      shutdown(typeof code === 'number' ? code : 1).catch(() => process.exit(1));
      return;
    }

    shutdown(0).catch(() => process.exit(1));
  };
}

api.on('exit', onChildExit('api'));
devServer.on('exit', onChildExit('rsbuild'));

process.on('SIGINT', () => {
  shutdown(130).catch(() => process.exit(130));
});
process.on('SIGTERM', () => {
  shutdown(143).catch(() => process.exit(143));
});
process.on('SIGHUP', () => {
  shutdown(129).catch(() => process.exit(129));
});
