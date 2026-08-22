import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import test from 'node:test';

import { linuxAppImageArchSuffix, readElfArchitecture, verifyExtractedPayload } from './verify-linux-appimage.mjs';

const writeElf = (filePath, architecture) => {
  fs.mkdirSync(path.dirname(filePath), { recursive: true });
  const header = Buffer.alloc(20);
  header.set([0x7f, 0x45, 0x4c, 0x46, 2, 1]);
  header.writeUInt16LE(architecture === 'x64' ? 62 : 183, 18);
  fs.writeFileSync(filePath, header, { mode: 0o755 });
};

const createPayload = () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'openchamber-payload-test-'));
  fs.writeFileSync(path.join(root, 'ompchamber.desktop'), [
    '[Desktop Entry]', 'Name=OMPChamber', 'Exec=AppRun --no-sandbox %U', 'Icon=ompchamber', 'StartupWMClass=ompchamber', '',
  ].join('\n'));
  writeElf(path.join(root, 'ompchamber'), 'x64');
  writeElf(path.join(root, 'resources/omp-host/omp-host'), 'x64');
  for (const name of ['pty.node', 'sherpa-onnx.node']) {
    writeElf(path.join(root, 'resources/app.asar.unpacked/node_modules', name), 'x64');
  }
  // onnxruntime-style packages stage every platform; only Linux binaries
  // may be parsed, foreign Mach-O/PE copies must be ignored.
  writeElf(path.join(root, 'resources/app.asar.unpacked/node_modules/onnxruntime-node/bin/napi-v6/linux/x64/onnxruntime_binding.node'), 'x64');
  writeElf(path.join(root, 'resources/app.asar.unpacked/node_modules/onnxruntime-node/bin/napi-v6/linux/arm64/onnxruntime_binding.node'), 'arm64');
  fs.mkdirSync(path.join(root, 'resources/app.asar.unpacked/node_modules/onnxruntime-node/bin/napi-v6/darwin/arm64'), { recursive: true });
  fs.writeFileSync(path.join(root, 'resources/app.asar.unpacked/node_modules/onnxruntime-node/bin/napi-v6/darwin/arm64/onnxruntime_binding.node'), Buffer.from([0xcf, 0xfa, 0xed, 0xfe]));
  return root;
};

test('reads supported ELF architectures', () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'openchamber-elf-test-'));
  try {
    writeElf(path.join(root, 'x64'), 'x64');
    writeElf(path.join(root, 'arm64'), 'arm64');
    assert.equal(readElfArchitecture(path.join(root, 'x64')), 'x64');
    assert.equal(readElfArchitecture(path.join(root, 'arm64')), 'arm64');
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }
});

test('AppImage artifact names use electron-builder arch suffixes', () => {
  assert.equal(linuxAppImageArchSuffix('x64'), 'x86_64');
  assert.equal(linuxAppImageArchSuffix('arm64'), 'arm64');
});

test('ignores foreign-platform onnxruntime binaries and checks the Linux copy', () => {
  const root = createPayload();
  try {
    const result = verifyExtractedPayload({
      root,
      targetArchitecture: 'x64',
    });
    assert.equal(result.nativeModuleCount, 3);
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }
});

test('fails on a missing native module', () => {
  const root = createPayload();
  try {
    fs.rmSync(path.join(root, 'resources/app.asar.unpacked/node_modules/pty.node'));
    assert.throws(() => verifyExtractedPayload({
      root,
      targetArchitecture: 'x64',
    }), /Missing packaged native module: pty\.node/);
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }
});

test('fails on wrong host or native architecture', () => {
  const root = createPayload();
  try {
    writeElf(path.join(root, 'resources/omp-host/omp-host'), 'arm64');
    assert.throws(() => verifyExtractedPayload({
      root,
      targetArchitecture: 'x64',
    }), /omp host/);
    writeElf(path.join(root, 'resources/omp-host/omp-host'), 'x64');
    writeElf(path.join(root, 'resources/app.asar.unpacked/node_modules/pty.node'), 'arm64');
    assert.throws(() => verifyExtractedPayload({
      root,
      targetArchitecture: 'x64',
    }), /Native module architecture mismatch/);
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }
});
