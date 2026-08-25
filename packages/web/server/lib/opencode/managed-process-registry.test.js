import { existsSync, mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { afterEach, describe, expect, it, vi } from 'vitest';

const spawnSyncMock = vi.fn();

vi.mock('node:child_process', () => ({
  spawnSync: spawnSyncMock,
}));

const { commandIdentifiesOurServer, windowsImageLooksLikeEngine, reapOrphanedProcesses } =
  await import('./managed-process-registry.js');

afterEach(() => {
  spawnSyncMock.mockReset();
  vi.restoreAllMocks();
});

describe('managed process identification', () => {
  // The reaper once matched only `opencode`; the managed engine has been the
  // omp host (`omp-host.exe` / `.../lib/omp-host/host.js serve`) ever since,
  // which made orphan reaping dead code — leaked engines were never killed.
  describe('commandIdentifiesOurServer', () => {
    it('identifies a compiled omp-host serve command line', () => {
      expect(
        commandIdentifiesOurServer(
          'C:\\app\\resources\\omp-host\\omp-host.exe serve --hostname 127.0.0.1 --port 58941',
          { port: 58941 },
        ),
      ).toBe(true);
    });

    it('identifies a from-source host.js launch', () => {
      expect(
        commandIdentifiesOurServer(
          'bun /repo/packages/web/server/lib/omp-host/host.js serve --hostname 127.0.0.1 --port 3902',
          { port: 3902 },
        ),
      ).toBe(true);
    });

    it('still identifies the legacy opencode serve shape', () => {
      expect(commandIdentifiesOurServer('opencode serve --port 4096', { port: 4096 })).toBe(true);
    });

    it('rejects unrelated processes', () => {
      expect(commandIdentifiesOurServer('nginx serve', { port: 4096 })).toBe(false);
      expect(commandIdentifiesOurServer('/usr/bin/some-host --watch', { port: null })).toBe(false);
    });

    it('ties the match to the registered port', () => {
      const command = 'bun /repo/packages/web/server/lib/omp-host/host.js serve --hostname 127.0.0.1 --port 3902';
      expect(commandIdentifiesOurServer(command, { port: 3902 })).toBe(true);
      expect(commandIdentifiesOurServer(command, { port: 4000 })).toBe(false);
    });
  });

  describe('windowsImageLooksLikeEngine', () => {
    it('accepts tasklist CSV rows for our binaries', () => {
      expect(windowsImageLooksLikeEngine('"omp-host.exe","1234","Console","1","84,532 K"')).toBe(true);
      expect(windowsImageLooksLikeEngine('"opencode.exe","1234","Services","0","12,000 K"')).toBe(true);
    });

    it('rejects other images and missing rows', () => {
      expect(windowsImageLooksLikeEngine('"bun.exe","1234","Console","1","20,000 K"')).toBe(false);
      expect(windowsImageLooksLikeEngine('INFO: No tasks are running')).toBe(false);
      expect(windowsImageLooksLikeEngine(null)).toBe(false);
    });
  });
});

describe('reapOrphanedProcesses (win32 branch)', () => {
  const runOnWindows = process.platform === 'win32' ? it : it.skip;

  runOnWindows('reaps a dead-owner omp-host.exe orphan and prunes its entry', async () => {
    const dir = mkdtempSync(path.join(tmpdir(), 'omp-registry-test-'));
    process.env.OMPCHAMBER_MANAGED_PROCESS_REGISTRY = dir;
    const entryFile = path.join(dir, '4242.json');
    writeFileSync(
      entryFile,
      JSON.stringify({
        pid: 4242,
        ownerPid: 777,
        port: 58941,
        binary: 'C:\\app\\resources\\omp-host\\omp-host.exe',
        runtime: 'desktop',
      }),
    );

    try {
      // The orphan (4242) is alive; its owner (777) is long gone.
      vi.spyOn(process, 'kill').mockImplementation((target) => {
        if (target === 4242) return true;
        const error = new Error(`no such process: ${target}`);
        error.code = 'ESRCH';
        throw error;
      });
      spawnSyncMock.mockImplementation((command, args) => {
        if (command === 'tasklist') {
          return { stdout: '"omp-host.exe","4242","Console","1","84,532 K"\r\n' };
        }
        return { stdout: '' };
      });

      const logs = [];
      const result = await reapOrphanedProcesses({ log: (message) => logs.push(message) });

      expect(result).toEqual({ inspected: 1, reaped: 1 });
      const taskkill = spawnSyncMock.mock.calls.find(([command]) => command === 'taskkill');
      expect(taskkill?.[1]).toEqual(['/PID', '4242', '/T', '/F']);
      expect(logs.join('\n')).toContain('reaped orphaned engine pid 4242');
      expect(existsSync(entryFile)).toBe(false);
    } finally {
      delete process.env.OMPCHAMBER_MANAGED_PROCESS_REGISTRY;
      rmSync(dir, { recursive: true, force: true });
    }
  });

  runOnWindows('leaves an unrelated-image orphan untouched', async () => {
    const dir = mkdtempSync(path.join(tmpdir(), 'omp-registry-test-'));
    process.env.OMPCHAMBER_MANAGED_PROCESS_REGISTRY = dir;
    const entryFile = path.join(dir, '4243.json');
    writeFileSync(
      entryFile,
      JSON.stringify({ pid: 4243, ownerPid: 777, port: 58941, binary: null, runtime: 'desktop' }),
    );

    try {
      vi.spyOn(process, 'kill').mockImplementation((target) => {
        if (target === 4243) return true;
        const error = new Error(`no such process: ${target}`);
        error.code = 'ESRCH';
        throw error;
      });
      spawnSyncMock.mockImplementation((command) => {
        if (command === 'tasklist') {
          return { stdout: '"someother.exe","4243","Console","1","10,000 K"\r\n' };
        }
        return { stdout: '' };
      });

      const result = await reapOrphanedProcesses();

      expect(result).toEqual({ inspected: 1, reaped: 0 });
      expect(spawnSyncMock.mock.calls.some(([command]) => command === 'taskkill')).toBe(false);
      // The entry stays: the process is alive but not provably ours.
      expect(existsSync(entryFile)).toBe(true);
    } finally {
      delete process.env.OMPCHAMBER_MANAGED_PROCESS_REGISTRY;
      rmSync(dir, { recursive: true, force: true });
    }
  });
});
