/**
 * 技能仓库扫描模块：浅克隆一个 git 仓库，枚举其中所有含 SKILL.md 的技能
 * 目录，解析 YAML frontmatter（name/description），并标注可安装性与告警。
 * 优先用 sparse-checkout 只检出 SKILL.md 再从磁盘解析（免去逐个 git show），
 * 失败时回退到 git ls-tree + git show 逐文件读取。
 */
import fs from 'fs';
import os from 'os';
import path from 'path';
import yaml from 'yaml';

import { assertGitAvailable, looksLikeAuthError, runGit } from './git.js';
import { parseSkillRepoSource } from './source.js';

/** OpenCode 合法技能名的正则：小写字母/数字/连字符，首尾不能是连字符（另允许单个字符）。 */
const SKILL_NAME_PATTERN = /^[a-z0-9][a-z0-9-]*[a-z0-9]$|^[a-z0-9]$/;

/** 校验技能名是否为 OpenCode 合法格式：字符串、1-64 字符且匹配 SKILL_NAME_PATTERN。 */
function validateSkillName(skillName) {
  if (typeof skillName !== 'string') return false;
  if (skillName.length < 1 || skillName.length > 64) return false;
  return SKILL_NAME_PATTERN.test(skillName);
}

/**
 * 解析 SKILL.md 的 YAML frontmatter。缺分隔符或 YAML 解析失败均非致命：
 * 返回空 frontmatter 并附告警，扫描结果仍能列出该技能。
 */
function parseSkillMd(content) {
  const text = typeof content === 'string' ? content : '';
  const match = text.match(/^---\r?\n([\s\S]*?)\r?\n---\r?\n([\s\S]*)$/);
  if (!match) {
    return {
      ok: true,
      frontmatter: {},
      warnings: ['Invalid SKILL.md: missing YAML frontmatter delimiter'],
    };
  }

  try {
    const frontmatter = yaml.parse(match[1]) || {};
    return { ok: true, frontmatter, warnings: [] };
  } catch {
    return {
      ok: true,
      frontmatter: {},
      warnings: ['Invalid SKILL.md: failed to parse YAML frontmatter'],
    };
  }
}

/** 递归删除目录（recursive + force），失败静默忽略；仅用于临时克隆目录的清理。 */
async function safeRm(dir) {
  try {
    await fs.promises.rm(dir, { recursive: true, force: true });
  } catch {
    // ignore
  }
}

/**
 * 浅克隆（--depth 1 --no-checkout）到 tempDir。优先 blob:none 部分克隆，
 * 旧版 git 不支持时回退普通浅克隆；两次都失败时返回最后一次的错误。
 */
async function cloneRepo({ cloneUrl, identity, tempDir }) {
  const preferred = ['clone', '--depth', '1', '--filter=blob:none', '--no-checkout', cloneUrl, tempDir];
  const fallback = ['clone', '--depth', '1', '--no-checkout', cloneUrl, tempDir];

  const result = await runGit(preferred, { identity, timeoutMs: 60_000 });
  if (result.ok) return { ok: true };

  const fallbackResult = await runGit(fallback, { identity, timeoutMs: 60_000 });
  if (fallbackResult.ok) return { ok: true };

  return {
    ok: false,
    error: fallbackResult,
  };
}

/**
 * 扫描仓库中的全部技能。返回 { ok, normalizedRepo, effectiveSubpath, items }，
 * items 按技能名排序，每项含 skillDir、skillName、frontmatterName、
 * description、installable 与 warnings。克隆认证失败返回 authRequired；
 * subpath 不存在时视为空扫描（items 为空数组）而非错误。
 */
export async function scanSkillsRepository({
  source,
  subpath,
  defaultSubpath,
  identity,
} = {}) {
  const gitCheck = await assertGitAvailable();
  if (!gitCheck.ok) {
    return { ok: false, error: gitCheck.error };
  }

  const parsed = parseSkillRepoSource(source, { subpath });
  if (!parsed.ok) {
    return { ok: false, error: parsed.error };
  }

  const effectiveSubpath = parsed.effectiveSubpath || (typeof defaultSubpath === 'string' && defaultSubpath.trim() ? defaultSubpath.trim() : null);
  const cloneUrl = identity?.sshKey ? parsed.cloneUrlSsh : parsed.cloneUrlHttps;

  const tempBase = await fs.promises.mkdtemp(path.join(os.tmpdir(), 'ompchamber-skills-scan-'));

  try {
    const cloned = await cloneRepo({ cloneUrl, identity, tempDir: tempBase });
    if (!cloned.ok) {
      const msg = `${cloned.error?.stderr || ''}\n${cloned.error?.message || ''}`.trim();
      if (looksLikeAuthError(msg)) {
        return { ok: false, error: { kind: 'authRequired', message: 'Authentication required to access this repository', sshOnly: true } };
      }
      return { ok: false, error: { kind: 'networkError', message: msg || 'Failed to clone repository' } };
    }

    /** 把仓库内相对 POSIX 路径转成 tempBase 下的本地文件系统路径。 */
    const toFsPath = (posixPath) => path.join(tempBase, ...String(posixPath || '').split('/').filter(Boolean));

    const patterns = effectiveSubpath
      ? [`${effectiveSubpath}/SKILL.md`, `${effectiveSubpath}/**/SKILL.md`]
      : ['SKILL.md', '**/SKILL.md'];

    let skillMdPaths = null;

    // Fast path: sparse checkout only SKILL.md files, then parse from disk.
    // This avoids one `git show` per skill.
    const sparseInit = await runGit(['-C', tempBase, 'sparse-checkout', 'init', '--no-cone'], { identity, timeoutMs: 15_000 });
    if (sparseInit.ok) {
      const sparseSet = await runGit(['-C', tempBase, 'sparse-checkout', 'set', ...patterns], { identity, timeoutMs: 30_000 });
      if (sparseSet.ok) {
        const checkout = await runGit(['-C', tempBase, 'checkout', '--force', 'HEAD'], { identity, timeoutMs: 60_000 });
        if (checkout.ok) {
          const lsFiles = await runGit(['-C', tempBase, 'ls-files'], { identity, timeoutMs: 15_000 });
          if (lsFiles.ok) {
            skillMdPaths = lsFiles.stdout
              .split(/\r?\n/)
              .map((line) => line.trim())
              .filter(Boolean)
              .filter((p) => p.endsWith('/SKILL.md') || p === 'SKILL.md');
          }
        }
      }
    }

    // Fallback: list tree and read SKILL.md blobs via git.
    if (!Array.isArray(skillMdPaths)) {
      const listArgs = ['-C', tempBase, 'ls-tree', '-r', '--name-only', 'HEAD'];
      if (effectiveSubpath) {
        listArgs.push('--', effectiveSubpath);
      }

      const listResult = await runGit(listArgs, { identity, timeoutMs: 30_000 });
      if (!listResult.ok) {
        // If subpath doesn't exist, treat as empty scan.
        return {
          ok: true,
          normalizedRepo: parsed.normalizedRepo,
          effectiveSubpath,
          items: [],
        };
      }

      skillMdPaths = listResult.stdout
        .split(/\r?\n/)
        .map((line) => line.trim())
        .filter(Boolean)
        .filter((p) => p.endsWith('/SKILL.md') || p === 'SKILL.md');
    }

    // Root-level SKILL.md doesn't map cleanly to OpenCode's "skill name == folder name" convention.
    const uniqueSkillDirs = Array.from(
      new Set(
        skillMdPaths
          .filter((p) => p !== 'SKILL.md')
          .map((p) => path.posix.dirname(p))
      )
    );

    const items = [];
    const maxParallel = 10;
    let idx = 0;

    /** 并发 worker：从共享下标领取技能目录，读 SKILL.md（磁盘优先、git show 兜底）并解析出目录项。 */
    const worker = async () => {
      while (idx < uniqueSkillDirs.length) {
        const skillDir = uniqueSkillDirs[idx++];
        const skillName = path.posix.basename(skillDir);
        const skillMdPath = path.posix.join(skillDir, 'SKILL.md');

        const warnings = [];
        let skillMdContent = '';

        // Prefer filesystem reads when sparse checkout succeeded.
        const filePath = toFsPath(skillMdPath);
        try {
          skillMdContent = await fs.promises.readFile(filePath, 'utf8');
        } catch {
          const showResult = await runGit(['-C', tempBase, 'show', `HEAD:${skillMdPath}`], { identity, timeoutMs: 15_000 });
          if (!showResult.ok) {
            warnings.push('Failed to read SKILL.md');
          } else {
            skillMdContent = showResult.stdout;
          }
        }

        const parsedMd = parseSkillMd(skillMdContent);
        warnings.push(...(parsedMd.warnings || []));

        const description = typeof parsedMd.frontmatter?.description === 'string' ? parsedMd.frontmatter.description : undefined;
        const frontmatterName = typeof parsedMd.frontmatter?.name === 'string' ? parsedMd.frontmatter.name : undefined;

        const installable = validateSkillName(skillName);
        if (!installable) {
          warnings.push('Skill directory name is not a valid OpenCode skill name');
        }

        items.push({
          repoSource: source,
          repoSubpath: effectiveSubpath || undefined,
          skillDir,
          skillName,
          frontmatterName,
          description,
          installable,
          warnings: warnings.length ? warnings : undefined,
        });
      }
    };

    await Promise.all(Array.from({ length: Math.min(maxParallel, uniqueSkillDirs.length || 1) }, () => worker()));

    // Stable ordering for UX
    items.sort((a, b) => a.skillName.localeCompare(b.skillName));

    return {
      ok: true,
      normalizedRepo: parsed.normalizedRepo,
      effectiveSubpath,
      items,
    };
  } finally {
    await safeRm(tempBase);
  }
}
