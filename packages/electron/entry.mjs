// Dual-asar stable entry (app.asar main).
//
// Layout: app.asar holds only this launcher (+ baseline fallback bundle);
// the replaceable app surface — dist-bundle/main.mjs, preload.mjs, web-dist,
// omp-host (engine bundle; pi_natives arrives via the managed runtime),
// bun-runtime.json — ships inside ompchamber.asar (installer baseline under
// resources/, updated copy under <userData>/updates/). This entry:
//
//   1. installs a pending update: ompchamber.asar.tmp → ompchamber.asar
//      (the updater verified the bytes fully before writing the .tmp; the
//      same-directory rename is the atomic install point),
//   2. picks the active layer (updates copy wins over the read-only
//      installer baseline),
//   3. materializes the layer to <userData>/layer/<contentId>/ because two
//      consumers cannot read asar archives: the Rust server (plain fs reads
//      for web-dist) and the bun engine process (omp-host). Materialization
//      is write-once per content id (marker file) and keeps the newest two
//      layer dirs for rollback,
//   4. exports the layer via OMPCHAMBER_LAYER_* env keys and hands off to
//      the layer's dist-bundle/main.mjs.
//
// Fallbacks: a corrupt/missing layer falls back to the legacy single-asar
// layout (app.asar's own dist-bundle/main.mjs + resources/web-dist +
// resources/omp-host), which is exactly what the js-variant build ships —
// that variant never stages an ompchamber.asar. Dev (`electron ./main.mjs`)
// never runs through this entry at all.
//
// The update-signing public key is compiled into this STABLE layer (set the
// env var before the replaceable bundle loads so it cannot be forged by the
// code it validates).

import { app } from 'electron';
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const __dirname = path.dirname(fileURLToPath(import.meta.url));

/**
 * Ed25519 public key (base64/SPKI) verifying ompchamber-<ver>.asar.br
 * signatures from asar.yml. Built from CI secrets; the matching private key
 * never leaves CI. Rotation ships a new stable installer.
 */
const UPDATE_PUBLIC_KEY = process.env.OMPCHAMBER_BUILD_UPDATE_PUBLIC_KEY || '';

/** Stable-layer update key file written by stage-ompchamber-asar.mjs. */
const updateKeyCandidates = () => [
  process.resourcesPath ? path.join(process.resourcesPath, 'update-pubkey.json') : null,
  path.join(__dirname, 'resources', 'update-pubkey.json'),
].filter(Boolean);

const readUpdatePublicKey = () => {
  for (const candidate of updateKeyCandidates()) {
    try {
      const parsed = JSON.parse(fs.readFileSync(candidate, 'utf8'));
      if (parsed?.publicKey) return String(parsed.publicKey);
    } catch {
      // Absent or malformed: unsigned builds carry no key file.
    }
  }
  return '';
};

/** Update-channel bootstrap state the replaceable bundle reads via env. */
const layerPaths = () => {
  const userData = app.getPath('userData');
  return {
    updatesDir: path.join(userData, 'updates'),
    tmpAsar: path.join(userData, 'updates', 'ompchamber.asar.tmp'),
    activeAsar: path.join(userData, 'updates', 'ompchamber.asar'),
    baselineAsar: process.resourcesPath ? path.join(process.resourcesPath, 'ompchamber.asar') : null,
    layersRoot: path.join(userData, 'layer'),
  };
};

/** Install a pending update; a bad .tmp is dropped, never the live layer. */
const swapPendingLayer = async ({ tmpAsar, activeAsar }) => {
  if (!fs.existsSync(tmpAsar)) return false;
  // Validate before swapping: a .tmp that is not a readable layer (truncated
  // download, foreign bytes) must never replace a working update archive.
  // Electron's asar-aware fs only engages for *.asar paths, so the .tmp is
  // parsed with @electron/asar directly.
  const descriptorIsReadable = async () => {
    try {
      const { extractFile } = await import('@electron/asar');
      return Boolean(JSON.parse(extractFile(tmpAsar, 'layer.json').toString('utf8'))?.contentId);
    } catch {
      return false;
    }
  };
  if (!(await descriptorIsReadable())) {
    console.error('[entry] pending update .tmp is not a readable layer; discarding');
    fs.rmSync(tmpAsar, { force: true });
    return false;
  }
  try {
    fs.renameSync(tmpAsar, activeAsar);
    console.log('[entry] installed pending ompchamber.asar update');
    return true;
  } catch (error) {
    console.error('[entry] pending update swap failed; discarding .tmp:', error);
    fs.rmSync(tmpAsar, { force: true });
    return false;
  }
};

/** layer.json descriptor inside an asar (Electron's fs reads asar paths). */
const readLayerDescriptor = (asarPath) => {
  try {
    return JSON.parse(fs.readFileSync(path.join(asarPath, 'layer.json'), 'utf8'));
  } catch {
    return null;
  }
};

/** Extract an asar archive to a directory (pure-JS @electron/asar). */
const extractArchive = async (archive, destination) => {
  const { extractAll } = await import('@electron/asar');
  await extractAll(archive, destination);
};

/** Write-once materialization keyed by content id; keeps newest two dirs. */
const materializeLayer = async (asarPath, descriptor) => {
  const { layersRoot } = layerPaths();
  const layerDir = path.join(layersRoot, descriptor.contentId);
  if (fs.existsSync(path.join(layerDir, '.complete'))) return layerDir;

  const staging = path.join(layersRoot, `.staging-${descriptor.contentId}-${process.pid}`);
  fs.rmSync(staging, { recursive: true, force: true });
  fs.mkdirSync(staging, { recursive: true });
  try {
    await extractArchive(asarPath, staging);
    fs.writeFileSync(path.join(staging, '.complete'), String(Date.now()));
    fs.rmSync(layerDir, { recursive: true, force: true });
    fs.renameSync(staging, layerDir);
    pruneLayerDirs(layersRoot, layerDir);
    return layerDir;
  } catch (error) {
    fs.rmSync(staging, { recursive: true, force: true });
    console.error('[entry] layer materialization failed:', error);
    return null;
  }
};

/** Remove stale staging dirs and all but the newest two complete layers. */
const pruneLayerDirs = (layersRoot, keepDir) => {
  let entries;
  try {
    entries = fs.readdirSync(layersRoot);
  } catch {
    return;
  }
  for (const entry of entries) {
    if (entry.startsWith('.staging-')) {
      fs.rmSync(path.join(layersRoot, entry), { recursive: true, force: true });
    }
  }
  const complete = entries
    .filter((entry) => fs.existsSync(path.join(layersRoot, entry, '.complete')))
    .map((entry) => ({ entry, mtime: fs.statSync(path.join(layersRoot, entry, '.complete')).mtimeMs }))
    .sort((a, b) => b.mtime - a.mtime);
  for (const item of complete.slice(2)) {
    const dir = path.join(layersRoot, item.entry);
    if (dir === keepDir) continue;
    fs.rmSync(dir, { recursive: true, force: true });
  }
};

const resolveLayerBundle = async () => {
  if (!app.isPackaged) {
    return { mainUrl: pathToFileURL(path.join(__dirname, 'main.mjs')).href };
  }
  const paths = layerPaths();
  await swapPendingLayer(paths);

  const asarCandidates = [paths.activeAsar, paths.baselineAsar].filter(Boolean);
  for (const asarPath of asarCandidates) {
    if (!fs.existsSync(asarPath)) continue;
    const descriptor = readLayerDescriptor(asarPath);
    if (!descriptor?.contentId) continue;
    const layerDir = await materializeLayer(asarPath, descriptor);
    if (!layerDir) continue;
    process.env.OMPCHAMBER_LAYER_DIR = layerDir;
    process.env.OMPCHAMBER_LAYER_ASAR = asarPath;
    process.env.OMPCHAMBER_LAYER_VERSION = descriptor.version ?? '';
    const publicKey = UPDATE_PUBLIC_KEY || readUpdatePublicKey();
    if (publicKey) process.env.OMPCHAMBER_UPDATE_PUBLIC_KEY = publicKey;
    const mainPath = path.join(layerDir, 'dist-bundle', 'main.mjs');
    if (!fs.existsSync(mainPath)) {
      console.error(`[entry] layer ${asarPath} has no dist-bundle/main.mjs`);
      continue;
    }
    console.log(`[entry] using replaceable layer ${descriptor.version ?? 'unknown'} (${layerDir})`);
    return { mainUrl: pathToFileURL(mainPath).href };
  }

  // Legacy single-asar layout (js variant, or a rust build without a staged
  // layer): the app.asar bundle + resources/web-dist + resources/omp-host.
  console.log('[entry] no ompchamber.asar layer; booting legacy single-asar layout');
  return { mainUrl: pathToFileURL(path.join(app.getAppPath(), 'dist-bundle', 'main.mjs')).href };
};

const { mainUrl } = await resolveLayerBundle();
await import(mainUrl);
