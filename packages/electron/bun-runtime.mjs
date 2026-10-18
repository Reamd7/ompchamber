// Managed Bun runtime for the desktop app: download, verify, install, and
// resolve an official Bun binary inside the app's user data directory.
//
// The engine (omp host bundle) always runs under Bun; when the machine has no
// usable bun (>= pi-coding-agent engines.bun) the app installs one under
// `<userData>/runtimes/bun-<version>/`. Downloads are pinned and sha256-
// verified against `resources/bun-runtime.json` baked at build time
// (scripts/prepare-bun-runtime-manifest.mjs). Extraction is pure Node
// (adm-zip, already a workspace dependency) — the system `unzip` is never
// used. Install is atomic: a staging directory is fully populated and
// verified, then renamed into place, so a killed download can never leave a
// half-valid runtime behind. Old versions are pruned (keep current + one
// previous rollback copy).

import { spawnSync } from 'node:child_process';
import crypto from 'node:crypto';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

/** Compare two dotted numeric versions: -1 | 0 | 1. */
export const compareVersions = (a, b) => {
  const pa = String(a).split('.').map(Number);
  const pb = String(b).split('.').map(Number);
  for (let i = 0; i < Math.max(pa.length, pb.length); i++) {
    const delta = (pa[i] || 0) - (pb[i] || 0);
    if (delta) return delta > 0 ? 1 : -1;
  }
  return 0;
};

/** True when `version` satisfies a `>=x.y.z` minimum. */
export const satisfiesMinimum = (version, minimum) => {
  const match = /^>=\s*(\d+(?:\.\d+)*)$/.exec(String(minimum || '').trim());
  if (!match) return true;
  return compareVersions(version, match[1]) >= 0;
};


/**
 * Download fetch implementation: under Electron prefer net.fetch — Node's
 * undici fetch ignores http_proxy/HTTPS_PROXY env, which throttles direct
 * GitHub downloads to a crawl on proxied networks (Chromium's stack honors
 * system and env proxies on Linux/macOS).
 */
const resolveDownloadFetch = async () => {
  try {
    const electron = await import('electron');
    const netFetch = electron?.net?.fetch;
    if (netFetch) {
      return netFetch.bind(electron.net);
    }
  } catch {
    // Not under Electron (scripts/tests): global fetch.
  }
  return fetch;
};
const __dirname = path.dirname(fileURLToPath(import.meta.url));

/** Read the build-time pinned manifest (dev: package resources; packaged: app resources). */
export const readBunRuntimeManifest = () => {
  const candidates = [
    process.env.OMPCHAMBER_LAYER_DIR ? path.join(process.env.OMPCHAMBER_LAYER_DIR, 'bun-runtime.json') : null,
    process.resourcesPath ? path.join(process.resourcesPath, 'bun-runtime.json') : null,
    path.join(__dirname, 'resources', 'bun-runtime.json'),
  ].filter(Boolean);
  for (const candidate of candidates) {
    if (fs.existsSync(candidate)) return JSON.parse(fs.readFileSync(candidate, 'utf8'));
  }
  return null;
};

/** Root directory holding managed runtimes for a user-data directory. */
export const managedRuntimesRoot = (userDataDir) => path.join(userDataDir, 'runtimes');

/** Binary file name inside a managed runtime directory. */
const bunBinaryName = () => (process.platform === 'win32' ? 'bun.exe' : 'bun');

/** All installed managed runtimes (version-sorted, newest first) or []. */
export const listManagedBuns = (userDataDir) => {
  const root = managedRuntimesRoot(userDataDir);
  let entries;
  try {
    entries = fs.readdirSync(root);
  } catch {
    return [];
  }
  return entries
    .filter((entry) => /^bun-\d+(?:\.\d+)*$/.test(entry))
    .map((entry) => ({
      version: entry.slice('bun-'.length),
      dir: path.join(root, entry),
      binary: path.join(root, entry, bunBinaryName()),
    }))
    .sort((a, b) => compareVersions(b.version, a.version));
};

/**
 * Verify a bun binary answers `--version` with a string satisfying `minimum`.
 * Returns the reported version, or null (missing / not executable / too old /
 * spawn failure — never throws: callers decide between repair and fallback).
 */
export const verifyBunBinary = (binaryPath, minimum) => {
  if (!fs.existsSync(binaryPath)) return null;
  const result = spawnSync(binaryPath, ['--version'], {
    encoding: 'utf8',
    timeout: 15_000,
    windowsHide: true,
  });
  const version = (result.stdout || '').trim();
  if (result.status !== 0 || !/^\d+(?:\.\d+)*$/.test(version)) return null;
  return satisfiesMinimum(version, minimum) ? version : null;
};

/** Best valid managed bun for `manifest` (exact pin first, then any newer). */
export const findValidManagedBun = (userDataDir, manifest) => {
  if (!manifest) return null;
  for (const entry of listManagedBuns(userDataDir)) {
    if (compareVersions(entry.version, manifest.version) < 0) continue;
    const verified = verifyBunBinary(entry.binary, manifest.minimumVersion);
    if (verified) return { ...entry, verifiedVersion: verified, source: 'managed' };
  }
  return null;
};

/** Platform-arch key matching the manifest asset table (musl-aware on Linux). */
export const selectBunAssetKey = (platform = process.platform, arch = process.arch) => {
  if (platform === 'win32') return 'win32-x64';
  const archTag = arch === 'arm64' ? 'arm64' : 'x64';
  if (platform === 'darwin') return `darwin-${archTag}`;
  if (platform === 'linux') return `linux-${archTag}${isMuslLinux() ? '-musl' : ''}`;
  return null;
};

/** Linux libc detection: Alpine release file or `ldd` advertising musl. */
const isMuslLinux = () => {
  if (process.platform !== 'linux') return false;
  if (fs.existsSync('/etc/alpine-release')) return true;
  try {
    const ldd = spawnSync('ldd', ['--version'], { encoding: 'utf8', timeout: 5000, windowsHide: true });
    return /musl/i.test(`${ldd.stdout || ''}${ldd.stderr || ''}`);
  } catch {
    return false;
  }
};

/** Progress payloads surfaced to the UI (phase machine: download→verify→install→done). */
export const bunRuntimeProgress = (phase, fields = {}) => ({ phase, ...fields });

/**
 * Ensure a managed bun exists and is valid. Returns
 * `{ binary, version, source: 'managed', downloaded: boolean }`, or `null`
 * when the manifest/asset is unavailable for this platform or the download
 * failed (caller falls back to PATH bun with an actionable message).
 *
 * `onProgress(event)` receives `bunRuntimeProgress(...)` payloads; `signal`
 * aborts an in-flight download (partial staging is cleaned up).
 */
export const ensureManagedBunRuntime = async ({ userDataDir, manifest, onProgress, signal }) => {
  const existing = findValidManagedBun(userDataDir, manifest);
  if (existing) {
    return { binary: existing.binary, version: existing.verifiedVersion, source: 'managed', downloaded: false };
  }

  const assetKey = selectBunAssetKey();
  const asset = manifest?.assets?.[assetKey];
  if (!asset) return null;

  const root = managedRuntimesRoot(userDataDir);
  const installDir = path.join(root, `bun-${manifest.version}`);
  const stagingDir = path.join(root, `.staging-bun-${manifest.version}-${process.pid}-${Date.now()}`);
  fs.mkdirSync(stagingDir, { recursive: true });

  try {
    // 1) Download with byte-accurate progress.
    onProgress?.(bunRuntimeProgress('download', { version: manifest.version, received: 0, total: 0 }));
    const response = await (await resolveDownloadFetch())(asset.url, { redirect: 'follow', signal });
    if (!response.ok) throw new Error(`bun runtime download failed (HTTP ${response.status})`);
    const total = Number(response.headers.get('content-length') || 0);
    const zipPath = path.join(stagingDir, 'runtime.zip');
    // CJS interop: adm-zip's default export is the constructor under Node
    // and Bun; the named shape is kept for a future ESM build.
    const admZipModule = await import('adm-zip');
    const AdmZip = admZipModule.default ?? admZipModule.AdmZip;
    const hash = crypto.createHash('sha256');
    const out = fs.createWriteStream(zipPath);
    let received = 0;
    // Throttle progress to ~5Hz: emitting per network chunk floods every
    // window's IPC channel and starves the download loop itself.
    let lastEmitMs = 0;
    if (!response.body) throw new Error('bun runtime download returned no body');
    for await (const chunk of response.body) {
      hash.update(chunk);
      received += chunk.length;
      out.write(chunk);
      const now = Date.now();
      if (now - lastEmitMs >= 200) {
        lastEmitMs = now;
        onProgress?.(bunRuntimeProgress('download', { version: manifest.version, received, total }));
      }
    }
    await new Promise((resolve, reject) => out.end((error) => (error ? reject(error) : resolve())));

    // 2) sha256 pin verification.
    onProgress?.(bunRuntimeProgress('verify', { version: manifest.version }));
    const digest = hash.digest('hex');
    if (digest !== asset.sha256) throw new Error(`bun runtime sha256 mismatch: expected ${asset.sha256}, got ${digest}`);

    // 3) Extract only the runtime binaries (pure Node; no system unzip).
    onProgress?.(bunRuntimeProgress('install', { version: manifest.version }));
    const zip = new AdmZip(zipPath);
    const binaryName = bunBinaryName();
    const bunxName = process.platform === 'win32' ? 'bunx.exe' : 'bunx';
    for (const entry of zip.getEntries()) {
      const base = path.basename(entry.entryName);
      if ((base !== binaryName && base !== bunxName) || entry.isDirectory) continue;
      fs.writeFileSync(path.join(stagingDir, base), entry.getData());
    }
    const binaryPath = path.join(stagingDir, binaryName);
    if (!fs.existsSync(binaryPath)) throw new Error(`bun runtime zip has no ${binaryName} entry`);
    // The official zip stores `bunx` as a symlink to `bun`, which adm-zip
    // does not materialize — provide it as a real copy so scripts invoking
    // `bunx` inside the managed runtime keep working.
    const bunxPath = path.join(stagingDir, bunxName);
    if (!fs.existsSync(bunxPath)) fs.copyFileSync(binaryPath, bunxPath);
    for (const extracted of [binaryPath, bunxPath]) {
      if (process.platform !== 'win32') fs.chmodSync(extracted, 0o755);
    }
    if (process.platform === 'darwin') {
      // Belt-and-braces: files written by this process normally carry no
      // quarantine xattr, but clear it if Gatekeeper attribution appeared.
      for (const target of [binaryPath, bunxPath]) {
        spawnSync('xattr', ['-d', 'com.apple.quarantine', target], { windowsHide: true });
      }
    }
    fs.rmSync(zipPath, { force: true });

    // 4) Verify the staged binary before it becomes visible to resolution.
    const stagedBinary = path.join(stagingDir, bunBinaryName());
    const stagedVersion = verifyBunBinary(stagedBinary, manifest.minimumVersion);
    if (!stagedVersion) throw new Error('staged bun runtime failed --version verification');

    // 5) Atomic publish + prune (keep current and one previous).
    fs.rmSync(installDir, { recursive: true, force: true });
    fs.renameSync(stagingDir, installDir);
    pruneManagedBuns(root, manifest.version);
    onProgress?.(bunRuntimeProgress('done', { version: stagedVersion, binary: path.join(installDir, bunBinaryName()) }));
    return { binary: path.join(installDir, bunBinaryName()), version: stagedVersion, source: 'managed', downloaded: true };
  } catch (error) {
    fs.rmSync(stagingDir, { recursive: true, force: true });
    onProgress?.(bunRuntimeProgress('error', { version: manifest.version, message: error instanceof Error ? error.message : String(error) }));
    return null;
  }
};

/** Remove stale staging dirs and all but the newest `keep` version dirs. */
export const pruneManagedBuns = (root, currentVersion, keep = 2) => {
  let entries;
  try {
    entries = fs.readdirSync(root);
  } catch {
    return;
  }
  for (const entry of entries) {
    if (entry.startsWith('.staging-bun-') || entry.startsWith('.download-bun-')) {
      fs.rmSync(path.join(root, entry), { recursive: true, force: true });
    }
  }
  const versions = entries
    .filter((entry) => /^bun-\d+(?:\.\d+)*$/.test(entry))
    .map((entry) => ({ entry, version: entry.slice('bun-'.length) }))
    .sort((a, b) => compareVersions(b.version, a.version));
  for (const item of versions.slice(keep)) {
    if (item.version === currentVersion) continue;
    fs.rmSync(path.join(root, item.entry), { recursive: true, force: true });
  }
};

/** npm registry base: env override → project/user .npmrc → official. */
const readNpmRegistry = () => {
  const explicit = process.env.NPM_CONFIG_REGISTRY?.trim();
  if (explicit) return explicit.replace(/\/+$/, '');
  const configFiles = [
    path.join(__dirname, '..', '..', '.npmrc'),
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

/** Version marker written next to the addon so SDK bumps (delivered as a
 * light layer update carrying a new bun-runtime.json nativesVersion)
 * trigger an automatic refresh instead of leaving a stale addon for the
 * engine loader to reject. */
const NATIVES_VERSION_MARKER = 'pi-natives-version.json';

/** Installed addon version from the marker, or null (absent/legacy installs). */
export const readInstalledNativesVersion = (bunDir) => {
  try {
    const parsed = JSON.parse(fs.readFileSync(path.join(bunDir, NATIVES_VERSION_MARKER), 'utf8'));
    // Domain-field parse at this file-I/O boundary: only a real version
    // string counts; anything else (legacy marker, corruption) is "absent".
    const version = parsed?.version;
    return version ? String(version) : null;
  } catch {
    return null;
  }
};

/**
 * Ensure the pi_natives addon sits NEXT TO the managed bun binary. The
 * engine loader's non-compiled candidates are `<host.js dir>/../native` and
 * the bun executable's own directory (execDir) — with the addon stripped
 * from the installer (S3.5) the managed runtime directory is the only
 * writable candidate, so the first boot downloads the version-pinned npm
 * tarball (same channel as omp-host-natives.js) into it. Extraction uses
 * the system tar (present on every supported platform; the no-system-unzip
 * rule covers the bun zip, whose layout tar cannot read anyway).
 *
 * Version-aware: a no-op only when the installed marker matches the
 * manifest's nativesVersion; a mismatch (SDK bump via a light layer update)
 * removes the stale addon and downloads the pinned replacement before the
 * engine can observe it. Returns true when natives are ready (already or
 * after download); false when the download failed — the engine surfaces its
 * own actionable loader error naming the candidates it tried.
 */
export const ensureManagedBunNatives = async ({ bunDir, nativesVersion, onProgress }) => {
  const tag = { win32: 'win32', darwin: 'darwin', linux: 'linux' }[process.platform];
  if (!tag) return true;
  const addonFiles = () => (fs.existsSync(bunDir)
    ? fs.readdirSync(bunDir).filter((entry) => /^pi_natives\..+\.node$/.test(entry))
    : []);
  if (addonFiles().length > 0 && readInstalledNativesVersion(bunDir) === nativesVersion) return true;

  const packageName = `@oh-my-pi/pi-natives-${tag}-${process.arch}`;
  const unscoped = packageName.split('/').pop();
  const registry = readNpmRegistry();
  const tarballUrl = `${registry}/${packageName}/-/${unscoped}-${nativesVersion}.tgz`;
  onProgress?.(bunRuntimeProgress('natives-download', { version: nativesVersion, received: 0, total: 0 }));
  const tempDir = fs.mkdtempSync(path.join(os.tmpdir(), 'omp-natives-'));
  try {
    const response = await (await resolveDownloadFetch())(tarballUrl, { redirect: 'follow', signal: AbortSignal.timeout(600_000) });
    if (!response.ok) throw new Error(`pi_natives download failed (HTTP ${response.status}): ${tarballUrl}`);
    const total = Number(response.headers.get('content-length') || 0);
    let received = 0;
    const tarballPath = path.join(tempDir, 'natives.tgz');
    const out = fs.createWriteStream(tarballPath);
    let lastEmitMs = 0;
    for await (const chunk of response.body) {
      received += chunk.length;
      out.write(chunk);
      const now = Date.now();
      if (now - lastEmitMs >= 200) {
        lastEmitMs = now;
        onProgress?.(bunRuntimeProgress('natives-download', { version: nativesVersion, received, total }));
      }
    }
    await new Promise((resolve, reject) => out.end((error) => (error ? reject(error) : resolve())));
    const extract = spawnSync('tar', ['-xzf', tarballPath, '-C', tempDir], { windowsHide: true });
    if (extract.status !== 0) throw new Error(`pi_natives tar extract failed (${extract.status})`);
    const files = fs.readdirSync(path.join(tempDir, 'package')).filter((entry) => /^pi_natives\..+\.node$/.test(entry));
    if (files.length === 0) throw new Error(`pi_natives tarball ${packageName}@${nativesVersion} has no addon`);
    // Replace, don't merge: a stale addon from an older SDK must not survive
    // (the loader rejects foreign versions at boot with a hard error).
    for (const stale of addonFiles()) fs.rmSync(path.join(bunDir, stale), { force: true });
    for (const file of files) {
      fs.copyFileSync(path.join(tempDir, 'package', file), path.join(bunDir, file));
    }
    fs.writeFileSync(path.join(bunDir, NATIVES_VERSION_MARKER), `${JSON.stringify({ version: nativesVersion }, null, 2)}\n`);
    onProgress?.(bunRuntimeProgress('natives-done', { version: nativesVersion, files: files.length }));
    console.log(`[electron] staged pi_natives ${packageName}@${nativesVersion}: ${files.join(', ')}`);
    return true;
  } catch (error) {
    onProgress?.(bunRuntimeProgress('error', { phase: 'natives', message: error instanceof Error ? error.message : String(error) }));
    return false;
  } finally {
    fs.rmSync(tempDir, { recursive: true, force: true });
  }
};
