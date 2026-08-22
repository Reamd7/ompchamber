// Stage the OpenChamber omp host as a self-contained binary for packaged
// desktop builds.
//
// The host (packages/web/server/lib/omp-host) embeds @oh-my-pi/
// pi-coding-agent and serves the OpenCode-compatible wire surface. We compile
// it with `bun build --compile` so the packaged app needs no separate Bun
// runtime. Output: resources/omp-host/omp-host(.exe), verified by running
// `--version`-style readiness (the host serves /global/health, so verification
// is done by verify-omp-host.mjs at packaging time).

import { spawnSync } from 'node:child_process';
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { resolveTargetArchitecture } from './target-architecture.mjs';

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const electronRoot = path.resolve(__dirname, '..');
const workspaceRoot = path.resolve(electronRoot, '../..');
const outputDir = path.join(electronRoot, 'resources', 'omp-host');
const hostEntry = path.join(workspaceRoot, 'packages', 'web', 'server', 'lib', 'omp-host', 'host.js');

const targetForPlatform = (platform, targetArchitecture) => {
  const arch = targetArchitecture.opencode;
  if (platform === 'darwin') {
    if (arch === 'arm64') return 'bun-darwin-aarch64';
    if (arch === 'x64') return 'bun-darwin-x64';
  }
  if (platform === 'win32') {
    // Bun does not ship windows-arm64; x64 runs under emulation.
    if (arch === 'arm64' || arch === 'x64') return 'bun-windows-x64';
  }
  if (platform === 'linux') {
    if (arch === 'arm64') return 'bun-linux-aarch64';
    if (arch === 'x64') return 'bun-linux-x64';
  }
  throw new Error(`No compile target for ${platform}/${arch}`);
};

const main = () => {
  if (!fs.existsSync(hostEntry)) {
    throw new Error(`omp host entry missing: ${hostEntry}`);
  }
  const targetArchitecture = resolveTargetArchitecture();
  const target = process.env.OPENCHAMBER_OMP_HOST_COMPILE_TARGET || targetForPlatform(process.platform, targetArchitecture);
  const binaryName = process.platform === 'win32' ? 'omp-host.exe' : 'omp-host';
  const outputBinary = path.join(outputDir, binaryName);

  const expectedDescriptor = `${target}`;
  const descriptorPath = path.join(outputDir, '.build-target');
  if (fs.existsSync(outputBinary) && fs.readFileSync(descriptorPath, 'utf8').trim() === expectedDescriptor) {
    console.log(`[electron] bundled omp host already prepared: ${outputBinary} (${expectedDescriptor})`);
    return;
  }

  fs.mkdirSync(outputDir, { recursive: true });
  for (const entry of fs.readdirSync(outputDir)) {
    if (entry === '.gitkeep') continue;
    fs.rmSync(path.join(outputDir, entry), { recursive: true, force: true });
  }

  // Cross-compiling for a foreign platform needs the matching bun build
  // target; same-platform builds use the local bun directly.
  const compileArgs = [
    'build',
    '--compile',
    ...(target ? ['--target', target] : []),
    // Optional omp extension surface that only exists inside the omp repo.
    '--external',
    'omp-legacy-pi-modules',
    hostEntry,
    '--outfile',
    outputBinary,
  ];
  console.log(`[electron] compiling omp host (${target || 'local'})`);
  const result = spawnSync('bun', compileArgs, {
    cwd: path.join(workspaceRoot, 'packages', 'web'),
    stdio: 'inherit',
    windowsHide: true,
  });
  if (result.status !== 0) {
    throw new Error(`bun build --compile failed with status ${result.status}`);
  }
  if (!fs.existsSync(outputBinary)) {
    throw new Error(`compile produced no binary at ${outputBinary}`);
  }
  if (process.platform !== 'win32') {
    fs.chmodSync(outputBinary, 0o755);
  }
  fs.writeFileSync(descriptorPath, expectedDescriptor);
  console.log(`[electron] prepared omp host: ${outputBinary} (${expectedDescriptor})`);
};

try {
  main();
} catch (error) {
  console.error(error instanceof Error ? error.message : error);
  process.exit(1);
}
