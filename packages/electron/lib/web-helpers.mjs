// Verbatim local copies of the three helpers the main process used to import
// from @ompchamber/web. That dependency pulled the whole workspace web package
// (including server-rs with its cargo target tree) into electron-builder's
// dependency traversal, ballooning app.asar by ~850 MB — hence vendoring and
// dropping the dependency.
//
// NOTE mintOutsideFileGrant: grants are minted in the Electron process, but
// file-access enforcement now lives in the spawned Rust server, whose token
// store cannot see these. Same behavior as since the sidecar switch
// (pre-existing gap; a control-channel op is the follow-up).
//
// Upstream sources (keep in sync):
//   packages/web/server/lib/fs/routes.js          (mintOutsideFileGrant)
//   packages/web/server/lib/opencode/path-utils.js
//   packages/web/server/lib/inherited-env.js      (clearAppImageArgv0FromProcessEnv)

import { createRequire } from 'node:module';
import nodeFsPromises from 'node:fs/promises';
import nodePath from 'node:path';

// --- fs/routes.js: mintOutsideFileGrant (verbatim) ---

const OUTSIDE_FILE_GRANT_TTL_MS = 10 * 60 * 1000;

const outsideFileGrants = new Map();

const pruneOutsideFileGrants = () => {
  const now = Date.now();
  for (const [token, grant] of outsideFileGrants.entries()) {
    if (!grant || grant.expiresAt <= now) {
      outsideFileGrants.delete(token);
    }
  }
};

export const mintOutsideFileGrant = async (targetPath, {
  scopes = ['stat', 'read', 'raw'],
  fsPromises = nodeFsPromises,
  path = nodePath,
  crypto = globalThis.crypto,
} = {}) => {
  const raw = typeof targetPath === 'string' ? targetPath.trim() : '';
  if (!raw) {
    throw new Error('Path is required');
  }
  const canonicalPath = await fsPromises.realpath(raw);
  const stats = await fsPromises.stat(canonicalPath);
  if (!stats.isFile()) {
    throw new Error('Outside file grants require a file path');
  }
  pruneOutsideFileGrants();
  const token = typeof crypto?.randomUUID === 'function'
    ? crypto.randomUUID()
    : `${Date.now()}-${Math.random().toString(36).slice(2)}`;
  const normalizedScopes = new Set(
    (Array.isArray(scopes) ? scopes : [])
      .filter((scope) => typeof scope === 'string' && scope.trim())
      .map((scope) => scope.trim())
  );
  if (normalizedScopes.size === 0) {
    normalizedScopes.add('read');
  }
  const grant = {
    canonicalPath,
    base: path.dirname(canonicalPath),
    scopes: normalizedScopes,
    expiresAt: Date.now() + OUTSIDE_FILE_GRANT_TTL_MS,
  };
  outsideFileGrants.set(token, grant);
  return {
    path: canonicalPath,
    outsideFileGrant: token,
    expiresAt: grant.expiresAt,
  };
};

// --- opencode/path-utils.js (verbatim) ---

const TOOLCHAIN_SEGMENTS = [
  '/opt/homebrew/',
  '/opt/pkg/',
  '/opt/pmk/',
  '/snap/',
];

const TOOLCHAIN_BASENAMES = new Set([
  '.cargo',
  '.bun',
  '.nvm',
  '.pyenv',
  '.rbenv',
  '.sdkman',
  '.asdf',
  '.volta',
  '.fnm',
  '.local',
  '.opencode',
  'node_modules',
]);

export function pathLooksUserConfigured(value, home, delim) {
  if (typeof value !== 'string' || !value) {
    return false;
  }

  const normalizedHome = typeof home === 'string' ? home.replaceAll('\\', '/') : '';
  const homeWithSep = normalizedHome ? normalizedHome + '/' : '';

  return value.split(delim).some((segment) => {
    if (!segment) return false;
    const normalizedSegment = segment.replaceAll('\\', '/');

    // Any path under the user's home directory.
    if (normalizedHome && (normalizedSegment === normalizedHome || normalizedSegment.startsWith(homeWithSep))) {
      return true;
    }

    // Well-known package-manager / toolchain prefixes.
    if (TOOLCHAIN_SEGMENTS.some((prefix) => normalizedSegment.startsWith(prefix))) {
      return true;
    }

    // Well-known dot-directories inside home (e.g. ~/.cargo/bin).
    const parts = normalizedSegment.split('/').filter(Boolean);
    if (parts.some((part) => TOOLCHAIN_BASENAMES.has(part))) {
      return true;
    }

    return false;
  });
}

export function mergePathValues(primary, fallback, delim) {
  const seen = new Set();
  const result = [];

  const addSegments = (value) => {
    if (typeof value !== 'string' || !value) return;
    for (const segment of value.split(delim)) {
      if (segment && !seen.has(segment)) {
        seen.add(segment);
        result.push(segment);
      }
    }
  };

  addSegments(primary);
  addSegments(fallback);

  return result.join(delim);
}

// --- inherited-env.js: clearAppImageArgv0FromProcessEnv (verbatim) ---

export function clearAppImageArgv0FromProcessEnv() {
  delete process.env.ARGV0;
  if (process.platform !== 'linux' || typeof Bun === 'undefined') return;
  try {
    const require = createRequire(import.meta.url);
    const { dlopen } = require('bun:ffi');
    const libc = dlopen('libc.so.6', {
      unsetenv: { args: ['cstring'], returns: 'i32' },
    });
    libc.symbols.unsetenv(Buffer.from('ARGV0\0'));
  } catch {
    // Node/Electron and environments without bun:ffi rely on explicit child envs.
  }
}
