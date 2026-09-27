/**
 * small-model 服务层（index.js）测试套件：不可用 provider 的拒绝、模型
 * 选择器的 provider 可见性、超长输入的截断与报错策略、describeSmallModel
 * 的能力上报，以及输出预算与输入预留的一致性。通过 vi.mock 隔离
 * auth/config/catalog/call/runtime-providers，并把 OMPCHAMBER_DATA_DIR
 * 指向临时目录，避免读到开发者本机设置。
 */
import fs from 'fs';
import os from 'os';
import path from 'path';
import { afterAll, afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

// The settings override is read straight from disk at module load, so without
// this the suite would resolve whatever small model the developer running it
// happens to have configured.
/** 临时数据目录：隔离本套件对 settings.json 的读取，不受本机配置影响。 */
const TEMP_DATA_DIR = fs.mkdtempSync(path.join(os.tmpdir(), 'small-model-settings-'));
process.env.OMPCHAMBER_DATA_DIR = TEMP_DATA_DIR;

vi.mock('../opencode/auth.js', () => ({ readAuthFile: vi.fn() }));
vi.mock('../opencode/shared.js', () => ({
  readConfig: vi.fn(),
  readConfigLayers: vi.fn(),
}));
vi.mock('./catalog.js', () => ({
  getModelCatalog: vi.fn(),
  getCatalogProvider: vi.fn(),
}));
vi.mock('./call.js', () => ({
  DEDICATED_WIRE_FORMAT_PROVIDERS: new Set(['github-copilot', 'copilot', 'openai', 'anthropic', 'google']),
  callSmallModel: vi.fn(),
  resolveProviderLogin: vi.fn(async ({ auth, providerID }) => {
    const entry = auth?.[providerID];
    return entry && typeof entry === 'object' ? entry : null;
  }),
}));
vi.mock('./runtime-providers.js', () => ({
  getRuntimeProviderSnapshot: vi.fn(async () => null),
}));

// 被测服务层的三个入口（vi.mock 生效后动态导入）。
const { generateSmallModelText, describeSmallModel, listAuthenticatedProviders } = await import('./index.js');
// auth.json 读取 mock，用于控制登录状态。
const { readAuthFile } = await import('../opencode/auth.js');
// 运行时 provider 快照 mock，默认返回 null（"未知"）。
const { getRuntimeProviderSnapshot } = await import('./runtime-providers.js');
// 配置层读取 mock，用于控制 small_model 配置。
const { readConfigLayers } = await import('../opencode/shared.js');
// 模型目录 mock。
const { getModelCatalog } = await import('./catalog.js');
// 底层传输调用 mock，用于拦截并检查最终发出的请求参数。
const { callSmallModel } = await import('./call.js');

// Claude Code 的无条件拒绝：不允许作为小模型调用，也不出现在选择器里。
describe('unsupported small-model providers', () => {
  beforeEach(() => {
    readAuthFile.mockReturnValue({
      'claude-code': {
        type: 'oauth',
        access: 'claude-cli-managed',
        refresh: 'claude-cli-managed',
      },
    });
    readConfigLayers.mockReturnValue({ mergedConfig: {} });
    getModelCatalog.mockResolvedValue({});
    callSmallModel.mockReset();
    getRuntimeProviderSnapshot.mockResolvedValue(null);
  });

  it('rejects Claude Code with an actionable error before transport dispatch', async () => {
    await expect(generateSmallModelText({
      prompt: 'summarize this',
      model: 'claude-code/haiku',
    })).rejects.toMatchObject({
      statusCode: 422,
      code: 'small-model-provider-unsupported',
    });
    expect(callSmallModel).not.toHaveBeenCalled();
  });

  it('does not offer Claude Code in the Small Model picker', async () => {
    expect(await listAuthenticatedProviders()).not.toContain('claude-code');
  });

  // A plugin can publish an OpenAI-compatible endpoint for Claude Code, but it
  // is a façade over the Claude Agent SDK: every call spawns the CLI and
  // spends the user's Claude subscription. The refusal is about that cost, so
  // an available endpoint must not lift it.
  it('still refuses Claude Code when a plugin publishes an HTTP endpoint for it', async () => {
    getRuntimeProviderSnapshot.mockResolvedValue({
      providers: new Map([['claude-code', { id: 'claude-code', apiKey: 'plugin-key', baseURL: 'http://127.0.0.1:60668/v1', anonymousZen: false }]]),
      connected: new Set(['claude-code']),
    });

    await expect(generateSmallModelText({
      prompt: 'summarize this',
      model: 'claude-code/haiku',
    })).rejects.toMatchObject({ code: 'small-model-provider-unsupported' });
    expect(await listAuthenticatedProviders()).not.toContain('claude-code');

    getRuntimeProviderSnapshot.mockResolvedValue(null);
  });
});

// 模型选择器可见的 provider 集合：插件运行时 provider、无端点 provider、zen 匿名与 OpenCode 不可达时的回退。
describe('provider availability for the model pickers', () => {
  beforeEach(() => {
    readAuthFile.mockReturnValue({ openai: { type: 'api', key: 'sk-test' } });
    readConfigLayers.mockReturnValue({ mergedConfig: {} });
    getModelCatalog.mockResolvedValue({});
    getRuntimeProviderSnapshot.mockResolvedValue(null);
  });

  /** 把 provider 数组组装成运行时快照形状（Map + Set）的便捷工厂。 */
  const snapshot = (providers, connected) => ({
    providers: new Map(providers.map((provider) => [provider.id, provider])),
    connected: new Set(connected ?? providers.map((provider) => provider.id)),
  });

  it('offers a plugin provider that OpenCode resolved at runtime', async () => {
    getRuntimeProviderSnapshot.mockResolvedValue(snapshot([
      { id: 'llmapi', apiKey: 'plugin-key', baseURL: 'https://api.llmapi.ai/v1', anonymousZen: false },
    ]));

    expect(await listAuthenticatedProviders()).toEqual(expect.arrayContaining(['openai', 'llmapi']));
  });

  it('hides a provider with no endpoint to send a request to', async () => {
    getRuntimeProviderSnapshot.mockResolvedValue(snapshot([
      { id: 'endpointless', apiKey: 'plugin-key', baseURL: null, anonymousZen: false },
    ]));

    expect(await listAuthenticatedProviders()).not.toContain('endpointless');
  });

  it('never offers opencode zen without a real login', async () => {
    // The zen sentinel is not a credential, so the snapshot carries no apiKey.
    getRuntimeProviderSnapshot.mockResolvedValue(snapshot([
      { id: 'opencode', apiKey: null, baseURL: 'https://opencode.ai/zen/v1', anonymousZen: true },
    ]));

    expect(await listAuthenticatedProviders()).not.toContain('opencode');
  });

  it('keeps the auth.json providers when OpenCode cannot be reached', async () => {
    getRuntimeProviderSnapshot.mockResolvedValue(null);

    expect(await listAuthenticatedProviders()).toContain('openai');
  });
});

// 8k context leaves 4k input tokens after the output reserve → 16k chars.
/** 测试目录：8k 上下文模型，供截断与能力断言复用。 */
const CATALOG = {
  anthropic: {
    id: 'anthropic',
    models: {
      'claude-haiku-4-5': { id: 'claude-haiku-4-5', limit: { context: 8_000 }, structured_output: true },
      'legacy-tiny': { id: 'legacy-tiny', limit: { context: 8_000 }, structured_output: false },
      'unlisted-capability': { id: 'unlisted-capability', limit: { context: 8_000 } },
    },
  },
};

/** generateSmallModelText 请求参数工厂：默认 20k 字符超长 prompt，便于覆盖截断路径。 */
const request = (overrides = {}) => ({
  prompt: 'x'.repeat(20_000),
  model: 'anthropic/claude-haiku-4-5',
  directory: '/proj',
  ...overrides,
});

// 超长输入处理策略：默认截断并标记 inputTruncated；onOverflow 为 'error' 时直接拒绝且不触达 provider。
describe('generateSmallModelText — oversized input', () => {
  beforeEach(() => {
    readAuthFile.mockReturnValue({ anthropic: { type: 'api', key: 'sk-ant' } });
    readConfigLayers.mockReturnValue({ mergedConfig: {} });
    getModelCatalog.mockResolvedValue(CATALOG);
    callSmallModel.mockReset();
    callSmallModel.mockResolvedValue('generated');
  });

  afterEach(() => {
    vi.clearAllMocks();
  });

  it('truncates and flags the response by default', async () => {
    const result = await generateSmallModelText(request());

    expect(result.inputTruncated).toBe(true);
    const sent = callSmallModel.mock.calls.at(-1)[0].prompt;
    expect(sent.length).toBeLessThan(20_000);
    expect(sent.endsWith('…')).toBe(true);
  });

  it('refuses without calling the provider when the caller cannot survive truncation', async () => {
    await expect(generateSmallModelText(request({ onOverflow: 'error' })))
      .rejects.toMatchObject({
        statusCode: 413,
        code: 'context-too-small',
        requiredChars: 20_000,
        availableChars: 16_000,
      });

    expect(callSmallModel).not.toHaveBeenCalled();
  });

  it('leaves an input that fits untouched under either policy', async () => {
    const result = await generateSmallModelText(request({ prompt: 'short prompt', onOverflow: 'error' }));

    expect(result.inputTruncated).toBeUndefined();
    expect(callSmallModel.mock.calls.at(-1)[0].prompt).toBe('short prompt');
  });

  it('forwards schema, timeout, and abort signal to the transport', async () => {
    const controller = new AbortController();
    const schema = { type: 'object' };

    await generateSmallModelText(request({
      prompt: 'short',
      responseSchema: schema,
      timeoutMs: 240_000,
      signal: controller.signal,
    }));

    expect(callSmallModel.mock.calls.at(-1)[0]).toMatchObject({
      responseSchema: schema,
      timeoutMs: 240_000,
      signal: controller.signal,
    });
  });
});

// describeSmallModel 的能力上报：输入预算、登录状态与 structured output 三态。
describe('describeSmallModel — capability reporting', () => {
  beforeEach(() => {
    readAuthFile.mockReturnValue({ anthropic: { type: 'api', key: 'sk-ant' } });
    readConfigLayers.mockReturnValue({ mergedConfig: { small_model: 'anthropic/claude-haiku-4-5' } });
    getModelCatalog.mockResolvedValue(CATALOG);
  });

  afterEach(() => {
    vi.clearAllMocks();
  });

  it('reports the input budget and a known structured-output capability', async () => {
    const described = await describeSmallModel({ directory: '/proj' });

    expect(described).toMatchObject({
      providerID: 'anthropic',
      modelID: 'claude-haiku-4-5',
      inputCharBudget: 16_000,
      contextTokens: 8_000,
      contextKnown: true,
      structuredOutput: true,
      hasLogin: true,
    });
  });

  it('reports hasLogin false when the resolved provider has no usable credential', async () => {
    readAuthFile.mockReturnValue({});

    const described = await describeSmallModel({ directory: '/proj' });

    expect(described).toMatchObject({
      providerID: 'anthropic',
      modelID: 'claude-haiku-4-5',
      hasLogin: false,
    });
  });

  it('reports an explicit false so callers can block the model', async () => {
    readConfigLayers.mockReturnValue({ mergedConfig: { small_model: 'anthropic/legacy-tiny' } });

    const described = await describeSmallModel({ directory: '/proj' });

    expect(described.structuredOutput).toBe(false);
  });

  it('reports null — not false — when the catalog omits the capability', async () => {
    readConfigLayers.mockReturnValue({ mergedConfig: { small_model: 'anthropic/unlisted-capability' } });

    const described = await describeSmallModel({ directory: '/proj' });

    expect(described.structuredOutput).toBeNull();
  });
});

// The input reserve and the requested output budget are the same number seen
// from two sides; if they drift, a caller that asks for a large answer overruns
// the model's context and the failure looks like a truncation bug.
// 输出预算与输入预留必须一致：申请的输出 token 会从输入字符预算中等量扣除。
describe('output budget and input reserve', () => {
  beforeEach(() => {
    readAuthFile.mockReturnValue({ anthropic: { type: 'api', key: 'sk-ant' } });
    readConfigLayers.mockReturnValue({ mergedConfig: {} });
    getModelCatalog.mockResolvedValue({
      anthropic: {
        id: 'anthropic',
        models: {
          roomy: { id: 'roomy', limit: { context: 100_000, output: 8_000 } },
          unlisted: { id: 'unlisted', limit: { context: 100_000 } },
        },
      },
    });
    callSmallModel.mockReset();
    callSmallModel.mockResolvedValue('generated');
  });

  afterEach(() => {
    vi.clearAllMocks();
  });

  it('caps the request at the model\'s advertised output limit', async () => {
    await generateSmallModelText({
      prompt: 'short',
      model: 'anthropic/roomy',
      maxOutputTokens: 24_000,
    });

    expect(callSmallModel.mock.calls.at(-1)[0].maxOutputTokens).toBe(8_000);
  });

  it('honours the requested budget when the catalog states no output limit', async () => {
    await generateSmallModelText({
      prompt: 'short',
      model: 'anthropic/unlisted',
      maxOutputTokens: 24_000,
    });

    expect(callSmallModel.mock.calls.at(-1)[0].maxOutputTokens).toBe(24_000);
  });

  it('reserves exactly the requested output budget from the input allowance', async () => {
    // 100k context − 24k reserved for the answer = 76k tokens ≈ 304k chars.
    await expect(generateSmallModelText({
      prompt: 'x'.repeat(304_001),
      model: 'anthropic/unlisted',
      maxOutputTokens: 24_000,
      onOverflow: 'error',
    })).rejects.toMatchObject({ code: 'context-too-small', availableChars: 304_000 });

    await expect(generateSmallModelText({
      prompt: 'x'.repeat(303_999),
      model: 'anthropic/unlisted',
      maxOutputTokens: 24_000,
      onOverflow: 'error',
    })).resolves.toBeTruthy();
  });

  it('reports the same budget through describeSmallModel', async () => {
    readConfigLayers.mockReturnValue({ mergedConfig: { small_model: 'anthropic/unlisted' } });

    const described = await describeSmallModel({ directory: '/proj', outputReserveTokens: 24_000 });

    expect(described.inputCharBudget).toBe(304_000);
  });

  // A caller that wants "as much room as this model allows" cannot name a
  // number before knowing which model it got, so it hands over the decision.
  it('lets the reserve be decided from the resolved model\'s limits', async () => {
    readConfigLayers.mockReturnValue({ mergedConfig: { small_model: 'anthropic/roomy' } });

    const described = await describeSmallModel({
      directory: '/proj',
      outputReserveTokens: ({ contextTokens, outputTokenLimit }) => Math.min(contextTokens / 10, outputTokenLimit),
    });

    // 100k context, 8k output limit -> 8k reserved, leaving 92k tokens.
    expect(described.outputTokens).toBe(8_000);
    expect(described.inputCharBudget).toBe(92_000 * 4);
  });

  it('reports the reserve it used so the caller can request the same number', async () => {
    readConfigLayers.mockReturnValue({ mergedConfig: { small_model: 'anthropic/unlisted' } });

    const described = await describeSmallModel({ directory: '/proj', outputReserveTokens: 24_000 });

    expect(described.outputTokens).toBe(24_000);
  });
});

// 清理临时数据目录。
afterAll(() => {
  fs.rmSync(TEMP_DATA_DIR, { recursive: true, force: true });
});
