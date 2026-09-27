/**
 * scheduled-tasks 运行时测试套件：覆盖 computeNextRunAt 等纯函数的
 * 日程计算行为，以及 createScheduledTasksRuntime.syncProject 对 loop
 * markdown 文件的对账接线（发现、持久化、路径不可解析时的降级）。
 */
import { describe, expect, it, vi } from 'vitest';
import os from 'os';
import path from 'path';
import { mkdtemp, rm, mkdir, writeFile } from 'fs/promises';
import {
  computeNextRunAt,
  expandCommandGoalObjective,
  formatScheduledSessionTitle,
  parseScheduledCommandPrompt,
  createScheduledTasksRuntime,
} from './runtime.js';
import { createProjectConfigRuntime } from '../projects/project-config.js';

/**
 * 纯 helper 行为验证：时区感知的下一次运行时间计算（daily / weekly /
 * once 及多时刻取最近）、定时会话标题格式化、斜杠命令 prompt 解析，
 * 以及命令参数到 goal objective 的展开（$ARGUMENTS / 位置参数 / 无占位符回退）。
 */
describe('scheduled-tasks runtime helpers', () => {
  it('computes next daily run in timezone', () => {
    const nowUtc = Date.UTC(2025, 0, 1, 8, 0, 0);
    const next = computeNextRunAt({
      enabled: true,
      schedule: {
        kind: 'daily',
        times: ['09:30'],
        timezone: 'UTC',
      },
    }, nowUtc);

    expect(next).toBe(Date.UTC(2025, 0, 1, 9, 30, 0));
  });

  it('computes weekly next run using weekdays', () => {
    // Monday 2025-01-06 10:00:00 UTC
    const nowUtc = Date.UTC(2025, 0, 6, 10, 0, 0);
    const next = computeNextRunAt({
      enabled: true,
      schedule: {
        kind: 'weekly',
        times: ['09:00'],
        weekdays: [1, 3],
        timezone: 'UTC',
      },
    }, nowUtc);

    // Wednesday 2025-01-08 09:00:00 UTC
    expect(next).toBe(Date.UTC(2025, 0, 8, 9, 0, 0));
  });

  it('picks nearest time from multiple daily times', () => {
    const nowUtc = Date.UTC(2025, 0, 1, 9, 20, 0);
    const next = computeNextRunAt({
      enabled: true,
      schedule: {
        kind: 'daily',
        times: ['09:15', '09:45', '18:00'],
        timezone: 'UTC',
      },
    }, nowUtc);

    expect(next).toBe(Date.UTC(2025, 0, 1, 9, 45, 0));
  });

  it('computes one-time next run for future date', () => {
    const nowUtc = Date.UTC(2026, 3, 15, 10, 0, 0);
    const next = computeNextRunAt({
      enabled: true,
      schedule: {
        kind: 'once',
        date: '2026-04-16',
        time: '13:30',
        timezone: 'UTC',
      },
    }, nowUtc);

    expect(next).toBe(Date.UTC(2026, 3, 16, 13, 30, 0));
  });

  it('returns null for past one-time schedule', () => {
    const nowUtc = Date.UTC(2026, 3, 16, 14, 0, 0);
    const next = computeNextRunAt({
      enabled: true,
      schedule: {
        kind: 'once',
        date: '2026-04-16',
        time: '13:30',
        timezone: 'UTC',
      },
    }, nowUtc);

    expect(next).toBeNull();
  });

  it('formats session title with timestamp suffix', () => {
    const title = formatScheduledSessionTitle({
      name: 'Morning Sync',
      schedule: { timezone: 'UTC' },
    }, Date.UTC(2025, 2, 10, 7, 5, 0));

    expect(title).toBe('Morning Sync 2025-03-10 07:05');
  });

  it('parses slash command prompt for scheduled command mode', () => {
    expect(parseScheduledCommandPrompt('/review src/components')).toEqual({
      command: 'review',
      arguments: 'src/components',
    });
  });

  it('returns null when prompt is not a slash command', () => {
    expect(parseScheduledCommandPrompt('Summarize open issues')).toBeNull();
    expect(parseScheduledCommandPrompt('/')).toBeNull();
  });

  it('expands command arguments into the goal objective', () => {
    expect(expandCommandGoalObjective(
      'Run the issue pipeline for $ARGUMENTS. Verify $ARGUMENTS is represented by the PR.',
      'LIN-123 --draft',
    )).toBe('Run the issue pipeline for LIN-123 --draft. Verify LIN-123 --draft is represented by the PR.');
    expect(expandCommandGoalObjective(undefined, 'LIN-123')).toBeNull();
    expect(expandCommandGoalObjective('Move $1 to $2', '"src old" dist extra')).toBe('Move src old to dist extra');
    expect(expandCommandGoalObjective('Review the requested scope.', 'auth module'))
      .toBe('Review the requested scope.\n\nauth module');
  });
});

/**
 * syncProject 接线验证：项目路径可解析时把发现的 loop markdown 对账并
 * 持久化为任务（含 nextRunAt）；路径不可解析（项目未注册）时退化为
 * 普通列表查询，绝不触发对账。
 */
describe('scheduled-tasks runtime syncProject wiring', () => {
  /** 创建带 .agents/loops 目录的临时项目仓库，返回临时根、仓库路径与 cleanup 清理函数。 */
  const createTempProject = async () => {
    const tempRoot = await mkdtemp(path.join(os.tmpdir(), 'oc-runtime-loop-'));
    const repoPath = path.join(tempRoot, 'repo');
    await mkdir(path.join(repoPath, '.agents', 'loops'), { recursive: true });
    return {
      tempRoot,
      repoPath,
      cleanup: async () => {
        await rm(tempRoot, { recursive: true, force: true });
      },
    };
  };

  /** 基于临时目录构造项目配置运行时（固定 taskID 生成器，与真实数据目录隔离）。 */
  const createProjectConfig = async (tempRoot) => createProjectConfigRuntime({
    fsPromises: await import('fs/promises'),
    path,
    projectsDirPath: path.join(tempRoot, 'config'),
    createTaskID: () => 'task-fixed-id',
  });

  /** 构造定时任务运行时的最小依赖集；overrides 可替换任意项。 */
  const createRuntimeDeps = (overrides = {}) => ({
    buildOpenCodeUrl: () => 'http://localhost',
    getOpenCodeAuthHeaders: () => ({}),
    waitForOpenCodeReady: async () => {},
    ...overrides,
  });

  it('reconciles discovered loops when the project path is known', async () => {
    const { tempRoot, repoPath, cleanup } = await createTempProject();
    try {
      await writeFile(path.join(repoPath, '.agents', 'loops', 'daily.md'), `---
name: daily
schedule: "0 9 * * *"
enabled: true
model: openai/gpt-5
---
Run daily.
`, 'utf8');

      const projectConfigRuntime = await createProjectConfig(tempRoot);
      const runtime = createScheduledTasksRuntime({
        ...createRuntimeDeps(),
        projectConfigRuntime,
        listProjects: async () => [{ id: 'proj', path: repoPath }],
      });

      await runtime.syncProject('proj');

      const tasks = await projectConfigRuntime.listScheduledTasks('proj');
      expect(tasks).toHaveLength(1);
      expect(tasks[0].id).toBe('loop:project:daily');
      expect(tasks[0].loopFile).toBe(path.join(repoPath, '.agents', 'loops', 'daily.md'));
      // syncTaskSchedule computed and persisted the next run for the enabled task.
      expect(tasks[0].state.nextRunAt).toBeGreaterThan(0);
    } finally {
      await cleanup();
    }
  });

  it('falls back to plain listing when the project path cannot be resolved', async () => {
    const { tempRoot, cleanup } = await createTempProject();
    try {
      const projectConfigRuntime = await createProjectConfig(tempRoot);
      const reconcileSpy = vi.spyOn(projectConfigRuntime, 'reconcileLoopTasks');
      const listSpy = vi.spyOn(projectConfigRuntime, 'listScheduledTasks');

      const runtime = createScheduledTasksRuntime({
        ...createRuntimeDeps(),
        projectConfigRuntime,
        // Project not registered -> ensureProjectPath cannot resolve a path.
        listProjects: async () => [],
      });

      await runtime.syncProject('proj');

      expect(reconcileSpy).not.toHaveBeenCalled();
      expect(listSpy).toHaveBeenCalledWith('proj');
      expect(await projectConfigRuntime.listScheduledTasks('proj')).toEqual([]);
      reconcileSpy.mockRestore();
      listSpy.mockRestore();
    } finally {
      await cleanup();
    }
  });
});
