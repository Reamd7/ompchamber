// Stage the Rust ompchamber-server binary for packaged desktop builds.
//
// The Electron main spawns this as the UI server sidecar (replacing the
// former in-process JS server). Output: resources/ompchamber-server(.exe),
// built from packages/web/server-rs. Cross-architecture builds pass
// `--target` through to cargo when the rustup target is installed.

import { spawnSync } from 'node:child_process';
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { resolveTargetArchitecture } from './target-architecture.mjs';

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const electronRoot = path.resolve(__dirname, '..');
const workspaceRoot = path.resolve(electronRoot, '../..');
const serverRoot = path.join(workspaceRoot, 'packages', 'web', 'server-rs');
const outputName = process.platform === 'win32' ? 'ompchamber-server.exe' : 'ompchamber-server';
const outputPath = path.join(electronRoot, 'resources', outputName);

const env = { ...process.env };
const builderArgs = process.argv.slice(2);
const targetArchitecture = resolveTargetArchitecture({ environment: env, builderArgs });

const cargoArgs = ['build', '--release'];
const rustTargetMap = {
  'darwin-arm64': 'aarch64-apple-darwin',
  'darwin-x64': 'x86_64-apple-darwin',
  'linux-arm64': 'aarch64-unknown-linux-gnu',
  'linux-x64': 'x86_64-unknown-linux-gnu',
  'win32-x64': 'x86_64-pc-windows-msvc',
};
const requestedTarget = builderArgs
  .find((argument) => argument.startsWith('--target='))
  ?.slice('--target='.length);
if (requestedTarget) {
  cargoArgs.push('--target', requestedTarget);
} else {
  const rustTarget = rustTargetMap[`${process.platform}-${targetArchitecture.electronBuilder}`];
  if (rustTarget && rustTarget !== 'aarch64-apple-darwin') {
    cargoArgs.push('--target', rustTarget);
  }
}

console.log(`[electron] cargo ${cargoArgs.join(' ')} (server-rs)`);
const build = spawnSync('cargo', cargoArgs, { cwd: serverRoot, stdio: 'inherit', env });
if (build.status !== 0) {
  console.error('[electron] cargo build failed');
  process.exit(build.status ?? 1);
}

const builtPath = requestedTarget
  ? path.join(serverRoot, 'target', requestedTarget, 'release', outputName)
  : path.join(serverRoot, 'target', 'release', outputName);
if (!fs.existsSync(builtPath)) {
  console.error(`[electron] expected server binary missing: ${builtPath}`);
  process.exit(1);
}

fs.mkdirSync(path.dirname(outputPath), { recursive: true });
fs.copyFileSync(builtPath, outputPath);
fs.chmodSync(outputPath, 0o755);
console.log(`[electron] staged rust server: ${outputPath}`);
