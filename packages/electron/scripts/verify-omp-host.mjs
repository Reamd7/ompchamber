// Verify the staged/packaged omp host binary for desktop builds.
//
// --staged   check resources/omp-host in the workspace (post prepare)
// --packaged check the binary inside a packaged app directory

import { spawn } from 'node:child_process';
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const electronRoot = path.resolve(__dirname, '..');
const mode = process.argv.includes('--packaged') ? 'packaged' : 'staged';

const binaryName = process.platform === 'win32' ? 'omp-host.exe' : 'omp-host';
const appBundleName = 'OMPChamber.app';
const candidates =
  mode === 'staged'
    ? [path.join(electronRoot, 'resources', 'omp-host', binaryName)]
    : [
        // In-app invocation (Electron sets process.resourcesPath).
        path.join(process.resourcesPath ?? '', 'omp-host', binaryName),
        // electron-builder unpacked outputs for each desktop platform.
        path.join(electronRoot, 'dist', 'win-unpacked', 'resources', 'omp-host', binaryName),
        path.join(electronRoot, 'dist', 'win-arm64-unpacked', 'resources', 'omp-host', binaryName),
        path.join(electronRoot, 'dist', 'linux-unpacked', 'resources', 'omp-host', binaryName),
        path.join(electronRoot, 'dist', 'linux-arm64-unpacked', 'resources', 'omp-host', binaryName),
        path.join(electronRoot, 'dist', 'mac', appBundleName, 'Contents', 'Resources', 'omp-host', binaryName),
        path.join(electronRoot, 'dist', 'mac-arm64', appBundleName, 'Contents', 'Resources', 'omp-host', binaryName),
      ];

const binary = candidates.find((candidate) => fs.existsSync(candidate));
if (!binary) {
  console.error(`[electron] omp host binary not found (${mode}): ${candidates.join(', ')}`);
  process.exit(1);
}

// Boot the host on a loopback port, poll /global/health until it reports
// healthy, then stop it. This proves the compiled engine actually serves.
const port = 3997;
const healthCheckTimeoutMs = 30_000;

const child = spawn(binary, ['serve', '--hostname', '127.0.0.1', '--port', String(port)], {
  stdio: ['ignore', 'ignore', 'pipe'],
  windowsHide: true,
  // The verification protocol below polls without credentials; an ambient
  // OPENCODE_SERVER_PASSWORD (e.g. packaging run inside the desktop app)
  // would 401 every probe until timeout. Verify the no-auth boot shape.
  env: { ...process.env, OPENCODE_SERVER_PASSWORD: '' },
});
child.unref();

let stderrTail = '';
child.stderr?.on('data', (chunk) => {
  stderrTail = (stderrTail + String(chunk)).slice(-2000);
});

const stopChild = () => {
  try {
    process.kill(child.pid);
  } catch {
    // Already gone.
  }
};

const finish = (code) => {
  stopChild();
  setTimeout(() => process.exit(code), 250);
};
const waitForHealthy = async () => {
  const deadline = Date.now() + healthCheckTimeoutMs;
  const url = `http://127.0.0.1:${port}/global/health`;
  for (;;) {
    if (child.exitCode !== null) {
      console.error(`[electron] omp host exited early with code ${child.exitCode}`);
      if (stderrTail.trim()) console.error(`[electron] omp host stderr (tail):\n${stderrTail.trim()}`);
      process.exit(1);
    }
    if (Date.now() > deadline) {
      console.error('[electron] omp host health check timed out');
      finish(1);
      return;
    }
    try {
      const response = await fetch(url, { signal: AbortSignal.timeout(2000) });
      const body = await response.text();
      if (response.ok && body.includes('"healthy":true')) {
        // /global/health only proves the server booted; capabilities also
        // proves bundled build-time data survived compilation (a runtime
        // readFileSync into the bunfs root 500'd every packaged build
        // while health-only verification stayed green).
        const caps = await fetch(`http://127.0.0.1:${port}/omp/capabilities`, { signal: AbortSignal.timeout(5000) })
          .then((r) => r.json())
          .catch(() => null);
        if (!caps?.features?.['modelRoles.v1']) {
          console.error(`[electron] omp host capabilities check failed: ${JSON.stringify(caps).slice(0, 300)}`);
          finish(1);
          return;
        }
        console.log(`[electron] omp host capabilities: ${Object.keys(caps.features).length} features`);
        console.log(`[electron] verified omp host ${mode}: ${binary}`);
        finish(0);
        return;
      }
    } catch {
      // Not listening yet; retry.
      await new Promise((resolve) => setTimeout(resolve, 500));
    }
  }
};

await waitForHealthy();
