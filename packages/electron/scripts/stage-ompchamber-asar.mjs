// Stage the replaceable ompchamber.asar layer for rust-variant desktop builds.
//
// Layer contents: dist-bundle/main.mjs (self-contained except `electron` and
// the backend-gated dynamic @ompchamber/web import), preload.mjs, web-dist,
// omp-host/dist (host.js + sidecar assets — pi_natives is EXCLUDED, see
// below), and bun-runtime.json. Outputs:
//
//   resources/ompchamber.asar                  — installer baseline (extraResources)
//   dist/ompchamber-<version>.asar.br          — brotli-compressed update artifact
//   dist/asar.yml                              — update manifest
//     (version, minimumVersion, sha512 of the .br, ed25519 signature)
//   resources/update-pubkey.json               — public key for entry.mjs
//
// pi_natives exclusion (S3.5): the engine loader reads the addon from the bun
// executable's directory, so the first boot downloads it next to the managed
// runtime instead of shipping 219MB per platform in the installer AND every
// update artifact — which also makes the artifact platform-neutral (one
// .br + asar.yml serves mac/win/linux from releases/latest/download).
//
// The signature key: set OMPCHAMBER_UPDATE_PRIVATE_KEY (base64 PKCS8). CI
// holds it in secrets; the matching public key (OMPCHAMBER_UPDATE_PUBLIC_KEY,
// base64 SPKI) is baked here for entry.mjs. Without a private key (local dev
// builds) the manifest carries an empty signature and the updater's verify
// step rejects it — fixture builds pass the key explicitly.
//
// The content id hashes the layer INPUTS plus the version (sorted relpath +
// size + file digest), so it is stable across repacking, differs between
// releases, and names the materialized <userData>/layer/<contentId>/
// directory without a second build pass.

import { createHash, createPrivateKey, sign as cryptoSign } from 'node:crypto';
import { brotliCompress, constants as zlibConstants } from 'node:zlib';
import { promisify } from 'node:util';
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const electronRoot = path.resolve(__dirname, '..');

const brotliCompressAsync = promisify(brotliCompress);

const collectFiles = (dir, prefix = '') => {
  const files = [];
  for (const entry of fs.readdirSync(dir, { withFileTypes: true }).sort((a, b) => a.name.localeCompare(b.name))) {
    const rel = prefix ? `${prefix}/${entry.name}` : entry.name;
    if (entry.isDirectory()) {
      files.push(...collectFiles(path.join(dir, entry.name), rel));
    } else if (entry.isFile()) {
      files.push(rel);
    }
  }
  return files;
};

/** Stable content id over layer inputs + version: relpath + size + sha256. */
const computeContentId = (layerRoot, version) => {
  const hash = createHash('sha256');
  // The layer version participates in the id: materialization caches by
  // contentId, and two releases of identical file content must not alias to
  // the same extracted directory.
  hash.update(`ompchamber-asar/${version}\n`);
  for (const rel of collectFiles(layerRoot)) {
    const filePath = path.join(layerRoot, rel);
    const stat = fs.statSync(filePath);
    hash.update(rel);
    hash.update(String(stat.size));
    hash.update(createHash('sha256').update(fs.readFileSync(filePath)).digest('hex'));
  }
  return hash.digest('hex').slice(0, 32);
};

const copyDir = (from, to) => {
  fs.mkdirSync(path.dirname(to), { recursive: true });
  fs.cpSync(from, to, { recursive: true });
};

const main = async () => {
  const pkg = JSON.parse(fs.readFileSync(path.join(electronRoot, 'package.json'), 'utf8'));
  // OMPCHAMBER_ASAR_VERSION overrides the layer/artifact version (E2E: stage
  // an N+1 update artifact against an installed N app without touching
  // package.json).
  const version = process.env.OMPCHAMBER_ASAR_VERSION?.trim() || pkg.version;
  const staging = path.join(electronRoot, '.cache', 'ompchamber-asar-staging');
  fs.rmSync(staging, { recursive: true, force: true });
  fs.mkdirSync(path.join(staging, 'dist-bundle'), { recursive: true });

  const inputs = [
    [path.join(electronRoot, 'dist-bundle', 'main.mjs'), path.join(staging, 'dist-bundle', 'main.mjs')],
    [path.join(electronRoot, 'preload.mjs'), path.join(staging, 'preload.mjs')],
    [path.join(electronRoot, 'resources', 'web-dist'), path.join(staging, 'web-dist')],
    // pi_natives is excluded (S3.5): the loader reads it from the bun
    // executable's directory, so the first boot downloads the addon next to
    // the managed runtime instead of shipping 219MB per platform in the
    // installer AND every update artifact.
    [path.join(electronRoot, 'resources', 'omp-host', 'dist'), path.join(staging, 'omp-host', 'dist')],
    [path.join(electronRoot, 'resources', 'bun-runtime.json'), path.join(staging, 'bun-runtime.json')],
  ];
  for (const [from, to] of inputs) {
    if (!fs.existsSync(from)) throw new Error(`ompchamber.asar input missing: ${from}`);
    copyDir(from, to);
  }

  const contentId = computeContentId(staging, version);
  fs.writeFileSync(
    path.join(staging, 'layer.json'),
    `${JSON.stringify({ version, contentId }, null, 2)}\n`,
  );

  const { createPackage } = await import('@electron/asar');
  // Artifact-only mode (OMPCHAMBER_ASAR_VERSION override, E2E): stage the
  // dist update artifacts WITHOUT touching the installer baseline or the
  // pubkey file — a later baseline stage owns those.
  const artifactOnly = Boolean(process.env.OMPCHAMBER_ASAR_VERSION?.trim());
  const baselineAsar = path.join(electronRoot, 'resources', 'ompchamber.asar');
  const packageAsar = artifactOnly ? path.join(staging, 'artifact.asar') : baselineAsar;
  fs.rmSync(packageAsar, { force: true });
  await createPackage(staging, packageAsar);
  const asarBytes = fs.readFileSync(packageAsar);

  // Quality is env-tunable: q11 on a large asar is single-threaded and can
  // take tens of minutes; CI uses the default 9, local E2E runs pass 4.
  const quality = Number(process.env.OMPCHAMBER_BROTLI_QUALITY || 9);
  const compressed = await brotliCompressAsync(asarBytes, {
    params: { [zlibConstants.BROTLI_PARAM_QUALITY]: quality },
  });
  const artifactDir = path.join(electronRoot, 'dist');
  fs.mkdirSync(artifactDir, { recursive: true });
  const artifactName = `ompchamber-${version}.asar.br`;
  const artifactPath = path.join(artifactDir, artifactName);
  fs.writeFileSync(artifactPath, compressed);

  const sha512 = createHash('sha512').update(compressed).digest('base64');
  const privateKey = process.env.OMPCHAMBER_UPDATE_PRIVATE_KEY?.trim();
  // ed25519 signs the raw bytes (no separate digest); the updater verifies
  // with crypto.verify(null, bytes, publicKey, signature).
  const signature = privateKey
    ? cryptoSign(null, compressed, createPrivateKey({
      key: Buffer.from(privateKey, 'base64'),
      format: 'der',
      type: 'pkcs8',
    })).toString('base64')
    : '';
  const manifest = {
    version,
    minimumVersion: process.env.OMPCHAMBER_ASAR_MINIMUM_VERSION || '0.0.1',
    file: artifactName,
    sha512,
    signature,
  };

  if (artifactOnly) fs.rmSync(packageAsar, { force: true });

  // Stable-layer update key: exported by CI as OMPCHAMBER_UPDATE_PUBLIC_KEY
  // Stable-layer update key: exported by CI as OMPCHAMBER_UPDATE_PUBLIC_KEY
  // (base64 SPKI), baked here for entry.mjs to hand the layer at boot.
  // Always written (empty key on unsigned builds) so the extraResources
  // entry resolves on every build; entry.mjs ignores an empty key.
  const publicKey = process.env.OMPCHAMBER_UPDATE_PUBLIC_KEY?.trim() || '';
  fs.writeFileSync(
    path.join(electronRoot, 'resources', 'update-pubkey.json'),
    `${JSON.stringify({ publicKey }, null, 2)}\n`,
  );

  // Flat `key: value` YAML subset — parseable without a yaml dependency
  // (same no-dep posture as updater-check.mjs).
  const manifestPath = path.join(artifactDir, 'asar.yml');
  fs.writeFileSync(manifestPath, [
    `version: ${manifest.version}`,
    `minimumVersion: ${manifest.minimumVersion}`,
    `file: ${manifest.file}`,
    `sha512: ${manifest.sha512}`,
    `signature: ${JSON.stringify(manifest.signature)}`,
    '',
  ].join('\n'));

  fs.rmSync(staging, { recursive: true, force: true });
  const sizeMb = (bytes) => (bytes / 1024 / 1024).toFixed(1);
  console.log(
    `[electron] ompchamber.asar staged: ${sizeMb(asarBytes.length)}MB (asar) / ${sizeMb(compressed.length)}MB (.br),`
    + ` version=${version} contentId=${contentId}${signature ? ' signed' : ' UNSIGNED'}`,
  );
  console.log(`[electron] update artifacts: ${artifactPath}, ${manifestPath}`);
};

main().catch((error) => {
  console.error(error instanceof Error ? error.message : error);
  process.exit(1);
});
