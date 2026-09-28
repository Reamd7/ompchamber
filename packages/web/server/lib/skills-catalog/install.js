/**
 * 技能安装模块：克隆 git 仓库并把选中的 skill 目录安装到用户级或项目级
 * 目录。负责参数与名称校验、冲突检测（skipAll/overwriteAll/逐项决策）、
 * sparse-checkout 选择性检出，以及拒绝 symlink 与路径穿越的安全复制。
 */
import fs from 'fs';
import os from 'os';
import path from 'path';

import { assertGitAvailable, looksLikeAuthError, runGit } from './git.js';
import { parseSkillRepoSource } from './source.js';

/** OpenCode 合法技能名的正则：小写字母/数字/连字符，首尾不能是连字符（另允许单个字符）。 */
const SKILL_NAME_PATTERN = /^[a-z0-9][a-z0-9-]*[a-z0-9]$|^[a-z0-9]$/;

/**
 * 兼容旧版单数路径 ~/.config/opencode/skill：只有旧目录存在且复数目录不存在时
 * 才沿用旧路径，否则一律视为指向 ~/.config/opencode/skills。
 */
function normalizeUserSkillDir(userSkillDir) {
  if (!userSkillDir) return null;
  const legacySkillDir = path.join(os.homedir(), '.config', 'opencode', 'skill');
  const pluralSkillDir = path.join(os.homedir(), '.config', 'opencode', 'skills');
  if (userSkillDir === legacySkillDir) {
    if (fs.existsSync(legacySkillDir) && !fs.existsSync(pluralSkillDir)) return legacySkillDir;
    return pluralSkillDir;
  }
  return userSkillDir;
}

/** 校验技能名是否为 OpenCode 合法格式：字符串、1-64 字符且匹配 SKILL_NAME_PATTERN。 */
function validateSkillName(skillName) {
  if (typeof skillName !== 'string') return false;
  if (skillName.length < 1 || skillName.length > 64) return false;
  return SKILL_NAME_PATTERN.test(skillName);
}

/** 递归删除目录（recursive + force），失败静默忽略；仅用于临时目录与安装失败的清理。 */
async function safeRm(dir) {
  try {
    await fs.promises.rm(dir, { recursive: true, force: true });
  } catch {
    // ignore
  }
}

/** 把仓库内的相对 POSIX 路径拼接为本地文件系统路径（逐段过滤空串与空白）。 */
function toFsPath(repoDir, repoRelPosixPath) {
  const parts = String(repoRelPosixPath || '')
    .split('/')
    .map((p) => p.trim())
    .filter(Boolean);
  return path.join(repoDir, ...parts);
}

/** 递归创建目录；已存在时不报错。 */
async function ensureDir(dirPath) {
  await fs.promises.mkdir(dirPath, { recursive: true });
}

/**
 * 递归复制技能目录。逐项 lstat 拒绝 symlink，并用 realpath 校验每一层的
 * 父目录仍位于 srcReal 之下以防路径穿越；文件复制后尽力保留权限位
 * （chmod 失败忽略）。socket、设备等其它类型直接跳过。
 */
async function copyDirectoryNoSymlinks(srcDir, dstDir) {
  const srcReal = await fs.promises.realpath(srcDir);
  await ensureDir(dstDir);

  /** 深度优先遍历复制：目录递归、常规文件复制、symlink 抛错。 */
  const walk = async (currentSrc, currentDst) => {
    const entries = await fs.promises.readdir(currentSrc, { withFileTypes: true });
    for (const entry of entries) {
      const nextSrc = path.join(currentSrc, entry.name);
      const nextDst = path.join(currentDst, entry.name);

      const stat = await fs.promises.lstat(nextSrc);
      if (stat.isSymbolicLink()) {
        throw new Error('Symlinks are not supported in skills');
      }

      // Guard against traversal: ensure source is still under srcReal
      const nextRealParent = await fs.promises.realpath(path.dirname(nextSrc));
      if (!nextRealParent.startsWith(srcReal)) {
        throw new Error('Invalid source path traversal detected');
      }

      if (stat.isDirectory()) {
        await ensureDir(nextDst);
        await walk(nextSrc, nextDst);
        continue;
      }

      if (stat.isFile()) {
        await ensureDir(path.dirname(nextDst));
        await fs.promises.copyFile(nextSrc, nextDst);
        try {
          await fs.promises.chmod(nextDst, stat.mode & 0o777);
        } catch {
          // best-effort
        }
        continue;
      }

      // Skip other types (sockets, devices, etc.)
    }
  };

  await walk(srcDir, dstDir);
}

/**
 * 浅克隆（--depth 1 --no-checkout）到 tempDir。优先 blob:none 部分克隆，
 * 旧版 git 不支持时回退普通浅克隆；两次都失败时返回最后一次的错误。
 */
async function cloneRepo({ cloneUrl, identity, tempDir }) {
  const preferred = ['clone', '--depth', '1', '--filter=blob:none', '--no-checkout', cloneUrl, tempDir];
  const fallback = ['clone', '--depth', '1', '--no-checkout', cloneUrl, tempDir];

  const result = await runGit(preferred, { identity, timeoutMs: 90_000 });
  if (result.ok) return { ok: true };

  const fallbackResult = await runGit(fallback, { identity, timeoutMs: 90_000 });
  if (fallbackResult.ok) return { ok: true };

  return {
    ok: false,
    error: fallbackResult,
  };
}

/**
 * 计算技能的目标安装目录。scope=user 时装到用户目录（agents 源为
 * ~/.agents/skills，opencode 源为 userSkillDir）；scope=project 时必须提供
 * workingDirectory，装到其下的 .agents/skills 或 .opencode/skills。
 */
function getTargetSkillDir({ scope, targetSource, workingDirectory, userSkillDir, skillName }) {
  const source = targetSource === 'agents' ? 'agents' : 'opencode';

  if (scope === 'user') {
    if (source === 'agents') {
      return path.join(os.homedir(), '.agents', 'skills', skillName);
    }
    return path.join(userSkillDir, skillName);
  }

  if (!workingDirectory) {
    throw new Error('workingDirectory is required for project installs');
  }

  if (source === 'agents') {
    return path.join(workingDirectory, '.agents', 'skills', skillName);
  }

  return path.join(workingDirectory, '.opencode', 'skills', skillName);
}

/**
 * 从 git 仓库安装选中的技能。流程：校验 git 可用与各项参数 → 解析仓库源 →
 * 无副作用地预检名称与冲突（存在同名目录、无逐项决策且无自动策略时返回
 * kind 'conflicts' 交给调用方确认）→ 临时目录克隆 + sparse-checkout 检出
 * 所选目录 → 逐项安装（缺 SKILL.md 或名称非法则跳过；冲突按 skip/overwrite
 * 决策；复制失败删除半成品）。返回 { ok, installed, skipped }；克隆认证
 * 失败返回 authRequired（sshOnly）。临时目录在 finally 中清理。
 */
export async function installSkillsFromRepository({
  source,
  subpath,
  defaultSubpath,
  identity,
  scope,
  targetSource,
  workingDirectory,
  userSkillDir,
  selections,
  conflictPolicy,
  conflictDecisions,
} = {}) {
  const gitCheck = await assertGitAvailable();
  if (!gitCheck.ok) {
    return { ok: false, error: gitCheck.error };
  }

  const normalizedUserSkillDir = normalizeUserSkillDir(userSkillDir);
  if (normalizedUserSkillDir) {
    userSkillDir = normalizedUserSkillDir;
  }

  if (!userSkillDir) {
    return { ok: false, error: { kind: 'unknown', message: 'userSkillDir is required' } };
  }

  if (scope !== 'user' && scope !== 'project') {
    return { ok: false, error: { kind: 'invalidSource', message: 'Invalid scope' } };
  }

  if (targetSource !== undefined && targetSource !== 'opencode' && targetSource !== 'agents') {
    return { ok: false, error: { kind: 'invalidSource', message: 'Invalid target source' } };
  }

  if (scope === 'project' && !workingDirectory) {
    return { ok: false, error: { kind: 'invalidSource', message: 'Project installs require a directory parameter' } };
  }

  const parsed = parseSkillRepoSource(source, { subpath });
  if (!parsed.ok) {
    return { ok: false, error: parsed.error };
  }

  const effectiveSubpath = parsed.effectiveSubpath || (typeof defaultSubpath === 'string' && defaultSubpath.trim() ? defaultSubpath.trim() : null);
  void effectiveSubpath;

  const cloneUrl = identity?.sshKey ? parsed.cloneUrlSsh : parsed.cloneUrlHttps;

  const requestedDirs = Array.isArray(selections) ? selections.map((s) => String(s?.skillDir || '').trim()).filter(Boolean) : [];
  if (requestedDirs.length === 0) {
    return { ok: false, error: { kind: 'invalidSource', message: 'No skills selected for installation' } };
  }

  // Validate names early and compute conflicts without mutating.
  const skillPlans = requestedDirs.map((skillDirPosix) => {
    const skillName = path.posix.basename(skillDirPosix);
    return { skillDirPosix, skillName, installable: validateSkillName(skillName) };
  });

  const conflicts = [];
  for (const plan of skillPlans) {
    if (!plan.installable) {
      continue;
    }

    const targetDir = getTargetSkillDir({ scope, targetSource, workingDirectory, userSkillDir, skillName: plan.skillName });
    if (fs.existsSync(targetDir)) {
      const decision = conflictDecisions?.[plan.skillName];
      const hasAutoPolicy = conflictPolicy === 'skipAll' || conflictPolicy === 'overwriteAll';
      if (!decision && !hasAutoPolicy) {
        conflicts.push({ skillName: plan.skillName, scope, source: targetSource === 'agents' ? 'agents' : 'opencode' });
      }
    }
  }

  if (conflicts.length > 0) {
    return {
      ok: false,
      error: {
        kind: 'conflicts',
        message: 'Some skills already exist in the selected scope',
        conflicts,
      },
    };
  }

  const tempBase = await fs.promises.mkdtemp(path.join(os.tmpdir(), 'ompchamber-skills-install-'));

  try {
    const cloned = await cloneRepo({ cloneUrl, identity, tempDir: tempBase });
    if (!cloned.ok) {
      const msg = `${cloned.error?.stderr || ''}\n${cloned.error?.message || ''}`.trim();
      if (looksLikeAuthError(msg)) {
        return { ok: false, error: { kind: 'authRequired', message: 'Authentication required to access this repository', sshOnly: true } };
      }
      return { ok: false, error: { kind: 'networkError', message: msg || 'Failed to clone repository' } };
    }

    // Selective checkout for only requested skill dirs.
    await runGit(['-C', tempBase, 'sparse-checkout', 'init', '--cone'], { identity, timeoutMs: 15_000 });
    const setResult = await runGit(['-C', tempBase, 'sparse-checkout', 'set', ...requestedDirs], { identity, timeoutMs: 30_000 });
    if (!setResult.ok) {
      return { ok: false, error: { kind: 'unknown', message: setResult.stderr || setResult.message || 'Failed to configure sparse checkout' } };
    }

    const checkoutResult = await runGit(['-C', tempBase, 'checkout', '--force', 'HEAD'], { identity, timeoutMs: 60_000 });
    if (!checkoutResult.ok) {
      return { ok: false, error: { kind: 'unknown', message: checkoutResult.stderr || checkoutResult.message || 'Failed to checkout repository' } };
    }

    const installed = [];
    const skipped = [];

    for (const plan of skillPlans) {
      if (!plan.installable) {
        skipped.push({ skillName: plan.skillName, reason: 'Invalid skill name (directory basename)' });
        continue;
      }

      const srcDir = toFsPath(tempBase, plan.skillDirPosix);
      const skillMdPath = path.join(srcDir, 'SKILL.md');
      if (!fs.existsSync(skillMdPath)) {
        skipped.push({ skillName: plan.skillName, reason: 'SKILL.md not found in selected directory' });
        continue;
      }

      const targetDir = getTargetSkillDir({ scope, targetSource, workingDirectory, userSkillDir, skillName: plan.skillName });
      const exists = fs.existsSync(targetDir);

      let decision = conflictDecisions?.[plan.skillName] || null;
      if (!decision) {
        if (exists && conflictPolicy === 'skipAll') decision = 'skip';
        if (exists && conflictPolicy === 'overwriteAll') decision = 'overwrite';
        if (!exists) decision = 'overwrite'; // no conflict, proceed
      }

      if (exists && decision === 'skip') {
        skipped.push({ skillName: plan.skillName, reason: 'Already installed (skipped)' });
        continue;
      }

      if (exists && decision === 'overwrite') {
        await safeRm(targetDir);
      }

      // Ensure project parent directories exist
      await ensureDir(path.dirname(targetDir));

      try {
        await copyDirectoryNoSymlinks(srcDir, targetDir);
        installed.push({ skillName: plan.skillName, scope, source: targetSource === 'agents' ? 'agents' : 'opencode' });
      } catch (error) {
        await safeRm(targetDir);
        skipped.push({
          skillName: plan.skillName,
          reason: error instanceof Error ? error.message : 'Failed to copy skill files',
        });
      }
    }

    return { ok: true, installed, skipped };
  } finally {
    await safeRm(tempBase);
  }
}
