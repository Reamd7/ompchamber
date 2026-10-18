// Replaceable-layer (ompchamber.asar) update channel.
//
// Dual-channel update policy (S3.4): the asar.yml feed is checked first.
//   - `asar`    : a newer replaceable layer exists AND the installed stable
//                 entry satisfies its minimumVersion → light update:
//                 download ompchamber-<ver>.asar.br, verify sha512 + the
//                 ed25519 signature whose public key ships in the STABLE
//                 layer (resources/update-pubkey.json → entry.mjs exports it
//                 as OMPCHAMBER_UPDATE_PUBLIC_KEY before the layer loads),
//                 then write <userData>/updates/ompchamber.asar.tmp. The
//                 next relaunch installs it atomically (entry swap).
//   - `full`    : minimumVersion exceeds the installed entry version → the
//                 stable layer is too old for light updates; defer to the
//                 existing electron-updater full channel (latest*.yml).
//   - `none`    : 404 from the feed = no update (same semantics as
//                 updater-check.mjs) or the remote version is not newer.
//   - `disabled`: no replaceable layer active (dev, js-variant single-asar
//                 builds) → the full channel is the only channel.
//
// The feed is zero-server: GitHub Releases `releases/latest/download` stable
// URLs (or a loopback base URL for the E2E harness via
// OMPCHAMBER_ASAR_UPDATE_URL).

import { createHash, createPublicKey, verify as cryptoVerify } from 'node:crypto';
import { brotliDecompress } from 'node:zlib';
import { promisify } from 'node:util';
import fs from 'node:fs';
import path from 'node:path';

const brotliDecompressAsync = promisify(brotliDecompress);

/** Flat `key: value` YAML subset parser (what stage-ompchamber-asar emits). */
export const parseAsarManifestYml = (text) => {
  const manifest = {};
  for (const line of String(text).split(/\r?\n/)) {
    const match = /^([A-Za-z0-9_]+):\s*(.*)$/.exec(line);
    if (!match) continue;
    const raw = match[2].trim();
    if (raw.startsWith('"') && raw.endsWith('"')) {
      try {
        manifest[match[1]] = JSON.parse(raw);
      } catch {
        manifest[match[1]] = raw.slice(1, -1);
      }
    } else {
      manifest[match[1]] = raw;
    }
  }
  return manifest;
};

/** Dotted-numeric compare: -1 | 0 | 1. */
export const compareLayerVersions = (a, b) => {
  const pa = String(a).split('.').map(Number);
  const pb = String(b).split('.').map(Number);
  for (let i = 0; i < Math.max(pa.length, pb.length); i++) {
    const delta = (pa[i] || 0) - (pb[i] || 0);
    if (delta) return delta > 0 ? 1 : -1;
  }
  return 0;
};

/**
 * Feed base: OMPCHAMBER_ASAR_UPDATE_URL override (E2E loopback) → the
 * GitHub Releases stable-URL prefix of the production feed.
 */
export const resolveAsarUpdateFeed = ({ environment = process.env, productionFeed } = {}) => {
  // The override is E2E-gated exactly like updater-feed.mjs: production
  // builds must never be steerable to an arbitrary feed URL by environment.
  const override = environment.OMPCHAMBER_E2E === '1'
    ? environment.OMPCHAMBER_ASAR_UPDATE_URL?.trim()
    : '';
  if (override) return override.replace(/\/+$/, '');
  const feed = productionFeed ?? { owner: 'Reamd7', repo: 'ompchamber' };
  return `https://github.com/${feed.owner}/${feed.repo}/releases/latest/download`;
};

/**
 * Fetch and classify the asar.yml feed. Never throws for offline/missing
 * feeds: those are `none` (or `disabled` without a layer), matching the
 * updater-check.mjs no-feed posture.
 */
export const checkAsarLayerUpdate = async ({ entryVersion, layerVersion, feedBaseUrl, fetchImpl = fetch }) => {
  if (!layerVersion) return { channel: 'disabled' };
  let response;
  try {
    response = await fetchImpl(`${feedBaseUrl}/asar.yml`, {
      redirect: 'follow',
      signal: AbortSignal.timeout(15_000),
    });
  } catch {
    return { channel: 'none', reason: 'feed-unreachable' };
  }
  if (response.status === 404) return { channel: 'none', reason: 'feed-missing' };
  if (!response.ok) return { channel: 'none', reason: `feed-http-${response.status}` };

  const manifest = parseAsarManifestYml(await response.text());
  if (!manifest.version || !manifest.file || !manifest.sha512) {
    return { channel: 'none', reason: 'feed-malformed' };
  }
  const minimumVersion = manifest.minimumVersion || '0.0.1';
  if (compareLayerVersions(minimumVersion, entryVersion) > 0) {
    return { channel: 'full', manifest };
  }
  if (compareLayerVersions(manifest.version, layerVersion) > 0) {
    return { channel: 'asar', manifest };
  }
  return { channel: 'none', reason: 'up-to-date', manifest };
};

/**
 * Download, verify, and stage the replaceable-layer artifact as
 * `<userData>/updates/ompchamber.asar.tmp`. Progress is reported through
 * `onProgress({ received, total })`. Throws on any verification failure —
 * a failed download must never leave a .tmp behind.
 */
export const downloadAsarLayerUpdate = async ({
  manifest,
  feedBaseUrl,
  updatesDir,
  publicKey,
  allowUnsigned = false,
  onProgress,
  fetchImpl = fetch,
}) => {
  fs.mkdirSync(updatesDir, { recursive: true });
  const tmpPath = path.join(updatesDir, 'ompchamber.asar.tmp');
  const partialPath = path.join(updatesDir, '.download.asar.br.partial');

  const signature = String(manifest.signature || '');
  if (!signature && !allowUnsigned) {
    throw new Error('Update artifact is unsigned and unsigned updates are not allowed');
  }

  const response = await fetchImpl(`${feedBaseUrl}/${manifest.file}`, { redirect: 'follow' });
  if (!response.ok) throw new Error(`Layer download failed (HTTP ${response.status})`);
  const total = Number(response.headers.get('content-length') || 0);
  const hash = createHash('sha512');
  const chunks = [];
  let received = 0;
  if (!response.body) throw new Error('Layer download returned no body');
  for await (const chunk of response.body) {
    hash.update(chunk);
    chunks.push(chunk);
    received += chunk.length;
    onProgress?.({ received, total });
  }
  const bytes = Buffer.concat(chunks);

  const digest = hash.digest('base64');
  if (digest !== manifest.sha512) {
    throw new Error(`Layer sha512 mismatch: expected ${manifest.sha512}, got ${digest}`);
  }
  if (signature) {
    if (!publicKey) throw new Error('Signed update received but no update public key is installed');
    let valid = false;
    try {
      const keyObject = createPublicKey({
        key: Buffer.from(publicKey, 'base64'),
        format: 'der',
        type: 'spki',
      });
      valid = cryptoVerify(null, bytes, keyObject, Buffer.from(signature, 'base64'));
    } catch {
      valid = false;
    }
    if (!valid) throw new Error('Layer signature verification failed');
  }

  // Fully verified bytes become the .tmp atomically; a crash mid-write can
  // The artifact travels brotli-compressed; the .tmp must hold the raw
  // asar bytes the entry can mount. sha512/signature above were over the
  // shipped .br bytes (what a tamperer would touch). Verified bytes become
  // the .tmp atomically; a crash mid-write can
  // only leave the .partial behind, which the next download overwrites.
  const asarBytes = await brotliDecompressAsync(bytes);
  fs.writeFileSync(partialPath, asarBytes);
  fs.rmSync(tmpPath, { force: true });
  fs.renameSync(partialPath, tmpPath);
  return { bytesWritten: asarBytes.length, tmpPath };
};
