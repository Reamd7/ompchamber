// Switch the Electron package between server-backend build variants.
//
//   node ./scripts/set-server-backend.mjs rust   (default variant)
//   node ./scripts/set-server-backend.mjs js     (legacy in-process parity build)
//
// What it does:
//   1. writes packages/electron/server-backend (shipped in the app; the main
//      process reads it at boot to pick the backend — see main.mjs)
//   2. for `rust`: REMOVES the `@ompchamber/web` dependency — with it present,
//      electron-builder's dependency traversal ships the whole workspace web
//      package (and its transitive deps) in app.asar (~850 MB with a cargo
//      target tree). The Rust build's main bundle never imports it at runtime
//      (the dynamic import is external + backend-gated).
//   3. for `js`: ensures the dependency is present (the in-process server)
//
// Run `bun install` after switching when the dependency set changed.

import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const electronRoot = path.resolve(__dirname, '..');
const backend = process.argv[2];

if (backend !== 'rust' && backend !== 'js') {
  console.error('usage: set-server-backend.mjs <rust|js>');
  process.exit(1);
}

fs.writeFileSync(path.join(electronRoot, 'server-backend'), `${backend}\n`);

const pkgPath = path.join(electronRoot, 'package.json');
const pkg = JSON.parse(fs.readFileSync(pkgPath, 'utf8'));
pkg.dependencies ??= {};

if (backend === 'js') {
  if (pkg.dependencies['@ompchamber/web'] !== 'workspace:*') {
    pkg.dependencies['@ompchamber/web'] = 'workspace:*';
    fs.writeFileSync(pkgPath, `${JSON.stringify(pkg, null, 2)}\n`);
    console.log('[backend] js: added @ompchamber/web dependency — run bun install');
  } else {
    console.log('[backend] js: dependency already present');
  }
} else if (pkg.dependencies['@ompchamber/web']) {
  delete pkg.dependencies['@ompchamber/web'];
  fs.writeFileSync(pkgPath, `${JSON.stringify(pkg, null, 2)}\n`);
  console.log('[backend] rust: removed @ompchamber/web dependency — run bun install');
} else {
  console.log('[backend] rust: dependency already absent');
}
console.log(`[backend] marker: ${backend}`);
