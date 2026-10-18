// Managed Bun runtime discovery for server-side launch resolution.
//
// The desktop app installs official Bun runtimes under
// `<userData>/runtimes/bun-<version>/` (packages/electron/bun-runtime.mjs)
// and exports the directory as OMPCHAMBER_MANAGED_BUN_DIR so every child
// (web server, rust server, omp host engine) can resolve it. Without a
// desktop shell the documented manual-install location is
// `~/.ompchamber/runtimes/`.
//
// This layer is presence-only: a managed binary is trusted because the
// installer verified `bun --version` against the pinned manifest before
// publishing it atomically. Version enforcement lives at install/repair time
// (Electron) — per-spawn `--version` probes here would tax every engine boot.

import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';

/** Binary file name inside a managed runtime directory (Windows 带 .exe)。 */
const bunBinaryName = () => (process.platform === 'win32' ? 'bun.exe' : 'bun');

/** 候选根目录：桌面注入的 OMPCHAMBER_MANAGED_BUN_DIR → 手动安装默认位。 */
const managedBunRoots = () => [
  process.env.OMPCHAMBER_MANAGED_BUN_DIR,
  path.join(os.homedir(), '.ompchamber', 'runtimes'),
].filter(Boolean);

/** 版本号比较：-1 | 0 | 1（目录名里的点分数字）。 */
const compareVersions = (a, b) => {
  const pa = a.split('.').map(Number);
  const pb = b.split('.').map(Number);
  for (let i = 0; i < Math.max(pa.length, pb.length); i++) {
    const delta = (pa[i] || 0) - (pb[i] || 0);
    if (delta) return delta > 0 ? 1 : -1;
  }
  return 0;
};

/**
 * 在托管根目录里找最新可用的 bun 二进制。
 * @returns {{ binary: string, version: string, source: 'managed' } | null}
 */
export const resolveManagedBunRuntimeBinary = () => {
  const found = [];
  for (const root of managedBunRoots()) {
    let entries;
    try {
      entries = fs.readdirSync(root);
    } catch {
      continue;
    }
    for (const entry of entries) {
      const match = /^bun-(\d+(?:\.\d+)*)$/.exec(entry);
      if (!match) continue;
      const binary = path.join(root, entry, bunBinaryName());
      if (fs.existsSync(binary)) found.push({ binary, version: match[1], source: 'managed' });
    }
  }
  found.sort((a, b) => compareVersions(b.version, a.version));
  return found[0] ?? null;
};
