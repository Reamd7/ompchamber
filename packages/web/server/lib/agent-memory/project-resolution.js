/**
 * Which project's memory a session directory belongs to.
 *
 * A session often runs in a worktree, whose path is not the project's path.
 * Keying memory by the session directory filed a worktree's memories under a
 * project the panel never reads, so the agent stored them and the user never
 * saw them. Every worktree of a repository shares one project memory, which is
 * also what the user means by "this project".
 *
 * A directory that is itself a configured project is taken as-is; anything else
 * resolves to its primary worktree. The configured check comes first because a
 * user may register a worktree as a project in its own right, and that choice
 * has to win over the git topology.
 */
/**
 * 会话目录到记忆所属项目的解析规则（中文说明）。
 *
 * 会话经常运行在 worktree 里，其路径并不是项目路径。若按会话目录归档
 * 记忆，worktree 写下的记忆会落进面板永远不读的项目——agent 存了，用
 * 户却看不见。同一仓库的所有 worktree 共享一份项目记忆，这也正是用户
 * 所说的"这个项目"。
 *
 * 本身已被配置为项目的目录按原样采用；其余目录一律解析到其主 worktree
 * 根。配置检查放在最前，因为用户可能把某个 worktree 单独注册为项目，
 * 这个选择必须压过 git 拓扑。
 */

import path from 'node:path';

import { createProjectIdFromPath } from '../projects/project-id.js';

/** 把输入规整为绝对路径字符串；非字符串或空白输入返回空串。 */
const normalize = (value) => {
  if (typeof value !== 'string') return '';
  const trimmed = value.trim();
  return trimmed ? path.resolve(trimmed) : '';
};

/**
 * 创建项目解析器。依赖：listProjectPaths（异步列出已配置的项目路径）、
 * resolvePrimaryWorktreeRoot（异步解析 git 主 worktree 根）、
 * managedProjectRoots（本服务托管的项目根目录列表，可省略）。
 * 返回的解析函数把任意会话目录映射为稳定的项目 ID（见 project-id.js）。
 */
export const createMemoryProjectResolver = (dependencies) => {
  const { listProjectPaths, resolvePrimaryWorktreeRoot, managedProjectRoots = [] } = dependencies;
  const managedRoots = managedProjectRoots.map(normalize).filter(Boolean);

  /**
   * 解析单个目录，三级回退：托管根 → 已配置项目 → git 主 worktree 根。
   * 任一级失败都不抛错，最坏退回目录本身，保证每个会话总有确定的记忆
   * 归属；空输入返回空串而不是默认项目。
   */
  return async (directory) => {
    const resolved = normalize(directory);
    if (!resolved) {
      return '';
    }

    const managedRoot = managedRoots.find((root) => {
      const relative = path.relative(root, resolved);
      return relative === '' || (!relative.startsWith('..') && !path.isAbsolute(relative));
    });
    if (managedRoot) {
      return createProjectIdFromPath(managedRoot);
    }

    let configured = [];
    try {
      configured = ((await listProjectPaths()) || []).map(normalize).filter(Boolean);
    } catch {
      // An unreadable project list must not lose the memory: the git-derived
      // root below still converges every worktree of the repository on one
      // store rather than scattering one per checkout.
    }
    if (configured.includes(resolved)) {
      return createProjectIdFromPath(resolved);
    }

    let primaryRoot = '';
    try {
      primaryRoot = normalize((await resolvePrimaryWorktreeRoot(resolved))?.root);
    } catch {
      // Not a git checkout, or git is unavailable.
    }

    return createProjectIdFromPath(primaryRoot || resolved);
  };
};
