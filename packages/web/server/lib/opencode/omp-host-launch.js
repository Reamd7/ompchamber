// Launch-spec resolution for the managed omp host process.
//
// The managed engine is no longer the `opencode` CLI: it is the OMPChamber
// omp host (server/lib/omp-host/host.js) run under Bun. The host embeds
// @oh-my-pi/pi-coding-agent and serves the OpenCode-compatible wire surface,
// so everything around the child process (ports, Basic auth, readiness line,
// health checks, orphan reaping) works exactly as it did for `opencode serve`.

import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const __dirname = path.dirname(fileURLToPath(import.meta.url));

export const OMP_HOST_ENTRY = path.join(__dirname, '..', 'omp-host', 'host.js');

const isBunRuntime = () => Boolean(process.versions?.bun) && !process.env.ELECTRON_RUN_AS_NODE;
const hostBinaryName = () => (process.platform === 'win32' ? 'omp-host.exe' : 'omp-host');

const findBundledHostBinary = () => {
  const dirs = [
    process.env.OMPCHAMBER_BUNDLED_OMP_HOST_DIR,
    process.resourcesPath ? path.join(process.resourcesPath, 'omp-host') : null,
  ].filter(Boolean);
  for (const dir of dirs) {
    const candidate = path.join(dir, hostBinaryName());
    if (fs.existsSync(candidate)) return candidate;
  }
  return null;
};

/**
 * Resolve the runtime binary that launches the omp host from source.
 *
 * Priority: explicit OMPCHAMBER_OMP_HOST_RUNTIME env → the current process
 * when it already runs under Bun → `bun` resolved from PATH.
 */
export const resolveOmpHostRuntimeBinary = () => {
  const explicit = (process.env.OMPCHAMBER_OMP_HOST_RUNTIME || '').trim();
  if (explicit) {
    if (!fs.existsSync(explicit)) {
      const error = new Error(`OMPCHAMBER_OMP_HOST_RUNTIME does not exist: ${explicit}`);
      error.code = 'OMP_HOST_RUNTIME_INVALID';
      throw error;
    }
    return { binary: explicit, source: 'env' };
  }
  if (isBunRuntime()) {
    return { binary: process.execPath, source: 'current-process' };
  }
  return { binary: 'bun', source: 'path' };
};

/**
 * Build the managed omp host launch spec.
 *
 * Priority: a packaged, self-contained host binary (explicit env or bundled
 * resources) → source entry launched under a Bun runtime.
 * @returns {{ binary: string, args: string[], source: string }}
 */
export const resolveOmpHostLaunchSpec = ({ hostname, port }) => {
  const serveArgs = ['serve', '--hostname', hostname, '--port', String(port)];

  const explicitHost = (process.env.OMPCHAMBER_OMP_HOST_BINARY || '').trim();
  if (explicitHost) {
    if (!fs.existsSync(explicitHost)) {
      const error = new Error(`OMPCHAMBER_OMP_HOST_BINARY does not exist: ${explicitHost}`);
      error.code = 'OMP_HOST_RUNTIME_INVALID';
      throw error;
    }
    return { binary: explicitHost, args: serveArgs, source: 'env-host' };
  }

  const bundled = findBundledHostBinary();
  if (bundled) {
    return { binary: bundled, args: serveArgs, source: 'bundled' };
  }

  if (!fs.existsSync(OMP_HOST_ENTRY)) {
    const error = new Error(`omp host entry missing: ${OMP_HOST_ENTRY}`);
    error.code = 'OMP_HOST_RUNTIME_INVALID';
    throw error;
  }
  const runtime = resolveOmpHostRuntimeBinary();
  return {
    binary: runtime.binary,
    args: [OMP_HOST_ENTRY, ...serveArgs],
    source: runtime.source,
  };
};
