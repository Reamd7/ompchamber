// Verify the staged/packaged omp host binary for desktop builds.
//
// --staged   check resources/omp-host in the workspace (post prepare)
// --packaged check the binary inside a packaged app directory

import { spawnSync } from 'node:child_process';
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const electronRoot = path.resolve(__dirname, '..');
const mode = process.argv.includes('--packaged') ? 'packaged' : 'staged';

const binaryName = process.platform === 'win32' ? 'omp-host.exe' : 'omp-host';
const candidates =
  mode === 'staged'
    ? [path.join(electronRoot, 'resources', 'omp-host', binaryName)]
    : [
        path.join(process.resourcesPath ?? '', 'omp-host', binaryName),
        path.join(electronRoot, 'dist', binaryName),
      ];

const binary = candidates.find((candidate) => fs.existsSync(candidate));
if (!binary) {
  console.error(`[electron] omp host binary not found (${mode}): ${candidates.join(', ')}`);
  process.exit(1);
}

// Boot the host on an ephemeral port and confirm it reports healthy, then
// stop it. This proves the compiled engine actually serves.
const port = 3997;
const child = spawnSync(
  binary,
  ['serve', '--hostname', '127.0.0.1', '--port', String(port)],
  { stdio: 'ignore', windowsHide: true, timeout: 30000 },
);

if (child.error && child.error.code !== 'ETIMEDOUT') {
  // spawnSync waits for exit; a healthy server stays up until the timeout.
  console.error(`[electron] omp host failed to launch: ${child.error.message}`);
  process.exit(1);
}

const check = spawnSync(
  process.platform === 'win32' ? 'curl' : 'curl',
  ['-s', '--max-time', '5', `http://127.0.0.1:${port}/global/health`],
  { encoding: 'utf8', timeout: 10000 },
);
const healthy = (check.stdout || '').includes('"healthy":true');
if (process.platform === 'win32') {
  spawnSync('taskkill', ['/F', '/IM', binaryName], { stdio: 'ignore' });
} else {
  spawnSync('pkill', ['-f', 'omp-host serve'], { stdio: 'ignore' });
}

if (!healthy) {
  console.error(`[electron] omp host health check failed: ${check.stdout || '(no output)'}`);
  process.exit(1);
}
console.log(`[electron] verified omp host ${mode}: ${binary}`);
