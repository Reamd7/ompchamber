/**
 * process-platform 适配器的测试套件。
 *
 * 适配器必须复用 SDK 在本进程内已加载的 pi-natives addon 直接建立，
 * 并通过它读取真实的进程后代树。这是 1.32.0 里回归过的接缝：经由
 * require.resolve 解析 addon 曾让打包后的桌面宿主在开始服务前就挂起。
 * 本套件 spawn 一个真实子进程，验证 enumerateTree/isAlive/terminate。
 */
// process-platform tests — the adapter must come up from the pi-natives addon
// the SDK already holds in this process, and read the real descendant tree
// through it. This is the seam that regressed in 1.32.0: resolving the addon
// through `require.resolve` hung packaged desktop hosts before they served.

import { afterAll, beforeAll, describe, expect, test } from 'bun:test';
import { spawn, type ChildProcess } from 'node:child_process';
import { VERSION } from '@oh-my-pi/pi-coding-agent';
import { createProcessPlatform } from './process-platform.ts';

void VERSION; // importing the SDK loads @oh-my-pi/pi-natives as a side effect

/**
 * 轮询等待谓词成立；timeoutMs（默认 3000ms）内每 50ms 轮询一次，
 * 超时返回 false 而不抛错。
 */
const waitFor = async (predicate: () => Promise<boolean>, timeoutMs = 3000): Promise<boolean> => {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (await predicate()) return true;
    await new Promise((resolve) => setTimeout(resolve, 50));
  }
  return false;
};

// createProcessPlatform 契约：复用进程内常驻的 pi-natives addon（而非
// 重新解析包），读取真实后代树，并对 argv 匹配的进程判定存活。
describe('createProcessPlatform', () => {
  let child: ChildProcess;

  beforeAll(() => {
    child = spawn(process.execPath, ['-e', 'setTimeout(() => {}, 30000)'], { stdio: 'ignore' });
  });

  afterAll(() => {
    child.kill('SIGKILL');
  });

  test('reuses the resident pi-natives addon instead of resolving the package', async () => {
    const platform = createProcessPlatform();
    expect(platform).not.toBeNull();
    const seen = await waitFor(async () => (await platform!.enumerateTree()).some((entry) => entry.pid === child.pid));
    expect(seen).toBe(true);
    const row = (await platform!.enumerateTree()).find((entry) => entry.pid === child.pid);
    expect(row?.ppid).toBe(process.pid);
    expect(platform!.isAlive(child.pid!, row!.argv)).toBe(true);
    expect(platform!.isAlive(child.pid!, 'some-other-program')).toBe(false);
  });

  test('terminate ends the tracked process', async () => {
    const platform = createProcessPlatform()!;
    expect(await platform.terminate(child.pid!)).toBe(true);
    const gone = await waitFor(async () => !(await platform.enumerateTree()).some((entry) => entry.pid === child.pid));
    expect(gone).toBe(true);
  });
});
