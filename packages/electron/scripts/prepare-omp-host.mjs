// Stage the OMPChamber omp host as a self-contained binary for packaged
// Stage the OMPChamber omp host as a self-contained JS bundle for packaged
// desktop builds.
// The host (packages/web/server/lib/omp-host) embeds @oh-my-pi/
// pi-coding-agent and serves the OpenCode-compatible wire surface. We bundle
// it with `bun build --target=bun --format=esm` so the packaged app ships one
// host.js plus emitted sidecar assets, executed by a Bun runtime resolved at
// launch (managed runtime or PATH — see omp-host-launch.js). Verified by
// probing /global/health (verify-omp-host.mjs at packaging time).

import { spawnSync } from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { resolveTargetArchitecture } from './target-architecture.mjs';

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const electronRoot = path.resolve(__dirname, '..');
const workspaceRoot = path.resolve(electronRoot, '../..');
const outputDir = path.join(electronRoot, 'resources', 'omp-host');
const hostEntry = path.join(workspaceRoot, 'packages', 'web', 'server', 'lib', 'omp-host', 'host.ts');

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

const NATIVES_PLATFORM_TAG = { windows: 'win32', win32: 'win32', darwin: 'darwin', linux: 'linux' };

// The effective compile target (not the build host) decides which pi_natives
// addon ships: bun has no windows-arm64 output, so arm64 Windows builds run
// the x64 host under emulation and need the x64 addon.
const parseCompileTarget = (value) => {
  const match = /^bun-(windows|win32|darwin|linux)-(x64|arm64|aarch64)$/.exec(value || '');
  if (!match) return null;
  return { platform: NATIVES_PLATFORM_TAG[match[1]], arch: match[2] === 'aarch64' ? 'arm64' : match[2] };
};

// The omp host engine dlopens the pi_natives native addon at boot. On a
// clean machine it resolves from the omp user cache (~/.omp/natives) or,
// for compiled binaries, from the directory of the executable itself — so
// the packaged app must ship the addon next to omp-host, or the engine
// crashes on first launch for every user without a prior omp install.
const listAddonFiles = (dir) => {
  try {
    return fs.readdirSync(dir).filter((entry) => /^pi_natives\..+\.node$/.test(entry));
  } catch {
    return [];
  }
};
const readNpmRegistry = ({ env, workspaceRoot }) => {
  const explicit = env.NPM_CONFIG_REGISTRY?.trim();
  if (explicit) return explicit.replace(/\/+$/, '');
  const configFiles = [
    path.join(workspaceRoot, '.npmrc'),
    path.join(os.homedir(), '.npmrc'),
  ];
  for (const configFile of configFiles) {
    try {
      const line = fs.readFileSync(configFile, 'utf8')
        .split(/\r?\n/)
        .find((entry) => /^\s*registry\s*=/.test(entry));
      if (line) return line.split('=').slice(1).join('=').trim().replace(/\/+$/, '');
    } catch {
      // No .npmrc at this location.
    }
  }
  return 'https://registry.npmjs.org';
};

const stageNatives = async ({ platform, arch, outputDir, workspaceRoot }) => {
  const tag = NATIVES_PLATFORM_TAG[platform];
  if (!tag) throw new Error(`No pi_natives platform tag for ${platform}`);
  const packageName = `pi-natives-${tag}-${arch}`;
  const candidateDirs = [
    path.join(workspaceRoot, 'node_modules', '.bun', 'node_modules', '@oh-my-pi', packageName),
    path.join(workspaceRoot, 'node_modules', '@oh-my-pi', packageName),
    path.join(workspaceRoot, 'packages', 'web', 'node_modules', '@oh-my-pi', packageName),
  ];
  const sourceDir = candidateDirs.find((dir) => listAddonFiles(dir).length > 0);
  if (sourceDir) {
    const files = listAddonFiles(sourceDir);
    for (const file of files) {
      fs.copyFileSync(path.join(sourceDir, file), path.join(outputDir, file));
    }
    console.log(`[electron] staged omp natives from ${packageName}: ${files.join(', ')}`);
    return;
  }

  // Cross-arch targets have no matching optional dependency on the build
  // host. The engine loader's error output points at GitHub release assets,
  // but versioned releases do not ship pi_natives files; the npm package is
  // the reliable, version-pinned channel.
  const agentPackagePath = path.join(workspaceRoot, 'packages', 'web', 'node_modules', '@oh-my-pi', 'pi-coding-agent', 'package.json');
  const version = JSON.parse(fs.readFileSync(agentPackagePath, 'utf8')).dependencies?.['@oh-my-pi/pi-natives'];
  if (!version) throw new Error(`Cannot determine omp natives version from ${agentPackagePath}`);
  const scopedName = `@oh-my-pi/${packageName}`;
  const registry = readNpmRegistry({ env: process.env, workspaceRoot });
  const tarballUrl = `${registry}/${scopedName}/-/${packageName}-${version}.tgz`;
  console.log(`[electron] ${scopedName} not installed; downloading ${tarballUrl}`);
  const response = await fetch(tarballUrl, { redirect: 'follow', signal: AbortSignal.timeout(300_000) });
  if (!response.ok) throw new Error(`Failed to download omp natives (${response.status}): ${tarballUrl}`);
  const tempDir = fs.mkdtempSync(path.join(os.tmpdir(), 'omp-natives-'));
  try {
    const tarballPath = path.join(tempDir, 'natives.tgz');
    fs.writeFileSync(tarballPath, Buffer.from(await response.arrayBuffer()));
    const extract = spawnSync('tar', ['-xzf', tarballPath, '-C', tempDir], { windowsHide: true });
    if (extract.status !== 0) throw new Error(`tar extract failed (${extract.status}) for ${tarballUrl}`);
    const files = listAddonFiles(path.join(tempDir, 'package'));
    if (files.length === 0) throw new Error(`npm tarball ${scopedName}@${version} contains no pi_natives addon`);
    for (const file of files) {
      fs.copyFileSync(path.join(tempDir, 'package', file), path.join(outputDir, file));
    }
    console.log(`[electron] downloaded omp natives ${scopedName}@${version}: ${files.join(', ')}`);
  } finally {
    fs.rmSync(tempDir, { recursive: true, force: true });
  }
};

const main = async () => {
  if (!fs.existsSync(hostEntry)) {
    throw new Error(`omp host entry missing: ${hostEntry}`);
  }
  const targetArchitecture = resolveTargetArchitecture();
  const target = process.env.OMPCHAMBER_OMP_HOST_COMPILE_TARGET || targetForPlatform(process.platform, targetArchitecture);
  // Layout: resources/omp-host/dist/ holds the bundle (host.js + emitted
  // assets) and resources/omp-host/native/ the pi_natives addon. The split is
  // load-bearing: the bundled loader resolves its nativeDir as
  // `<host.js dir>/../native`, so the addon must sit in a `native/` sibling of
  // the bundle directory (a flat layout would put the addon on a path the
  // loader never reads — the per-user ~/.omp/natives cache is only consulted
  // for compiled binaries).
  const bundleDir = path.join(outputDir, 'dist');
  const nativesDir = path.join(outputDir, 'native');
  const hostBundle = path.join(bundleDir, 'host.js');

  const expectedDescriptor = `${target}`;
  const descriptorPath = path.join(bundleDir, '.build-target');
  const alreadyBuilt = fs.existsSync(hostBundle)
    && fs.existsSync(descriptorPath)
    && fs.readFileSync(descriptorPath, 'utf8').trim() === expectedDescriptor;
  if (alreadyBuilt) {
    console.log(`[electron] bundled omp host already prepared: ${hostBundle} (${expectedDescriptor})`);
  } else {
    fs.mkdirSync(outputDir, { recursive: true });
    fs.rmSync(bundleDir, { recursive: true, force: true });
    fs.rmSync(nativesDir, { recursive: true, force: true });
    fs.mkdirSync(bundleDir, { recursive: true });

    // The bundle itself is platform-neutral JS plus emitted assets, so the
    // runtime target is always `bun` (full triples are only valid for
    // `--compile`). The resolved target triple still drives pi_natives
    // variant selection below and the rebuild descriptor.
    const buildArgs = [
      'build',
      '--target', 'bun',
      '--format', 'esm',
      // Optional omp extension surface that only exists inside the omp repo.
      '--external',
      'omp-legacy-pi-modules',
      hostEntry,
      '--outdir',
      bundleDir,
    ];
    console.log(`[electron] bundling omp host (${target})`);
    const result = spawnSync('bun', buildArgs, {
      cwd: path.join(workspaceRoot, 'packages', 'web'),
      stdio: 'inherit',
      windowsHide: true,
    });
    if (result.status !== 0) {
      throw new Error(`bun build failed with status ${result.status}`);
    }
    if (!fs.existsSync(hostBundle)) {
      throw new Error(`bundle produced no host.js at ${hostBundle}`);
    }
    fs.writeFileSync(descriptorPath, expectedDescriptor);
    console.log(`[electron] prepared omp host: ${hostBundle} (${expectedDescriptor})`);
  }

  const nativesTarget = parseCompileTarget(target)
    ?? { platform: process.platform, arch: targetArchitecture.opencode };
  if (process.env.OMPCHAMBER_STAGE_NATIVES === '0') {
    // S3.5 packaging: the installer ships no addon; first boot downloads it
    // next to the managed bun runtime (main.mjs ensureNativesNextTo).
    console.log('[electron] skipping omp natives staging (OMPCHAMBER_STAGE_NATIVES=0)');
    return;
  }
  fs.mkdirSync(nativesDir, { recursive: true });
  await stageNatives({
    platform: nativesTarget.platform,
    arch: nativesTarget.arch,
    outputDir: nativesDir,
    workspaceRoot,
  });
};

main().catch((error) => {
  console.error(error instanceof Error ? error.message : error);
  process.exit(1);
});
