const fs = require('node:fs');
const path = require('node:path');

/**
 * Prune the packaged app's node_modules after copy, before asar packing.
 *
 * The js backend ships the workspace web package in-process; the dependency
 * traversal collects far more than the server runtime needs:
 *   - build/dev subtrees inside @ompchamber/web (dist duplicates the staged
 *     web-dist, server-rs is the Rust source, bin/src/public are web-CLI)
 *   - every platform's optional native binaries (onnxruntime, sherpa-onnx,
 *     pi-natives, lightningcss, sharp) where only the target platform's
 *     copy is loadable
 * The rust backend has no web dependency at all (the variant script removes
 * it before packaging), so only the js variant is pruned.
 */
const pruneJsBackend = (appPath, platform, arch) => {
  const nodeModules = path.join(appPath, 'node_modules');

  // 1. Non-runtime subtrees of the workspace web package.
  const webPackage = path.join(nodeModules, '@ompchamber', 'web');
  for (const entry of ['dist', 'bin', 'src', 'public', 'server-rs', 'scripts', 'tests', 'index.html', 'node_modules']) {
    fs.rmSync(path.join(webPackage, entry), { recursive: true, force: true });
  }

  // 2. Foreign-platform optional binaries: package names ending in a
  //    platform-arch tag that does not match the build target get removed.
  const platformTags = {
    darwin: { arm64: ['darwin-arm64', 'darwin-aarch64'], x64: ['darwin-x64'] },
    win32: { x64: ['win32-x64', 'windows-x64'], arm64: ['win32-arm64', 'windows-arm64'] },
    linux: { x64: ['linux-x64', 'linux-x64-gnu'], arm64: ['linux-arm64', 'linux-arm64-gnu'] },
  };
  const keep = new Set((platformTags[platform] || {})[arch] || []);
  const foreignPlatformTag = (name) => {
    // Collect every platform-arch tag in the name (multi-segment suffixes
    // like linux-arm64-musl match whole); prune when all tags are foreign.
    const tags = name.match(/(darwin|win32|windows|linux|freebsd|android|sunos)-[a-z0-9]+(?:-[a-z0-9]+)*/gi) || [];
    return tags.length > 0 && tags.every((tag) => !keep.has(tag.toLowerCase()));
  };

  const pruneDirectory = (dir) => {
    let entries;
    try {
      entries = fs.readdirSync(dir, { withFileTypes: true });
    } catch {
      return;
    }
    for (const entry of entries) {
      if (!entry.isDirectory()) continue;
      if (foreignPlatformTag(entry.name)) {
        fs.rmSync(path.join(dir, entry.name), { recursive: true, force: true });
      } else if (entry.name.startsWith('@')) {
        pruneDirectory(path.join(dir, entry.name));
      }
    }
  };
  pruneDirectory(nodeModules);
};

module.exports = (context) => {
  const isMac = context.electronPlatformName === 'darwin';
  const resourcesPath = isMac
    ? path.join(context.appOutDir, `${context.packager.appInfo.productFilename}.app`, 'Contents', 'Resources')
    : path.join(context.appOutDir, 'resources');

  if (isMac) {
    const sourceAssetsPath = path.join(__dirname, '..', 'resources', 'icons', 'Assets.car');
    if (!fs.existsSync(sourceAssetsPath)) {
      throw new Error(`Missing compiled app icon asset catalog at ${sourceAssetsPath}`);
    }
    fs.copyFileSync(sourceAssetsPath, path.join(resourcesPath, 'Assets.car'));
  }

  // Prune only the js variant (the marker file ships inside the app).
  try {
    const appPath = path.join(resourcesPath, 'app');
    const backend = fs.readFileSync(path.join(appPath, 'server-backend'), 'utf8').trim();
    if (backend === 'js') {
      pruneJsBackend(
        appPath,
        context.electronPlatformName,
        context.packager.appInfo.architecture || process.arch,
      );
    }
  } catch {
    // No marker (dev builds) — nothing to prune.
  }
};
