import { isGeneratedArtifact } from './generated.js';
import { parseDiffFiles } from './hunks.js';

// The digest is what the model actually reads. Within what it covers there is
// no truncation: a diff that does not fit the model's context is refused
// upstream so the user can pick a roomier model, because a walkthrough written
// against a silently clipped diff is confidently wrong in a way nobody can see.
//
// The one thing it does not cover is tool-produced files (lockfiles, minified
// bundles, codegen). Those are excluded by name, not by size, and they are not
// hidden — they carry no hunk aliases, so nothing can anchor to them, and they
// surface in the uncovered tail like any other unreviewed change.

/**
 * walkthrough 的 digest 构建模块：把各来源 section 的 patch 解析成文件与
 * hunk，产出模型实际阅读的紧凑摘要，并维护 hunk 真实 id 与请求内别名
 * （h1、h2 …）的双向映射，供生成结果落库时把别名还原回 hunk。
 */
/**
 * Parse sections into files and build the model-facing digest.
 *
 * Hunks are exposed to the model as request-local aliases (`h1`, `h2`, …)
 * rather than their real ids: the aliases are far cheaper in tokens, and a
 * model cannot invent a plausible-looking id for a hunk that does not exist.
 *
 * 中文补充：sections 来自不同来源（工作区 / 分支 / PR）；被识别为生成物
 * 的文件不进入 digest、不分配别名，但仍保留在 files 中供客户端展示。
 * 返回 { digest, files, idByAlias, aliasById, hunkCount, fileCount,
 * generatedFileCount, generatedPaths }，其中 hunkCount 与 fileCount 只统计
 * 可评审部分。
 */
export function buildDigest(sections) {
  const files = [];
  for (const section of sections) {
    const parsed = parseDiffFiles(section.patch, section.scope);
    for (const file of parsed.files) {
      files.push({ ...file, scope: section.scope, generated: isGeneratedArtifact(file.path) });
    }
  }

  // 别名双向映射：idByAlias（h1 -> 真实 hunk id）与 aliasById（反向表）；
  // counter 全局递增，保证别名在整次请求内唯一。
  const idByAlias = new Map();
  const aliasById = new Map();
  let counter = 0;

  // 模型可见的文件视图：剔除生成物；每个 hunk 重写为别名 + 头部 + 新旧行
  // 区间 + 增删行数 + patch 正文，比携带真实 id 更省 token 且无法被捏造。
  const digestFiles = files
    .filter((file) => !file.generated)
    .map((file) => ({
      path: file.path,
      ...(file.oldPath ? { oldPath: file.oldPath } : {}),
      status: file.status,
      ...(file.scope !== 'branch' && !file.scope.startsWith('pr:') ? { scope: file.scope } : {}),
      ...(file.binary ? { binary: true } : {}),
      hunks: file.hunks.map((hunk) => {
        counter += 1;
        const alias = `h${counter}`;
        idByAlias.set(alias, hunk.id);
        aliasById.set(hunk.id, alias);
        return {
          alias,
          header: hunk.header,
          oldLines: `${hunk.oldStart}-${hunk.oldStart + Math.max(0, hunk.oldLines - 1)}`,
          newLines: `${hunk.newStart}-${hunk.newStart + Math.max(0, hunk.newLines - 1)}`,
          added: hunk.added,
          deleted: hunk.deleted,
          patch: hunk.body,
        };
      }),
    }));

  // 被剔除的生成物单独收集，用于计数并向客户端说明哪些变更未进入评审。
  const generatedFiles = files.filter((file) => file.generated);

  return {
    // 模型可见的精简视图（不含生成物）。
    digest: { files: digestFiles },
    // 原始文件列表（含生成物标记），供客户端展示未覆盖的变更。
    files,
    idByAlias,
    aliasById,
    // Reviewable counts: what the model is actually asked about. The excluded
    // files still reach the client through `files`.
    hunkCount: counter,
    fileCount: digestFiles.length,
    generatedFileCount: generatedFiles.length,
    generatedPaths: generatedFiles.map((file) => file.path),
  };
}
