import assert from 'node:assert/strict';
import { generateKeyPairSync, createHash, sign as cryptoSign } from 'node:crypto';
import { brotliCompress } from 'node:zlib';
import { promisify } from 'node:util';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import http from 'node:http';
import test from 'node:test';

import {
  checkAsarLayerUpdate,
  compareLayerVersions,
  downloadAsarLayerUpdate,
  parseAsarManifestYml,
  resolveAsarUpdateFeed,
} from './layer-updater.mjs';

test('parses the flat manifest yaml subset', () => {
  const manifest = parseAsarManifestYml([
    'version: 2.0.0',
    'minimumVersion: 1.5.0',
    'file: ompchamber-2.0.0.asar.br',
    'sha512: abc=',
    'signature: "sig with spaces=="',
    '# comment line is ignored because it has no key',
    '',
  ].join('\n'));
  assert.equal(manifest.version, '2.0.0');
  assert.equal(manifest.minimumVersion, '1.5.0');
  assert.equal(manifest.file, 'ompchamber-2.0.0.asar.br');
  assert.equal(manifest.sha512, 'abc=');
  assert.equal(manifest.signature, 'sig with spaces==');
});

test('compares dotted versions numerically', () => {
  assert.equal(compareLayerVersions('1.10.0', '1.9.9'), 1);
  assert.equal(compareLayerVersions('1.3.14', '1.3.14'), 0);
  assert.equal(compareLayerVersions('0.99', '1.0'), -1);
});

test('feed override is E2E-gated and production uses the stable release URL', () => {
  const production = resolveAsarUpdateFeed({ environment: { OMPCHAMBER_ASAR_UPDATE_URL: 'http://127.0.0.1:9/x/' } });
  assert.equal(production, 'https://github.com/Reamd7/ompchamber/releases/latest/download');
  const overridden = resolveAsarUpdateFeed({
    environment: { OMPCHAMBER_E2E: '1', OMPCHAMBER_ASAR_UPDATE_URL: 'http://127.0.0.1:9/x/' },
  });
  assert.equal(overridden, 'http://127.0.0.1:9/x');
});

const withFeedServer = async (handler) => {
  const server = http.createServer(handler);
  await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
  return { server, baseUrl: `http://127.0.0.1:${server.address().port}` };
};

test('classifies disabled/none/asar/full from the feed', async () => {
  // No layer active (single-asar build) → disabled without touching the feed.
  assert.equal((await checkAsarLayerUpdate({ entryVersion: '1.0.0', layerVersion: '', feedBaseUrl: 'http://127.0.0.1:1' })).channel, 'disabled');

  const manifest = [
    'version: 2.0.0',
    'minimumVersion: 1.0.0',
    'file: ompchamber-2.0.0.asar.br',
    'sha512: abc=',
  ].join('\n');

  const cases = [
    { status: 404, body: '', entry: '1.0.0', layer: '1.0.0', expect: 'none' },
    { status: 200, body: manifest, entry: '1.0.0', layer: '1.0.0', expect: 'asar' },
    { status: 200, body: manifest, entry: '2.0.0', layer: '2.0.0', expect: 'none' },
  ];
  for (const testCase of cases) {
    const { server, baseUrl } = await withFeedServer((request, response) => {
      response.writeHead(testCase.status);
      response.end(testCase.body);
    });
    try {
      const result = await checkAsarLayerUpdate({ entryVersion: testCase.entry, layerVersion: testCase.layer, feedBaseUrl: baseUrl });
      assert.equal(result.channel, testCase.expect, JSON.stringify(testCase));
    } finally {
      server.close();
    }
  }

  // minimumVersion above the installed entry forces the full channel.
  const bumped = manifest.replace('minimumVersion: 1.0.0', 'minimumVersion: 9.0.0');
  const { server, baseUrl } = await withFeedServer((request, response) => {
    response.writeHead(200);
    response.end(bumped);
  });
  try {
    const result = await checkAsarLayerUpdate({ entryVersion: '1.0.0', layerVersion: '1.0.0', feedBaseUrl: baseUrl });
    assert.equal(result.channel, 'full');
  } finally {
    server.close();
  }
});

const brotliCompressAsync = promisify(brotliCompress);

test('download verifies sha512 + signature, decompresses, and stages the tmp atomically', async () => {
  const { publicKey, privateKey } = generateKeyPairSync('ed25519');
  const publicKeyBase64 = publicKey.export({ type: 'spki', format: 'der' }).toString('base64');
  const asarPayload = Buffer.from('fake decompressed asar bytes for verification tests');
  // The wire artifact is the brotli-compressed archive (what stage emits).
  const payload = await brotliCompressAsync(asarPayload);

  const sha512 = createHash('sha512').update(payload).digest('base64');
  const signature = cryptoSign(null, payload, privateKey).toString('base64');
  const manifest = { version: '2.0.0', file: 'x.asar.br', sha512, signature };

  const { server, baseUrl } = await withFeedServer((request, response) => {
    response.writeHead(200, { 'content-length': payload.length });
    response.end(payload);
  });
  const updatesDir = fs.mkdtempSync(path.join(os.tmpdir(), 'layer-updates-'));
  try {
    const result = await downloadAsarLayerUpdate({ manifest, feedBaseUrl: baseUrl, updatesDir, publicKey: publicKeyBase64 });
    assert.equal(result.bytesWritten, asarPayload.length);
    assert.deepEqual(fs.readFileSync(path.join(updatesDir, 'ompchamber.asar.tmp')), asarPayload);
    assert.equal(fs.existsSync(path.join(updatesDir, '.download.asar.br.partial')), false);
  } finally {
    server.close();
    fs.rmSync(updatesDir, { recursive: true, force: true });
  }
});

test('download rejects tampered bytes, bad signatures, and unsigned artifacts', async () => {
  const { publicKey, privateKey } = generateKeyPairSync('ed25519');
  const publicKeyBase64 = publicKey.export({ type: 'spki', format: 'der' }).toString('base64');
  const payload = Buffer.from('payload');
  const goodSha = createHash('sha512').update(payload).digest('base64');
  const goodSig = cryptoSign(null, payload, privateKey).toString('base64');

  const { server, baseUrl } = await withFeedServer((request, response) => {
    response.writeHead(200, { 'content-length': payload.length });
    response.end(payload);
  });
  const updatesDir = fs.mkdtempSync(path.join(os.tmpdir(), 'layer-updates-'));
  try {
    await assert.rejects(
      () => downloadAsarLayerUpdate({ manifest: { file: 'x', sha512: 'bad=' }, feedBaseUrl: baseUrl, updatesDir, publicKey: publicKeyBase64, allowUnsigned: true }),
      /sha512 mismatch/,
    );
    await assert.rejects(
      () => downloadAsarLayerUpdate({ manifest: { file: 'x', sha512: goodSha, signature: Buffer.from('not-a-sig').toString('base64') }, feedBaseUrl: baseUrl, updatesDir, publicKey: publicKeyBase64 }),
      /signature verification failed/,
    );
    await assert.rejects(
      () => downloadAsarLayerUpdate({ manifest: { file: 'x', sha512: goodSha }, feedBaseUrl: baseUrl, updatesDir, publicKey: publicKeyBase64 }),
      /unsigned/,
    );
    // A foreign keypair must not verify either.
    const { privateKey: otherPrivate } = generateKeyPairSync('ed25519');
    const foreignSig = cryptoSign(null, payload, otherPrivate).toString('base64');
    await assert.rejects(
      () => downloadAsarLayerUpdate({ manifest: { file: 'x', sha512: goodSha, signature: foreignSig }, feedBaseUrl: baseUrl, updatesDir, publicKey: publicKeyBase64 }),
      /signature verification failed/,
    );
    // Failures must never leave a staged tmp behind.
    assert.equal(fs.existsSync(path.join(updatesDir, 'ompchamber.asar.tmp')), false);
  } finally {
    server.close();
    fs.rmSync(updatesDir, { recursive: true, force: true });
  }
});
