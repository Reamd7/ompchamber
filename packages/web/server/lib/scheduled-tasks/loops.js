/**
 * Markdown loops — portable scheduled-task definitions.
 *
 * Loops are git-commit-able markdown files with YAML frontmatter, discovered
 * from `.agents/loops/*.md` (project scope, including ancestor directories up
 * to the worktree root) and `~/.agents/loops/*.md` (user scope), mirroring the
 * skills discovery pattern (`packages/web/server/lib/opencode/skills.js`).
 *
 * File format:
 *
 *   ---
 *   name: daily-digest
 *   schedule: "0 9 * * *"
 *   enabled: true
 *   model: anthropic/claude-sonnet-4-5
 *   agent: plan
 *   timezone: Europe/Kyiv
 *   ---
 *   Summarize repository changes since yesterday and post the digest.
 *
 * Field mapping (see packages/ui/src/lib/scheduledTasksApi.ts):
 *   name     -> task.name
 *   schedule -> task.schedule.kind "cron" + task.schedule.cron
 *   enabled  -> task.enabled (default false — loops only run when the file
 *               explicitly enables them, so discovery never auto-executes
 *               repository content)
 *   model    -> split into task.execution.providerID / task.execution.modelID
 *   agent    -> task.execution.agent (optional)
 *   timezone -> task.schedule.timezone (optional, defaults to the server zone)
 *   body     -> task.execution.prompt
 *
 * `thinking_level` and `goalEnabled`/`goalTokenBudget` are not part of the
 * portable format (they are UI-only today); editing them in the file has no
 * effect and they remain JSON/UI-only.
 *
 * Runtime state (lastRunAt, nextRunAt, lastStatus, ...) is never written to
 * the markdown file; it continues to live in the project config/state store.
 */
/**
 * 中文说明：Markdown loops（循环任务）—— 可移植的定时任务定义。
 *
 * loop 是带 YAML frontmatter、可随 git 提交的 markdown 文件，发现位置为
 * `.agents/loops/*.md`（项目级，含项目路径向上直到 worktree 根的全部祖先
 * 目录）与 `~/.agents/loops/*.md`（用户级），与 skills 的发现模式
 * （packages/web/server/lib/opencode/skills.js）保持一致。
 *
 * 文件示例：
 *
 *   ---
 *   name: daily-digest
 *   schedule: "0 9 * * *"
 *   enabled: true
 *   model: anthropic/claude-sonnet-4-5
 *   agent: plan
 *   timezone: Europe/Kyiv
 *   ---
 *   Summarize repository changes since yesterday and post the digest.
 *
 * 字段到 scheduled task 的映射（见 packages/ui/src/lib/scheduledTasksApi.ts）：
 *   name     -> task.name（必填，超过 MAX_TASK_NAME_LENGTH 直接拒绝而非截断）
 *   schedule -> task.schedule.kind "cron" + task.schedule.cron（必填）
 *   enabled  -> task.enabled（默认 false：只有文件显式启用才会运行，
 *               因此发现过程绝不会自动执行仓库里的内容）
 *   model    -> 拆分为 task.execution.providerID / task.execution.modelID
 *   agent    -> task.execution.agent（可选）
 *   timezone -> task.schedule.timezone（可选，缺省用服务器时区）
 *   正文     -> task.execution.prompt（必填）
 *
 * `thinking_level` 与 `goalEnabled`/`goalTokenBudget` 不属于可移植格式
 * （目前仅存在于 UI/JSON）：写进文件也不生效。
 *
 * 运行时状态（lastRunAt、nextRunAt、lastStatus 等）从不写回 markdown 文件，
 * 始终保存在项目配置 / 状态存储中。
 */

import fs from 'fs';
import os from 'os';
import path from 'path';
import { parseMdFile, writeMdFile, getAncestors, findWorktreeRoot } from '../opencode/shared.js';
import { MAX_TASK_NAME_LENGTH } from '../projects/project-config.js';

/** `.agents/` 下存放 loop 定义文件的目录名。 */
const LOOP_DIR_NAME = 'loops';
/** 计算用户级 loop 根目录 `~/.agents/loops` 的绝对路径（每次调用现算，跟随 HOME）。 */
const USER_LOOP_ROOT = () => path.join(os.homedir(), '.agents', LOOP_DIR_NAME);

/**
 * 把任意值归一化为非空字符串：非字符串、空串或纯空白返回 null，
 * 否则返回去除首尾空白后的字符串。用于宽松校验 frontmatter 字段。
 */
const asNonEmptyString = (value) => {
  if (typeof value !== 'string') {
    return null;
  }
  const trimmed = value.trim();
  return trimmed.length > 0 ? trimmed : null;
};

/**
 * Split a `provider/model` string into its two parts. Splits on the first `/`
 * so model ids containing a slash (e.g. `openai/gpt-5`) still resolve.
 */
/**
 * 中文补充：把 `provider/model` 字符串拆成两部分。按第一个 `/` 切分，
 * 因此模型 id 本身含斜杠（如 `openai/gpt-5`）时仍能正确解析；
 * 缺分隔符、分隔符在首尾或任一侧为空白时返回 null。
 */
const splitProviderModel = (value) => {
  const raw = asNonEmptyString(value);
  if (!raw) {
    return null;
  }
  const separator = raw.indexOf('/');
  if (separator <= 0 || separator === raw.length - 1) {
    return null;
  }
  return {
    providerId: raw.slice(0, separator).trim(),
    modelId: raw.slice(separator + 1).trim(),
  };
};

/**
 * Parse one loop markdown file into a scheduled-task definition, or return
 * null when the file is malformed. Malformed files are skipped with a warning
 * and never prevent valid files from loading.
 */
/**
 * 中文补充：解析单个 loop markdown 文件为 scheduled-task 定义；文件畸形
 * （解析异常、缺 name/schedule/正文/model、name 超长）时打印 warning 并
 * 返回 null，绝不抛出，从而不阻碍其他合法文件加载。enabled 缺省按 false。
 */
export const parseLoopDefinition = (filePath) => {
  let parsed;
  try {
    parsed = parseMdFile(filePath);
  } catch (error) {
    console.warn(`[loops] skipped malformed loop file ${filePath}:`, error?.message ?? error);
    return null;
  }

  const frontmatter = parsed.frontmatter && typeof parsed.frontmatter === 'object'
    ? parsed.frontmatter
    : {};
  const name = asNonEmptyString(frontmatter.name);
  if (!name) {
    console.warn(`[loops] skipped ${filePath}: frontmatter "name" is required`);
    return null;
  }
  if (name.length > MAX_TASK_NAME_LENGTH) {
    // Reject instead of clamping: task names are clamped to this length at
    // storage time, so identity keys must match the stored value exactly.
    console.warn(`[loops] skipped ${filePath}: frontmatter "name" exceeds ${MAX_TASK_NAME_LENGTH} characters`);
    return null;
  }

  const cron = asNonEmptyString(frontmatter.schedule);
  if (!cron) {
    console.warn(`[loops] skipped ${filePath}: frontmatter "schedule" (cron expression) is required`);
    return null;
  }

  const prompt = asNonEmptyString(parsed.body);
  if (!prompt) {
    console.warn(`[loops] skipped ${filePath}: markdown body (the execution prompt) is required`);
    return null;
  }

  const providerModel = splitProviderModel(frontmatter.model);
  if (!providerModel) {
    console.warn(`[loops] skipped ${filePath}: frontmatter "model" must be "provider/model"`);
    return null;
  }

  const timezone = asNonEmptyString(frontmatter.timezone);
  const agent = asNonEmptyString(frontmatter.agent);

  return {
    name,
    enabled: typeof frontmatter.enabled === 'boolean' ? frontmatter.enabled : false,
    schedule: {
      kind: 'cron',
      cron,
      ...(timezone ? { timezone } : {}),
    },
    execution: {
      prompt,
      providerID: providerModel.providerId,
      modelID: providerModel.modelId,
      ...(agent ? { agent } : {}),
    },
  };
};

/**
 * 把 loop markdown 文件 frontmatter 的 `enabled` 字段改写为指定布尔值。
 * 文件当前解析不出合法定义时返回 false（拒绝写入）；写入成功返回 true。
 * 副作用：直接覆写磁盘上的 markdown 文件，保留其余 frontmatter 字段与正文。
 */
export const setLoopFileEnabled = (filePath, enabled) => {
  if (!parseLoopDefinition(filePath)) {
    return false;
  }
  const { frontmatter, body } = parseMdFile(filePath);
  writeMdFile(filePath, { ...frontmatter, enabled: Boolean(enabled) }, body);
  return true;
};

/**
 * 列出某目录下全部 `.md` 文件（仅当前层级，不递归），按文件名排序返回
 * 绝对路径数组。目录为空、不存在或读取失败时返回空数组，绝不抛出。
 */
const walkLoopMdFiles = (rootDir) => {
  if (!rootDir || !fs.existsSync(rootDir)) {
    return [];
  }
  try {
    return fs.readdirSync(rootDir, { withFileTypes: true })
      .filter((entry) => entry.isFile() && entry.name.endsWith('.md'))
      .map((entry) => path.join(rootDir, entry.name))
      .sort();
  } catch {
    return [];
  }
};

/**
 * Discover loop files for a project: `~/.agents/loops/*.md` (user scope) plus
 * `.agents/loops/*.md` in every ancestor of the project path up to the
 * worktree root (project scope).
 */
/**
 * 中文补充：发现一个项目的全部 loop 文件：用户级 `~/.agents/loops/*.md`
 * 加上项目路径到 worktree 根之间每个祖先目录里的 `.agents/loops/*.md`
 * （项目级）。返回 `{ filePath, scope }` 数组，scope 为 'user' 或 'project'。
 */
export const discoverLoopFiles = (projectPath) => {
  const files = [];
  for (const filePath of walkLoopMdFiles(USER_LOOP_ROOT())) {
    files.push({ filePath, scope: 'user' });
  }
  if (projectPath) {
    const worktreeRoot = findWorktreeRoot(projectPath) || path.resolve(projectPath);
    for (const ancestor of getAncestors(projectPath, worktreeRoot)) {
      const root = path.join(ancestor, '.agents', LOOP_DIR_NAME);
      for (const filePath of walkLoopMdFiles(root)) {
        files.push({ filePath, scope: 'project' });
      }
    }
  }
  return files;
};

/**
 * Discover and parse all loops for a project. Project-scope loops shadow
 * user-scope loops with the same name; among project files the nearest
 * ancestor wins.
 *
 * Unparseable files are reported as `{ scope, filePath, definition: null }`
 * entries instead of being dropped: the scheduler must distinguish "file is
 * gone" (unschedule its task) from "file exists but is currently malformed"
 * (keep its task with the last good definition until the file is fixed).
 * Malformed files never block valid ones in the same or other scopes.
 */
/**
 * 中文补充：发现并解析项目的全部 loop。同名遮蔽规则：项目级遮蔽用户级，
 * 项目级之间离项目最近的祖先目录优先。解析失败的文件以
 * `{ scope, filePath, definition: null }` 形式保留在结果里（而非被丢弃），
 * 以便调度器区分“文件已删除（应取消其任务）”与“文件仍在但暂时畸形
 * （保留上一个好定义直到修复）”。畸形文件从不阻塞任何合法文件。
 */
export const discoverLoops = (projectPath) => {
  const byName = new Map();
  const loops = [];
  for (const { filePath, scope } of discoverLoopFiles(projectPath)) {
    const definition = parseLoopDefinition(filePath);
    if (!definition) {
      loops.push({ scope, filePath, definition: null });
      continue;
    }
    const existing = byName.get(definition.name);
    if (existing && (existing.scope === 'project' || scope === 'user')) {
      continue;
    }
    byName.set(definition.name, { scope, filePath, definition });
  }
  for (const entry of byName.values()) {
    loops.push(entry);
  }
  return loops;
};
