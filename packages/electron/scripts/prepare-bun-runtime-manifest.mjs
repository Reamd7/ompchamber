// Generate the pinned managed-Bun-runtime manifest for desktop builds.
//
// The desktop app can download an official Bun runtime on first launch when
// no usable bun exists on the machine (see bun-runtime.mjs). That download
// must be pinned and verifiable offline of GitHub at runtime, so the build
// bakes `resources/bun-runtime.json`: the exact Bun version, the minimum the
// engine accepts (pi-coding-agent engines.bun), the pi-natives version that
// rides with the pinned SDK (first-boot natives download, S3.5), and
// per-platform zip sha256s taken from the release's SHASUMS256.txt.
//
// Offline-friendly: when the manifest already exists for BUN_VERSION it is
// kept as-is (no network), so packaging never hard-requires GitHub access.

import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const electronRoot = path.resolve(__dirname, '..');
const workspaceRoot = path.resolve(electronRoot, '../..');
const outputPath = path.join(electronRoot, 'resources', 'bun-runtime.json');

/** Pinned Bun version the app downloads. Bump with pi-coding-agent engines.bun. */
const BUN_VERSION = process.env.OMPCHAMBER_BUN_RUNTIME_VERSION || '1.3.14';
/** Minimum Bun version the engine accepts (pi-coding-agent engines.bun). */
const MINIMUM_VERSION = process.env.OMPCHAMBER_BUN_RUNTIME_MINIMUM || '>=1.3.14';

/** Our platform-arch key → release asset file name. */
const ASSETS = {
  'linux-x64': `bun-linux-x64.zip`,
  'linux-x64-musl': `bun-linux-x64-musl.zip`,
  'linux-arm64': `bun-linux-aarch64.zip`,
  'linux-arm64-musl': `bun-linux-aarch64-musl.zip`,
  'darwin-x64': `bun-darwin-x64.zip`,
  'darwin-arm64': `bun-darwin-aarch64.zip`,
  'win32-x64': `bun-windows-x64.zip`,
};

const RELEASE_BASE = 'https://github.com/oven-sh/bun/releases/download';

const parseShasums = (text) => {
  const entries = {};
  for (const line of text.split(/\r?\n/)) {
    const match = /^([0-9a-f]{64})\s+\*?(.+?)\s*$/.exec(line);
    if (match) entries[match[2]] = match[1];
  }
  return entries;
};

const main = async () => {
  const existing = fs.existsSync(outputPath)
    ? JSON.parse(fs.readFileSync(outputPath, 'utf8'))
    : null;
  if (existing?.version === BUN_VERSION && existing?.nativesVersion) {
    console.log(`[electron] bun runtime manifest already pinned: ${outputPath} (${BUN_VERSION})`);
    return;
  }

  const tag = `bun-v${BUN_VERSION}`;
  const shasumUrl = `${RELEASE_BASE}/${tag}/SHASUMS256.txt`;
  console.log(`[electron] fetching ${shasumUrl}`);
  const response = await fetch(shasumUrl, { redirect: 'follow', signal: AbortSignal.timeout(60_000) });
  if (!response.ok) throw new Error(`Failed to fetch SHASUMS256.txt (${response.status}): ${shasumUrl}`);
  const shasums = parseShasums(await response.text());

  const assets = {};
  for (const [key, file] of Object.entries(ASSETS)) {
    const sha256 = shasums[file];
    if (!sha256) throw new Error(`SHASUMS256.txt for ${tag} has no entry for ${file}`);
    assets[key] = { file, sha256, url: `${RELEASE_BASE}/${tag}/${file}` };
  }

  const agentPackage = JSON.parse(fs.readFileSync(
    path.join(workspaceRoot, 'packages', 'web', 'node_modules', '@oh-my-pi', 'pi-coding-agent', 'package.json'),
    'utf8',
  ));
  const agentEngines = agentPackage.engines?.bun;
  if (agentEngines && agentEngines !== MINIMUM_VERSION) {
    throw new Error(`Manifest minimum ${MINIMUM_VERSION} drifted from pi-coding-agent engines.bun ${agentEngines} — update BUN_VERSION/MINIMUM_VERSION together`);
  }

  // pi_natives version riding with the pinned SDK (the natives packages are
  // version-locked to pi-coding-agent); the runtime downloads the platform
  // addon on first boot when the installer omits it (S3.5).
  const nativesVersion = agentPackage.dependencies?.['@oh-my-pi/pi-natives'];
  if (!nativesVersion) throw new Error('Cannot determine pi-natives version from pi-coding-agent dependencies');

  fs.mkdirSync(path.dirname(outputPath), { recursive: true });
  fs.writeFileSync(outputPath, `${JSON.stringify({
    version: BUN_VERSION,
    minimumVersion: MINIMUM_VERSION,
    nativesVersion,
    assets,
  }, null, 2)}\n`);
  console.log(`[electron] wrote bun runtime manifest: ${outputPath} (${BUN_VERSION}, ${Object.keys(assets).length} assets)`);
};

main().catch((error) => {
  console.error(error instanceof Error ? error.message : error);
  process.exit(1);
});
