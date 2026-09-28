import { execFileSync } from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { afterEach, describe, expect, it } from 'vitest';

import {
  createWorktree,
  ensureWorktreeLongpaths,
  getWorktreeBootstrapStatus,
  populateWorktreeWithLockRecovery,
} from './service.js';

/**
 * issue #2746（"[Bug] new worktree, Filename too long"）回归测试套件。
 *
 * 验证新建 worktree 的 bootstrap 流程会在填充（git reset --hard）前启用
 * core.longpaths，从而在 Windows MAX_PATH 限制下也能检出深层嵌套文件；
 * 同时模拟单段文件名超过 NAME_MAX 的不可恢复场景，断言 bootstrap 以
 * failed 状态结束并给出含 path-length limit 的引导性错误。依赖真实 git
 * 可执行文件，环境缺失时用例静默跳过。
 */
// ---------------------------------------------------------------------------
// Regression for https://github.com/openchamber/openchamber/issues/2746
//
// "[Bug] new worktree， Filename too long"
//
// OMPChamber places worktrees under:
//   <XDG_DATA_HOME>/opencode/worktree/<40-char root commit hash>/<worktree name>
// and populates them with `git reset --hard`. On Windows, that deep prefix plus
// a deeply nested repo file (e.g. yudao ~173 chars) exceeds MAX_PATH (260) and
// git aborts with "Filename too long" unless `core.longpaths` is enabled.
// ---------------------------------------------------------------------------

/** 本套件各用例创建的临时目录（含临时 XDG_DATA_HOME），afterEach 统一递归清理。 */
const tempDirs = [];

/** 创建带 ompchamber-git-issue2746- 前缀的临时目录并登记到 tempDirs 以便统一清理。 */
const createTempDir = () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'ompchamber-git-issue2746-'));
  tempDirs.push(dir);
  return dir;
};

/** 在 cwd 同步执行 git 子命令并返回 utf8 stdout；可选通过 stdin 传入 input。 */
const runGit = (cwd, args, input) =>
  execFileSync('git', args, {
    cwd,
    encoding: 'utf8',
    input,
    stdio: ['pipe', 'pipe', 'pipe'],
  });

/** 探测环境是否可用 git（git --version）；不可用时用例直接 return 跳过。 */
const canRunGit = () => {
  try {
    execFileSync('git', ['--version'], { stdio: 'ignore' });
    return true;
  } catch {
    return false;
  }
};

// 递归删除本用例登记的所有临时目录。
afterEach(() => {
  for (const dir of tempDirs.splice(0)) {
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

/** 长路径支持：新建 worktree 自动启用 core.longpaths、设置的幂等性，以及不可恢复失败时的引导性错误。 */
describe('issue #2746 - worktree long path support', () => {
  it('enables core.longpaths and populates a deeply nested worktree checkout', async () => {
    if (!canRunGit()) return;

    const previousXdgDataHome = process.env.XDG_DATA_HOME;
    const dataHome = createTempDir();
    process.env.XDG_DATA_HOME = dataHome;

    try {
      const repo = createTempDir();
      runGit(repo, ['init', '-b', 'main']);
      runGit(repo, ['config', 'user.email', 'test@example.com']);
      runGit(repo, ['config', 'user.name', 'Test User']);

      // Realistic reporter path: many nested segments, each component well under
      // NAME_MAX. On Windows the managed worktree prefix + this relative path
      // exceeds MAX_PATH unless core.longpaths is enabled.
      const deepRelative = path.join(
        'server',
        'yudao-framework',
        'yudao-spring-boot-starter-biz-data-permission',
        'src',
        'main',
        'java',
        'cn',
        'iocoder',
        'yudao',
        'framework',
        'datapermission',
        'config',
        'YudaoDataPermissionAutoConfiguration.java',
      );
      fs.mkdirSync(path.dirname(path.join(repo, deepRelative)), { recursive: true });
      fs.writeFileSync(path.join(repo, deepRelative), '// yudao\n');
      fs.writeFileSync(path.join(repo, 'README.md'), '# Test\n');
      runGit(repo, ['add', 'README.md', deepRelative]);
      runGit(repo, ['commit', '-qm', 'init']);

      const created = await createWorktree(repo, {
        mode: 'new',
        worktreeName: 'issue-2746',
        branchName: 'ompchamber/issue-2746',
      });
      expect(created.directoryCreated).toBe(true);

      await expect.poll(async () => {
        const status = await getWorktreeBootstrapStatus(created.path);
        return status?.status;
      }, { timeout: 10_000 }).toBe('ready');

      const longpaths = runGit(created.path, ['config', '--get', 'core.longpaths']).trim();
      expect(longpaths).toBe('true');
      expect(fs.existsSync(path.join(created.path, deepRelative))).toBe(true);
      expect(fs.existsSync(path.join(created.path, 'README.md'))).toBe(true);
    } finally {
      if (previousXdgDataHome === undefined) {
        delete process.env.XDG_DATA_HOME;
      } else {
        process.env.XDG_DATA_HOME = previousXdgDataHome;
      }
    }
  });

  it('ensureWorktreeLongpaths is idempotent when already enabled', async () => {
    if (!canRunGit()) return;

    const repo = createTempDir();
    runGit(repo, ['init', '-b', 'main']);
    runGit(repo, ['config', 'user.email', 'test@example.com']);
    runGit(repo, ['config', 'user.name', 'Test User']);
    fs.writeFileSync(path.join(repo, 'README.md'), '# Test\n');
    runGit(repo, ['add', 'README.md']);
    runGit(repo, ['commit', '-qm', 'init']);
    runGit(repo, ['config', 'core.longpaths', 'true']);

    await expect(ensureWorktreeLongpaths(repo)).resolves.toBeUndefined();
    expect(runGit(repo, ['config', '--get', 'core.longpaths']).trim()).toBe('true');
  });

  it('surfaces guided bootstrap failure when a path component exceeds the filesystem name limit', async () => {
    if (!canRunGit()) return;

    const previousXdgDataHome = process.env.XDG_DATA_HOME;
    const dataHome = createTempDir();
    process.env.XDG_DATA_HOME = dataHome;

    try {
      const repo = createTempDir();
      runGit(repo, ['init', '-b', 'main']);
      runGit(repo, ['config', 'user.email', 'test@example.com']);
      runGit(repo, ['config', 'user.name', 'Test User']);

      // Linux/macOS NAME_MAX equivalent of the Windows failure mode: a single
      // path component longer than 255 cannot be materialized. core.longpaths
      // cannot fix this; bootstrap must fail clearly instead of leaving a
      // silent half-populated worktree.
      const longComponent = 'x'.repeat(300);
      const longPath = `server/${longComponent}/YudaoDataPermissionAutoConfiguration.java`;
      const blobHash = runGit(repo, ['hash-object', '-w', '--stdin'], '// test\n').trim();
      runGit(repo, ['update-index', '--add', '--cacheinfo', `100644,${blobHash},${longPath}`]);
      runGit(repo, ['commit', '-qm', 'init']);
      fs.writeFileSync(path.join(repo, 'README.md'), '# Test\n');
      runGit(repo, ['add', 'README.md']);
      runGit(repo, ['commit', '-qm', 'add readme']);

      const created = await createWorktree(repo, {
        mode: 'new',
        worktreeName: 'issue-2746-namemax',
        branchName: 'ompchamber/issue-2746-namemax',
      });
      expect(created.directoryCreated).toBe(true);

      await expect.poll(async () => {
        const status = await getWorktreeBootstrapStatus(created.path);
        return status?.status;
      }, { timeout: 10_000 }).toBe('failed');

      const status = await getWorktreeBootstrapStatus(created.path);
      expect(status?.error).toMatch(/file name too long|filename too long/i);
      expect(status?.error).toMatch(/path-length limit/i);
      expect(runGit(created.path, ['config', '--get', 'core.longpaths']).trim()).toBe('true');
    } finally {
      if (previousXdgDataHome === undefined) {
        delete process.env.XDG_DATA_HOME;
      } else {
        process.env.XDG_DATA_HOME = previousXdgDataHome;
      }
    }
  });

  it('populateWorktreeWithLockRecovery enables longpaths before reset', async () => {
    if (!canRunGit()) return;

    const repo = createTempDir();
    runGit(repo, ['init', '-b', 'main']);
    runGit(repo, ['config', 'user.email', 'test@example.com']);
    runGit(repo, ['config', 'user.name', 'Test User']);
    fs.writeFileSync(path.join(repo, 'README.md'), '# Test\n');
    runGit(repo, ['add', 'README.md']);
    runGit(repo, ['commit', '-qm', 'init']);

    const worktree = createTempDir();
    fs.rmSync(worktree, { recursive: true, force: true });
    runGit(repo, ['worktree', 'add', '--no-checkout', '-b', 'feature/longpaths-populate', worktree, 'HEAD']);

    await expect(populateWorktreeWithLockRecovery(worktree)).resolves.toBeUndefined();
    expect(runGit(worktree, ['config', '--get', 'core.longpaths']).trim()).toBe('true');
    expect(fs.readFileSync(path.join(worktree, 'README.md'), 'utf8')).toBe('# Test\n');
  });
});
