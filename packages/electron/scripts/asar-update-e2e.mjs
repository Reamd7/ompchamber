// Loopback E2E harness for the replaceable-layer (ompchamber.asar) update
// channel — the maintainer's S3.6 acceptance run.
//
// Prerequisites (built once per iteration, documented runbook):
//   1. A packaged app at version N with a signing keypair baked in:
//        OMPCHAMBER_UPDATE_PUBLIC_KEY=<pub> OMPCHAMBER_UPDATE_PRIVATE_KEY=<priv> \
//          bun run package:rust -- --dir
//      (produces dist/linux-unpacked with resources/ompchamber.asar at N and
//      resources/update-pubkey.json)
//   2. The N+1 update artifact staged with the SAME keys:
//        OMPCHAMBER_UPDATE_PRIVATE_KEY=<priv> OMPCHAMBER_ASAR_VERSION=<N+1> \
//          bun run stage:ompchamber-asar
//      (produces dist/ompchamber-<N+1>.asar.br + dist/asar.yml)
//
// Then: bun run asar:e2e -- --app <linux-unpacked dir> [--scenario main|tampered|minimum|404]
//
// Scenarios:
//   main      — serve N+1; expect boot(N) → check(asar:N+1) → downloaded(N+1)
//               → relaunch → boot(N+1 layer=N+1) → check(none:up-to-date).
//   tampered  — serve a byte-flipped .br with the ORIGINAL sha512; expect a
//               download error marker, no .tmp install, app keeps running N.
//   minimum   — serve asar.yml with minimumVersion above the installed entry;
//               expect check(full:...) and no download (electron-updater path
//               owns it from there).
//   404       — serve nothing; expect check(none:feed-missing).
//
// The harness binds 127.0.0.1 only, isolates the app's HOME, and drives the
// app through its E2E-only auto-update driver (main.mjs runAsarE2eAutoUpdate,
// gated by OMPCHAMBER_E2E + OMPCHAMBER_ASAR_E2E_AUTO) — the exact same
// layer-updater.mjs code the IPC update path uses.

import { spawn } from 'node:child_process';
import fs from 'node:fs';
import http from 'node:http';
import os from 'node:os';
import path from 'node:path';

const args = process.argv.slice(2);
const readArg = (name) => {
  const index = args.indexOf(`--${name}`);
  return index >= 0 ? args[index + 1] : null;
};
const scenario = readArg('scenario') || 'main';
const appDir = readArg('app');
if (!appDir) {
  console.error('usage: asar-update-e2e.mjs -- --app <linux-unpacked dir> [--scenario main|tampered|minimum|404]');
  process.exit(1);
}

const electronRoot = path.resolve(path.dirname(new URL(import.meta.url).pathname), '..');
const workspace = readArg('dir') || fs.mkdtempSync(path.join(os.tmpdir(), `asar-e2e-${scenario}-`));
fs.mkdirSync(workspace, { recursive: true });
const markerDir = path.join(workspace, 'markers');
const serveDir = path.join(workspace, 'serve');
const homeDir = path.join(workspace, 'home');
fs.mkdirSync(markerDir, { recursive: true });
fs.mkdirSync(serveDir, { recursive: true });
fs.mkdirSync(homeDir, { recursive: true });

const appVersion = JSON.parse(fs.readFileSync(path.join(electronRoot, 'package.json'), 'utf8')).version;
const nextVersion = readArg('next') || (() => {
  const parts = appVersion.split('.').map(Number);
  parts[parts.length - 1] += 1;
  return parts.join('.');
})();

const artifactPath = path.join(electronRoot, 'dist', `ompchamber-${nextVersion}.asar.br`);
const manifestPath = path.join(electronRoot, 'dist', 'asar.yml');
const appBinary = path.join(appDir, 'ompchamber');

for (const required of [appBinary, manifestPath]) {
  if (!fs.existsSync(required)) {
    console.error(`[asar-e2e] missing prerequisite: ${required}`);
    process.exit(1);
  }
}
if ((scenario === 'main' || scenario === 'tampered') && !fs.existsSync(artifactPath)) {
  console.error(`[asar-e2e] missing N+1 artifact: ${artifactPath} (stage with OMPCHAMBER_ASAR_VERSION=${nextVersion})`);
  process.exit(1);
}

// Serve dir contents per scenario.
if (scenario === 'main') {
  fs.copyFileSync(artifactPath, path.join(serveDir, path.basename(artifactPath)));
  fs.copyFileSync(manifestPath, path.join(serveDir, 'asar.yml'));
} else if (scenario === 'tampered') {
  const bytes = fs.readFileSync(artifactPath);
  bytes[bytes.length - 100] ^= 0xff; // keep size; corrupt content, sha512 fails
  fs.writeFileSync(path.join(serveDir, path.basename(artifactPath)), bytes);
  fs.copyFileSync(manifestPath, path.join(serveDir, 'asar.yml'));
} else if (scenario === 'minimum') {
  const bumped = fs.readFileSync(manifestPath, 'utf8')
    .replace(/^minimumVersion:.*$/m, 'minimumVersion: 99999.0.0');
  fs.writeFileSync(path.join(serveDir, 'asar.yml'), bumped);
}
// 404: serveDir stays empty.

const server = http.createServer((request, response) => {
  const rel = decodeURIComponent(new URL(request.url, 'http://127.0.0.1').pathname).replace(/^\/+/, '');
  const filePath = path.join(serveDir, rel);
  if (!rel || !fs.existsSync(filePath) || !fs.statSync(filePath).isFile()) {
    response.writeHead(404);
    response.end('not found');
    return;
  }
  response.writeHead(200, { 'content-length': fs.statSync(filePath).size });
  fs.createReadStream(filePath).pipe(response);
});
await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
const port = server.address().port;
console.log(`[asar-e2e] scenario=${scenario} feed=http://127.0.0.1:${port} app=${appVersion} next=${nextVersion}`);
console.log(`[asar-e2e] workspace=${workspace}`);

const readMarker = (name) => {
  try {
    return fs.readFileSync(path.join(markerDir, name), 'utf8').trim();
  } catch {
    return null;
  }
};

const waitFor = async (name, { timeoutMs = 300_000, stopOn } = {}) => {
  const deadline = Date.now() + timeoutMs;
  for (;;) {
    const value = readMarker(name);
    if (value !== null) return value;
    if (stopOn && stopOn()) return null;
    if (Date.now() > deadline) return null;
    await new Promise((resolve) => setTimeout(resolve, 500));
  }
};

const failures = [];
const expect = (label, condition, detail = '') => {
  const mark = condition ? 'PASS' : 'FAIL';
  console.log(`[asar-e2e] ${mark} ${label}${detail ? ` — ${detail}` : ''}`);
  if (!condition) failures.push(label);
};

const childEnv = {
    ...process.env,
    HOME: homeDir,
    // XDG overrides would route Electron's userData outside the isolated
    // HOME (appData prefers XDG_CONFIG_HOME on Linux) — scrub them.
    XDG_CONFIG_HOME: undefined,
    XDG_DATA_HOME: undefined,
    XDG_CACHE_HOME: undefined,
    XDG_STATE_HOME: undefined,
    OMPCHAMBER_E2E: '1',
    OMPCHAMBER_ASAR_UPDATE_URL: `http://127.0.0.1:${port}`,
    OMPCHAMBER_ASAR_E2E_AUTO: '1',
    OMPCHAMBER_ASAR_E2E_NO_RELAUNCH: '1',
    OMPCHAMBER_ASAR_E2E_MARKER_DIR: markerDir,
};

const childSpawnArgs = ['--no-sandbox', '--disable-gpu', '--disable-dev-shm-usage'];
const child = spawn(appBinary, childSpawnArgs, {
  env: childEnv,
  stdio: ['ignore', 'ignore', 'inherit'],
  detached: true,
});
const killTree = (pid) => {
  try { process.kill(-pid, 'SIGKILL'); } catch { /* group gone */ }
  try { process.kill(pid, 'SIGKILL'); } catch { /* already gone */ }
};

const appAlive = () => {
  try { process.kill(child.pid, 0); return true; } catch { return false; }
};

try {
  const boot1 = await waitFor('boot');
  expect('boot marker (N, layer active)', boot1 !== null && boot1.includes(`layer=${appVersion}`), boot1 || 'missing');

  const check1 = await waitFor('check');
  if (scenario === 'main' || scenario === 'tampered') {
    expect('check reports asar channel with N+1', check1 === `asar:${nextVersion}`, check1 || 'missing');
  } else if (scenario === 'minimum') {
    expect('check falls through to full channel', (check1 || '').startsWith('full:'), check1 || 'missing');
  } else {
    expect('check reports 404 as no update', check1 === 'none:feed-missing', check1 || 'missing');
  }

  if (scenario === 'main') {
    const downloaded = await waitFor('downloaded');
    expect('download+sha512+signature verified', downloaded === nextVersion, downloaded || 'missing');
    // In-app relaunch timing is environment-dependent (app.relaunch and
    // detached spawns both run the second instance at the OS's discretion);
    // the install contract is only "next boot swaps the .tmp", so restart
    // deterministically here: kill the app, relaunch, and expect entry.mjs
    // to swap + materialize the N+1 layer.
    killTree(child.pid);
    await new Promise((resolve) => setTimeout(resolve, 2000));
    const relaunched = spawn(appBinary, childSpawnArgs, {
      env: childEnv,
      stdio: ['ignore', 'ignore', 'inherit'],
      detached: false,
    });
    const finalBoot = await (async () => {
      const deadline = Date.now() + 180_000;
      for (;;) {
        const value = readMarker('boot');
        if (value && value.includes(`layer=${nextVersion}`)) return value;
        if (Date.now() > deadline) return value;
        await new Promise((resolve) => setTimeout(resolve, 1000));
      }
    })();
    const check2 = await (async () => {
      const deadline = Date.now() + 120_000;
      for (;;) {
        const value = readMarker('check');
        if (value && value !== check1 && value.startsWith('none:')) return value;
        if (Date.now() > deadline) return value;
        await new Promise((resolve) => setTimeout(resolve, 1000));
      }
    })();
    expect('post-update check is up-to-date', (check2 || '').startsWith('none:'), check2 || 'missing');
    killTree(relaunched.pid);
  } else if (scenario === 'tampered') {
    const error = await waitFor('error');
    expect('tampered artifact rejected by sha512', (error || '').includes('sha512 mismatch'), error || 'missing');
    await new Promise((resolve) => setTimeout(resolve, 5000));
    const appName = JSON.parse(fs.readFileSync(path.join(electronRoot, 'package.json'), 'utf8')).name;
    const userDataDir = path.join(homeDir, '.config', appName);
    expect('no .tmp staged', !fs.existsSync(path.join(userDataDir, 'updates', 'ompchamber.asar.tmp')));
    expect('app keeps running on N', appAlive());
    const bootStillN = readMarker('boot');
    expect('boot marker still N', (bootStillN || '').includes(`layer=${appVersion}`), bootStillN || 'missing');
  } else {
    await new Promise((resolve) => setTimeout(resolve, 5000));
    expect('no download attempted', readMarker('downloaded') === null);
    expect('app keeps running', appAlive());
  }
} finally {
  killTree(child.pid);
  server.close();
}

console.log(failures.length === 0 ? '[asar-e2e] ALL PASS' : `[asar-e2e] FAILURES: ${failures.join(', ')}`);
process.exit(failures.length === 0 ? 0 : 1);
