// process-platform tests — the adapter must come up from the pi-natives addon
// the SDK already holds in this process, and read the real descendant tree
// through it. This is the seam that regressed in 1.32.0: resolving the addon
// through `require.resolve` hung packaged desktop hosts before they served.

import { afterAll, beforeAll, describe, expect, test } from 'bun:test';
import { spawn, type ChildProcess } from 'node:child_process';
import { VERSION } from '@oh-my-pi/pi-coding-agent';
import { createProcessPlatform } from './process-platform.ts';

void VERSION; // importing the SDK loads @oh-my-pi/pi-natives as a side effect

const waitFor = async (predicate: () => Promise<boolean>, timeoutMs = 3000): Promise<boolean> => {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (await predicate()) return true;
    await new Promise((resolve) => setTimeout(resolve, 50));
  }
  return false;
};

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
