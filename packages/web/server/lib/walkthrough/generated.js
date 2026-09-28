// Files that are produced by a tool rather than written by a person. Their
// diffs are enormous, carry no intent, and are exactly the kind of content a
// reviewer scrolls past — but they are still part of the change, so they are
// never hidden: they are kept out of the model's input and shown in the
// uncovered tail instead.
// 中文：本模块识别“由工具生成、而非人手写”的文件。这类文件的 diff 巨大且
// 不承载意图，reviewer 只会快速划过——但它们仍是变更的一部分，因此从不
// 隐藏：只是不进入模型的输入，改在未覆盖尾部展示。

/** 各语言生态的锁文件名（按文件名精确匹配）。 */
const LOCKFILES = new Set([
  'bun.lock',
  'bun.lockb',
  'package-lock.json',
  'npm-shrinkwrap.json',
  'yarn.lock',
  'pnpm-lock.yaml',
  'composer.lock',
  'Gemfile.lock',
  'Pipfile.lock',
  'poetry.lock',
  'uv.lock',
  'Cargo.lock',
  'go.sum',
  'mix.lock',
  'pubspec.lock',
  'flake.lock',
  'gradle.lockfile',
  'packages.lock.json',
  'deno.lock',
]);

/** 生成产物的路径/文件名模式（正则匹配整段路径）。 */
const GENERATED_PATTERNS = [
  // Minified or bundled output committed to the repository.
  // 压缩或打包产物。
  /\.min\.(js|css)$/i,
  /\.(js|css)\.map$/i,
  // Conventional "this file is generated" naming.
  // 约定俗成的“生成文件”命名。
  /\.generated\.[^/]+$/i,
  /\.gen\.[^/]+$/i,
  /(^|\/)generated\//i,
  // Protocol buffers and similar codegen.
  // protobuf 等代码生成产物。
  /\.pb\.(go|ts|js)$/i,
  /_pb2(_grpc)?\.py$/i,
  /\.pb\.cc$|\.pb\.h$/i,
  // Test snapshots.
  // 测试快照。
  /(^|\/)__snapshots__\//,
  /\.snap$/,
];

/**
 * Whether a path is a tool-produced artifact rather than authored source.
 *
 * Deliberately conservative: a false positive silently removes real code from
 * the review, which is the failure this whole feature exists to prevent. Only
 * unambiguous, conventional names qualify.
 *
 * 中文：判断路径是否为工具产物而非手写源码。
 *
 * 刻意保守：误判会把真实代码从 review 中悄悄剔除，正是本功能要防止的
 * 失败模式；只有无歧义、约定俗成的命名才命中。锁文件按文件名精确匹配，
 * 其余按模式匹配整段路径；非字符串或空输入一律视为手写。
 */
export function isGeneratedArtifact(filePath) {
  if (typeof filePath !== 'string' || !filePath) return false;
  const name = filePath.split('/').pop() || '';
  if (LOCKFILES.has(name)) return true;
  return GENERATED_PATTERNS.some((pattern) => pattern.test(filePath));
}
