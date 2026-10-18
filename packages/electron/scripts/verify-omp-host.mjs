// Verify the staged/packaged omp host bundle for desktop builds.
//
// --staged   check resources/omp-host/dist in the workspace (post prepare)
// --packaged check the bundle inside a packaged app directory

import { spawn } from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const electronRoot = path.resolve(__dirname, '..');
const mode = process.argv.includes('--packaged') ? 'packaged' : 'staged';

const hostBundleEntry = path.join('dist', 'host.js');
const appBundleName = 'OMPChamber.app';
const candidates =
  mode === 'staged'
    ? [path.join(electronRoot, 'resources', 'omp-host', hostBundleEntry)]
    : [
        // In-app invocation (Electron sets process.resourcesPath).
        path.join(process.resourcesPath ?? '', 'omp-host', hostBundleEntry),
        // electron-builder unpacked outputs for each desktop platform.
        path.join(electronRoot, 'dist', 'win-unpacked', 'resources', 'omp-host', hostBundleEntry),
        path.join(electronRoot, 'dist', 'win-arm64-unpacked', 'resources', 'omp-host', hostBundleEntry),
        path.join(electronRoot, 'dist', 'linux-unpacked', 'resources', 'omp-host', hostBundleEntry),
        path.join(electronRoot, 'dist', 'linux-arm64-unpacked', 'resources', 'omp-host', hostBundleEntry),
        path.join(electronRoot, 'dist', 'mac', appBundleName, 'Contents', 'Resources', 'omp-host', hostBundleEntry),
        path.join(electronRoot, 'dist', 'mac-arm64', appBundleName, 'Contents', 'Resources', 'omp-host', hostBundleEntry),
        // The replaceable layer materializes omp-host next to web-dist; a
        // packaged app that already booted once serves it from there.
        process.env.OMPCHAMBER_LAYER_DIR ? path.join(process.env.OMPCHAMBER_LAYER_DIR, 'omp-host', hostBundleEntry) : null,
      ].filter(Boolean);

const hostJs = candidates.find((candidate) => fs.existsSync(candidate));
if (!hostJs) {
  console.error(`[electron] omp host bundle not found (${mode}): ${candidates.join(', ')}`);
  process.exit(1);
}

// The bundle is plain JS and always boots under a Bun runtime (explicit env
// → current process under Bun → PATH), mirroring resolveOmpHostRuntimeBinary.
const runtimeBinary = process.env.OMPCHAMBER_OMP_HOST_RUNTIME?.trim()
  || (process.versions?.bun ? process.execPath : 'bun');

// pi_natives policy: staged dev layouts must carry the addon in the native/
// sibling of dist/ (the bundled loader resolves <dist>/../native). Packaged
// S3.5 builds ship no addon — it downloads next to the managed bun at first
// boot — so they only require the manifest to pin a nativesVersion.
const nativesDir = path.join(path.dirname(path.dirname(hostJs)), 'native');
const nativesPresent = fs.existsSync(nativesDir)
  && fs.readdirSync(nativesDir).some((entry) => /^pi_natives\..+\.node$/.test(entry));
const nativesIntentionallyAbsent = mode === 'packaged' || process.env.OMPCHAMBER_STAGE_NATIVES === '0';
if (!nativesPresent && !nativesIntentionallyAbsent) {
  console.error(`[electron] omp host natives missing next to the staged bundle: ${nativesDir}`);
  process.exit(1);
}
if (!nativesPresent && nativesIntentionallyAbsent) {
  const manifestPath = path.join(path.dirname(path.dirname(path.dirname(hostJs))), 'bun-runtime.json');
  const manifest = fs.existsSync(manifestPath) ? JSON.parse(fs.readFileSync(manifestPath, 'utf8')) : null;
  if (!manifest?.nativesVersion) {
    console.error(`[electron] packaged build ships no pi_natives and bun-runtime.json pins no nativesVersion: ${manifestPath}`);
    process.exit(1);
  }
  console.log(`[electron] packaged build expects first-boot pi_natives ${manifest.nativesVersion} (managed runtime)`);
  // Without the addon the engine cannot boot here by design (it arrives via
  // the managed runtime at first app launch): bundle + manifest pin verified;
  // live boot stays covered by first-boot flows and the asar-update e2e
  // harness. Dev layouts and js-variant builds keep the live boot below.
  console.log('[electron] skipping live boot (natives-free layout)');
  process.exit(0);
}

// Boot the host on a loopback port, poll /global/health until it reports
// healthy, then stop it. This proves the bundled engine actually serves.
const port = 3997;
const healthCheckTimeoutMs = 30_000;

const child = spawn(runtimeBinary, [hostJs, 'serve', '--hostname', '127.0.0.1', '--port', String(port)], {
  stdio: ['ignore', 'ignore', 'pipe'],
  windowsHide: true,
  // The desktop app launches the host with cwd at the user's home, far from
  // any node_modules. Booting from the workspace instead let Bun's resolver
  // find packages through cwd and hid a compiled-only startup hang (the
  //1.32.0 processes.v1 regression) behind a green verification.
  cwd: os.tmpdir(),
  // The verification protocol below polls without credentials; an ambient
  // OPENCODE_SERVER_PASSWORD (e.g. packaging run inside the desktop app)
  // would 401 every probe until timeout. Verify the no-auth boot shape.
  env: { ...process.env, OPENCODE_SERVER_PASSWORD: '' },
});
child.unref();

let stderrTail = '';
child.stderr?.on('data', (chunk) => {
  stderrTail = (stderrTail + String(chunk)).slice(-2000);
});

const stopChild = () => {
  try {
    process.kill(child.pid);
  } catch {
    // Already gone.
  }
};

const finish = (code) => {
  stopChild();
  setTimeout(() => process.exit(code), 250);
};
const waitForHealthy = async () => {
  const deadline = Date.now() + healthCheckTimeoutMs;
  const url = `http://127.0.0.1:${port}/global/health`;
  for (;;) {
    if (child.exitCode !== null) {
      console.error(`[electron] omp host exited early with code ${child.exitCode}`);
      if (stderrTail.trim()) console.error(`[electron] omp host stderr (tail):\n${stderrTail.trim()}`);
      process.exit(1);
    }
    if (Date.now() > deadline) {
      console.error('[electron] omp host health check timed out');
      finish(1);
      return;
    }
    try {
      const response = await fetch(url, { signal: AbortSignal.timeout(2000) });
      const body = await response.text();
      if (response.ok && body.includes('"healthy":true')) {
        // /global/health only proves the server booted; capabilities also
        // proves bundled build-time data survived compilation (a runtime
        // readFileSync into the bunfs root 500'd every packaged build
        // while health-only verification stayed green).
        const caps = await fetch(`http://127.0.0.1:${port}/omp/capabilities`, { signal: AbortSignal.timeout(5000) })
          .then((r) => r.json())
          .catch(() => null);
        if (!caps?.features?.['modelRoles.v1']) {
          console.error(`[electron] omp host capabilities check failed: ${JSON.stringify(caps).slice(0, 300)}`);
          finish(1);
          return;
        }
        console.log(`[electron] omp host capabilities: ${Object.keys(caps.features).length} features`);
        // processes.v1 mounts only when the engine found the pi-natives addon
        // resident in the host process; a 501 here means the packaged
        // host lost the session process monitor even though it serves.
        // Natives-free packaged layouts exercise the addon after the
        // first-boot managed-runtime download, not here.
        if (nativesPresent) {
          const processes = await fetch(`http://127.0.0.1:${port}/omp/processes?directory=${encodeURIComponent(os.tmpdir())}`, {
            signal: AbortSignal.timeout(5000),
          }).catch(() => null);
          if (!processes?.ok) {
            console.error(`[electron] omp host processes.v1 check failed: HTTP ${processes?.status ?? 'unreachable'}`);
            finish(1);
            return;
          }
        }
        console.log(`[electron] verified omp host ${mode}: ${hostJs}`);
        finish(0);
        return;
      }
    } catch {
      // Not listening yet; retry.
      await new Promise((resolve) => setTimeout(resolve, 500));
    }
  }
};

await waitForHealthy();
