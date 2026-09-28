import { describe, expect, it } from 'vitest';
import { buildDigest } from './digest.js';
import { isGeneratedArtifact } from './generated.js';

/**
 * digest 构建与生成物识别的测试套件：覆盖 isGeneratedArtifact 的命中与
 * 关键反例（误报会把真实代码悄悄挤出评审），以及 buildDigest 对生成物的
 * 剔除、别名只分配给可评审 hunk、各项计数的正确性。
 */
/** 构造一个最小合法的单 hunk unified diff 片段，默认正文为新增一行代码。 */
const fileDiff = (path, body = '+const a = 1;') => `diff --git a/${path} b/${path}
--- a/${path}
+++ b/${path}
@@ -1,1 +1,2 @@
${body}
`;

// 生成物路径识别：精确 lockfile 名与常规产物命中，近似命名的手写源码绝不误伤。
describe('isGeneratedArtifact', () => {
  it('matches lockfiles by exact name anywhere in the tree', () => {
    expect(isGeneratedArtifact('bun.lock')).toBe(true);
    expect(isGeneratedArtifact('packages/web/package-lock.json')).toBe(true);
    expect(isGeneratedArtifact('Cargo.lock')).toBe(true);
    expect(isGeneratedArtifact('go.sum')).toBe(true);
  });

  it('matches conventional generated output', () => {
    expect(isGeneratedArtifact('dist/app.min.js')).toBe(true);
    expect(isGeneratedArtifact('src/api.generated.ts')).toBe(true);
    expect(isGeneratedArtifact('proto/user.pb.go')).toBe(true);
    expect(isGeneratedArtifact('src/__snapshots__/App.test.tsx.snap')).toBe(true);
    expect(isGeneratedArtifact('src/generated/client.ts')).toBe(true);
  });

  it('does not match authored source that merely looks similar', () => {
    // A false positive silently removes real code from the review, so these
    // near-misses matter more than the hits.
    expect(isGeneratedArtifact('src/lock.ts')).toBe(false);
    expect(isGeneratedArtifact('src/useLockfile.ts')).toBe(false);
    expect(isGeneratedArtifact('src/generator.ts')).toBe(false);
    expect(isGeneratedArtifact('src/minifier.ts')).toBe(false);
    expect(isGeneratedArtifact('packages/ui/src/lib/i18n/messages/en.ts')).toBe(false);
  });
});

// digest 构建：生成物不进模型视图但仍返回给客户端；别名与计数只覆盖可评审部分。
describe('buildDigest', () => {
  // 公共夹具：一次工作区变更，含两个源文件与一个 lockfile。
  const sections = [{
    scope: 'working',
    patch: [fileDiff('src/a.ts'), fileDiff('bun.lock', '+  "version": "2",'), fileDiff('src/b.ts')].join(''),
  }];

  it('keeps generated files out of what the model sees', () => {
    const built = buildDigest(sections);

    expect(built.digest.files.map((file) => file.path)).toEqual(['src/a.ts', 'src/b.ts']);
    expect(JSON.stringify(built.digest)).not.toContain('bun.lock');
    expect(built.fileCount).toBe(2);
    expect(built.hunkCount).toBe(2);
    expect(built.generatedFileCount).toBe(1);
    expect(built.generatedPaths).toEqual(['bun.lock']);
  });

  it('still returns generated files to the client so nothing disappears', () => {
    const built = buildDigest(sections);

    expect(built.files.map((file) => file.path)).toEqual(['src/a.ts', 'bun.lock', 'src/b.ts']);
    expect(built.files.find((file) => file.path === 'bun.lock')?.generated).toBe(true);
  });

  it('gives aliases only to reviewable hunks', () => {
    const built = buildDigest(sections);
    const aliased = [...built.idByAlias.values()];

    expect([...built.idByAlias.keys()]).toEqual(['h1', 'h2']);
    expect(aliased.some((id) => id.includes('bun.lock'))).toBe(false);
  });

  it('reports zero reviewable hunks when only generated files changed', () => {
    const built = buildDigest([{ scope: 'working', patch: fileDiff('bun.lock', '+  "version": "2",') }]);

    expect(built.hunkCount).toBe(0);
    expect(built.files).toHaveLength(1);
    expect(built.generatedFileCount).toBe(1);
  });
});
