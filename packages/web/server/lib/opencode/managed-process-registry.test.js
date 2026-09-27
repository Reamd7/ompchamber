/**
 * managed-process-registry 测试套件：验证进程识别启发式——
 * commandIdentifiesOurServer 匹配 omp-host（编译版与源码启动）及遗留
 * opencode 命令行并绑定注册端口，windowsImageLooksLikeEngine 识别
 * tasklist CSV 行；以及 win32 平台下 reapOrphanedProcesses 对孤儿引擎
 * 的收割与对无关进程的保守跳过。
 */
import { existsSync, mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { afterEach, describe, expect, it, vi } from 'vitest';

/** 被测导出：进程识别启发式与注册表工厂（动态导入以配合测试加载顺序）。 */
const { commandIdentifiesOurServer, windowsImageLooksLikeEngine, createManagedProcessRegistry } =
  await import('./managed-process-registry.js');

// 每个用例后还原 vi.spyOn 打的桩（如 process.kill）。
afterEach(() => {
  vi.restoreAllMocks();
});

// 进程识别主套件。
describe('managed process identification', () => {
  // 中文补充：reaper 曾只匹配 opencode；此后托管引擎一直是 omp host
  // （omp-host.exe 或 .../lib/omp-host/host.ts serve），导致孤儿收割长期
  // 失效——泄漏的引擎从未被杀掉。
  // The reaper once matched only `opencode`; the managed engine has been the
  // omp host (`omp-host.exe` / `.../lib/omp-host/host.ts serve`) ever since,
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

    it('identifies a from-source host.ts launch', () => {
      expect(
        commandIdentifiesOurServer(
          'bun /repo/packages/web/server/lib/omp-host/host.ts serve --hostname 127.0.0.1 --port 3902',
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
      const command = 'bun /repo/packages/web/server/lib/omp-host/host.ts serve --hostname 127.0.0.1 --port 3902';
      expect(commandIdentifiesOurServer(command, { port: 3902 })).toBe(true);
      expect(commandIdentifiesOurServer(command, { port: 4000 })).toBe(false);
    });
  });

  // windowsImageLooksLikeEngine：tasklist CSV 行的镜像识别。
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

// reapOrphanedProcesses 的 win32 分支（其它平台整体跳过）。
describe('reapOrphanedProcesses (win32 branch)', () => {
  // 仅在 win32 运行、其它平台降级为 it.skip 的用例包装器。
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
      const execFileCalls = [];
      const registry = createManagedProcessRegistry({
        execFileAsync: async (command, args) => {
          execFileCalls.push([command, args]);
          if (command === 'tasklist') {
            return { stdout: '"omp-host.exe","4242","Console","1","84,532 K"\r\n' };
          }
          return { stdout: '' };
        },
      });

      const logs = [];
      const result = await registry.reapOrphanedProcesses({ log: (message) => logs.push(message) });

      expect(result).toEqual({ inspected: 1, reaped: 1 });
      const taskkill = execFileCalls.find(([command]) => command === 'taskkill');
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
      JSON.stringify({
        pid: 4243,
        ownerPid: 777,
        port: 58941,
        binary: null,
        runtime: 'desktop',
      }),
    );

    try {
      vi.spyOn(process, 'kill').mockImplementation((target) => {
        if (target === 4243) return true;
        const error = new Error(`no such process: ${target}`);
        error.code = 'ESRCH';
        throw error;
      });
      const execFileCalls = [];
      const registry = createManagedProcessRegistry({
        execFileAsync: async (command) => {
          execFileCalls.push(command);
          if (command === 'tasklist') {
            return { stdout: '"someother.exe","4243","Console","1","10,000 K"\r\n' };
          }
          return { stdout: '' };
        },
      });

      const result = await registry.reapOrphanedProcesses();

      expect(result).toEqual({ inspected: 1, reaped: 0 });
      expect(execFileCalls.some((command) => command === 'taskkill')).toBe(false);
      // The entry stays: the process is alive but not provably ours.
      expect(existsSync(entryFile)).toBe(true);
    } finally {
      delete process.env.OMPCHAMBER_MANAGED_PROCESS_REGISTRY;
      rmSync(dir, { recursive: true, force: true });
    }
  });
});
