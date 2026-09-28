/**
 * walkthrough 生成任务（jobs 表）的行为测试：同一来源的并发请求复用同一任务、
 * 运行状态与阶段上报、只有显式取消才终止、输出预算与超时的取值、缓存复用，
 * 以及 provider 拒绝 schema 后的降级与记忆。git 与 small-model 均 mock，但走
 * 真实的 digest 与 prompt 路径。
 */
import fs from 'fs';
import os from 'os';
import path from 'path';
import { afterAll, afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

/** 临时数据目录：在动态 import 被测模块前设置，隔离落盘副作用。 */
const TEMP_DATA_DIR = fs.mkdtempSync(path.join(os.tmpdir(), 'walkthrough-jobs-'));
// store 相关模块在 import 时读取该环境变量，必须先于 import 设置。
process.env.OMPCHAMBER_DATA_DIR = TEMP_DATA_DIR;

// Mocking git rather than this module's own source loading: fewer of our own
// seams faked means the test exercises the real digest and prompt path.
vi.mock('../git/service.js', () => ({
  getRepositoryRoot: vi.fn(async () => '/repo'),
  getDiff: vi.fn(),
  getRangeDiff: vi.fn(),
  getUntrackedDiffs: vi.fn(async () => []),
  listUntrackedPaths: vi.fn(async () => []),
}));
vi.mock('../small-model/index.js', () => ({
  describeSmallModel: vi.fn(),
  generateSmallModelText: vi.fn(),
}));
/** 被测模块的任务 API 与测试导出（在环境变量与 mock 就位后动态导入）。 */
const {
  generateWalkthrough,
  cancelWalkthroughGeneration,
  isGenerating,
  getGenerationStage,
  __testing: walkthroughTesting,
} = await import('./index.js');
/** 取回 small-model 的 mock，供各用例配置返回值。 */
const { describeSmallModel, generateSmallModelText } = await import('../small-model/index.js');
/** 取回 git service 的 getDiff mock。 */
const { getDiff } = await import('../git/service.js');

// bun's vitest shim has no `vi.waitFor`.
/** 轮询断言辅助：每 interval 毫秒检查一次 predicate，超时抛错（补 bun 的 vitest shim 缺失的 vi.waitFor）。 */
const waitFor = async (predicate, { timeout = 2_000, interval = 5 } = {}) => {
  const deadline = Date.now() + timeout;
  for (;;) {
    if (predicate()) return;
    if (Date.now() > deadline) throw new Error('waitFor timed out');
    await new Promise((resolve) => setTimeout(resolve, interval));
  }
};

/** 测试用来源：working-tree 全部改动。 */
const SOURCE = { kind: 'working-tree', scope: 'all' };

/** 测试用 diff：单文件单 hunk。 */
const PATCH = `diff --git a/src/a.ts b/src/a.ts
--- a/src/a.ts
+++ b/src/a.ts
@@ -1,1 +1,2 @@
+const added = true;
`;

/** 测试用模型回复：一章一停靠点的合法 walkthrough JSON。 */
const RESPONSE = JSON.stringify({
  title: 'Change',
  focus: 'why',
  chapters: [{
    title: 'Data',
    icon: 'doc',
    blurb: '',
    stops: [{ title: 'Adds a flag', hunks: ['h1'], importance: 'normal', prose: 'It adds a flag.' }],
  }],
});

/** 生成任务的生命周期：并发去重、状态上报、显式取消、预算一致性与缓存复用。 */
describe('generation jobs', () => {
  beforeEach(() => {
    fs.rmSync(path.join(TEMP_DATA_DIR, 'walkthroughs'), { recursive: true, force: true });
    describeSmallModel.mockResolvedValue({
      providerID: 'anthropic',
      modelID: 'claude-haiku-4-5',
      source: 'config',
      inputCharBudget: 1_000_000,
      structuredOutput: true,
    });
    getDiff.mockImplementation(async (_dir, options) => (options?.staged ? '' : PATCH));
    generateSmallModelText.mockReset();
  });

  afterEach(async () => {
    if (isGenerating('/repo', 'working-tree:all')) {
      await cancelWalkthroughGeneration({ directory: '/repo', source: SOURCE }).catch(() => {});
    }
  });

  afterAll(() => {
    fs.rmSync(TEMP_DATA_DIR, { recursive: true, force: true });
  });

  it('runs a second request against the same job instead of paying twice', async () => {
    let release;
    generateSmallModelText.mockImplementation(() => new Promise((resolve) => {
      release = () => resolve({ text: RESPONSE });
    }));

    const first = generateWalkthrough({ directory: '/repo', source: SOURCE });
    // Let the first call reach the model before the second arrives, which is
    // what a refresh-then-press-again actually looks like.
    await waitFor(() => generateSmallModelText.mock.calls.length === 1);
    const second = generateWalkthrough({ directory: '/repo', source: SOURCE });

    release();
    const [a, b] = await Promise.all([first, second]);

    expect(generateSmallModelText).toHaveBeenCalledTimes(1);
    expect(a.walkthrough.title).toBe('Change');
    expect(b).toBe(a);
  });

  it('reports a running job so a returning client can show progress', async () => {
    let release;
    generateSmallModelText.mockImplementation(() => new Promise((resolve) => {
      release = () => resolve({ text: RESPONSE });
    }));

    const running = generateWalkthrough({ directory: '/repo', source: SOURCE });
    await waitFor(() => isGenerating('/repo', 'working-tree:all'));

    release();
    await running;

    expect(isGenerating('/repo', 'working-tree:all')).toBe(false);
  });

  it('stops only on an explicit cancel', async () => {
    generateSmallModelText.mockImplementation(({ signal }) => new Promise((_resolve, reject) => {
      signal?.addEventListener('abort', () => reject(Object.assign(new Error('aborted'), { name: 'AbortError' })));
    }));

    const running = generateWalkthrough({ directory: '/repo', source: SOURCE });
    await waitFor(() => isGenerating('/repo', 'working-tree:all'));

    expect(await cancelWalkthroughGeneration({ directory: '/repo', source: SOURCE }))
      .toEqual({ cancelled: true });
    await expect(running).rejects.toThrow();
    expect(isGenerating('/repo', 'working-tree:all')).toBe(false);
  });

  it('reports nothing to cancel when no job is running', async () => {
    expect(await cancelWalkthroughGeneration({ directory: '/repo', source: SOURCE }))
      .toEqual({ cancelled: false });
  });

  // The reserve and the request must be the same number: asking for more than
  // was subtracted from the input allowance overruns the context mid-answer.
  it('requests exactly the budget the model resolution reserved', async () => {
    describeSmallModel.mockResolvedValue({
      providerID: 'opencode-go',
      modelID: 'deepseek-v4-flash',
      source: 'config',
      inputCharBudget: 1_000_000,
      structuredOutput: true,
      outputTokens: 96_000,
      outputTokenLimit: 384_000,
    });
    generateSmallModelText.mockResolvedValue({ text: RESPONSE });

    await generateWalkthrough({ directory: '/repo', source: SOURCE });

    expect(generateSmallModelText.mock.calls.at(-1)[0].maxOutputTokens).toBe(96_000);
  });

  it('serves the cache once the job has finished, without calling the model again', async () => {
    generateSmallModelText.mockResolvedValue({ text: RESPONSE });

    await generateWalkthrough({ directory: '/repo', source: SOURCE });
    generateSmallModelText.mockClear();

    const second = await generateWalkthrough({ directory: '/repo', source: SOURCE });

    expect(second.fromCache).toBe(true);
    expect(generateSmallModelText).not.toHaveBeenCalled();
  });
});

// A fixed deadline made a three-hunk edit and a 500-hunk pull request wait the
// same, so the small case guarded nothing and the big case died just short of
// the finish line.
/** 超时函数：小 diff 有保底、随 hunk 数增长且有上限。 */
describe('generation timeout', () => {
  const { generationTimeoutMs } = walkthroughTesting;

  it('gives a small diff a floor rather than a proportional sliver', () => {
    expect(generationTimeoutMs(0)).toBe(120_000);
    expect(generationTimeoutMs(3)).toBe(123_000);
  });

  it('grows with the work', () => {
    expect(generationTimeoutMs(515)).toBeGreaterThan(generationTimeoutMs(138));
    expect(generationTimeoutMs(515)).toBe(635_000);
  });

  it('stays bounded so a hung connection cannot hold a job forever', () => {
    expect(generationTimeoutMs(100_000)).toBe(900_000);
  });
});

// The failure this replaced: a flat 24k ask, spent entirely on reasoning by a
// model that advertises 384k output tokens and a million of context. The ceiling
// exists because the same number is reserved out of the input allowance.
/** 输出预算函数：上下限、上下文比例与模型自身输出上限的相互作用。 */
describe('output budget', () => {
  const { walkthroughOutputTokens } = walkthroughTesting;

  it('asks a roomy model for far more than the old fixed budget', () => {
    expect(walkthroughOutputTokens({ contextTokens: 1_000_000, outputTokenLimit: 384_000 })).toBe(96_000);
  });

  it('never asks for more than the model says it can emit', () => {
    expect(walkthroughOutputTokens({ contextTokens: 202_752, outputTokenLimit: 32_768 })).toBe(32_768);
  });

  it('keeps the reserve to a share of the context', () => {
    expect(walkthroughOutputTokens({ contextTokens: 200_000, outputTokenLimit: 64_000 })).toBe(50_000);
  });

  it('holds the old floor for a small or uncatalogued model', () => {
    expect(walkthroughOutputTokens({ contextTokens: 64_000, outputTokenLimit: null })).toBe(24_000);
    expect(walkthroughOutputTokens({ contextTokens: 0, outputTokenLimit: null })).toBe(24_000);
  });

  it('yields to a model whose own limit is below the floor', () => {
    expect(walkthroughOutputTokens({ contextTokens: 128_000, outputTokenLimit: 8_192 })).toBe(8_192);
  });
});

/** 阶段上报：asking 与 retrying 的出现时机，以及任务结束后的清空。 */
describe('generation stages', () => {
  beforeEach(() => {
    // Without this the previous suite's cache entry is a hit for the same
    // content and the model is never called.
    fs.rmSync(path.join(TEMP_DATA_DIR, 'walkthroughs'), { recursive: true, force: true });
    describeSmallModel.mockResolvedValue({
      providerID: 'anthropic',
      modelID: 'claude-haiku-4-5',
      source: 'config',
      inputCharBudget: 1_000_000,
      structuredOutput: true,
    });
    getDiff.mockImplementation(async (_dir, options) => (options?.staged ? '' : PATCH));
    generateSmallModelText.mockReset();
  });

  it('reports asking while the model runs and clears when the job ends', async () => {
    let release;
    generateSmallModelText.mockImplementation(() => new Promise((resolve) => {
      release = () => resolve({ text: RESPONSE });
    }));

    const running = generateWalkthrough({ directory: '/repo', source: SOURCE });
    await waitFor(() => getGenerationStage('/repo', 'working-tree:all') === 'asking');

    release();
    await running;

    expect(getGenerationStage('/repo', 'working-tree:all')).toBeNull();
  });

  it('reports retrying only when a provider rejects the schema', async () => {
    let seen = [];
    let attempt = 0;
    generateSmallModelText.mockImplementation(async () => {
      attempt += 1;
      seen.push(getGenerationStage('/repo', 'working-tree:all'));
      if (attempt === 1) throw Object.assign(new Error('bad request'), { status: 400 });
      return { text: RESPONSE };
    });

    await generateWalkthrough({ directory: '/repo', source: SOURCE });

    expect(seen).toEqual(['asking', 'retrying']);
  });
});

// Retrying the schema on every generation means paying for a call already known
// to fail; the refusal has to be remembered.
/** schema 拒绝记忆：被 4xx 拒过的 provider 不再重复发送 schema。 */
describe('schema refusal memory', () => {
  beforeEach(() => {
    fs.rmSync(path.join(TEMP_DATA_DIR, 'walkthroughs'), { recursive: true, force: true });
    describeSmallModel.mockResolvedValue({
      providerID: 'opencode-go',
      modelID: 'deepseek-v4-flash',
      source: 'config',
      inputCharBudget: 1_000_000,
      structuredOutput: null,
    });
    getDiff.mockImplementation(async (_dir, options) => (options?.staged ? '' : PATCH));
    generateSmallModelText.mockReset();
  });

  it('stops sending a schema to a model that already rejected one', async () => {
    const sentSchema = [];
    generateSmallModelText.mockImplementation(async ({ responseSchema }) => {
      sentSchema.push(Boolean(responseSchema));
      if (responseSchema) throw Object.assign(new Error('bad request'), { status: 400 });
      return { text: RESPONSE };
    });

    await generateWalkthrough({ directory: '/repo', source: SOURCE });
    expect(sentSchema).toEqual([true, false]);

    // A different diff, so the cache cannot answer instead.
    getDiff.mockImplementation(async (_dir, options) => (
      options?.staged ? '' : PATCH.replace('const added = true;', 'const added = false;')
    ));
    await generateWalkthrough({ directory: '/repo', source: SOURCE });

    expect(sentSchema).toEqual([true, false, false]);
  });
});
