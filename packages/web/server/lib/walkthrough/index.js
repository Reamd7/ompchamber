/**
 * Walkthrough（代码变更导读）生成服务：把当前 diff 解析为文件与 hunk，交给
 * small-model 生成结构化的"阅读顺序"叙事（章节/停靠点，锚定到 hunk id），结果
 * 按 diff+模型+语言缓存并在仓库内记指针。生成是显式用户动作，任务表按仓库+
 * 来源去重且独立于请求生命周期（断线不取消）；同时暴露读取（getWalkthrough，
 * 不花 token）、就绪度判断与取消接口。本模块由路由层懒加载。
 */
import { getRepositoryRoot } from '../git/service.js';
import { describeSmallModel, generateSmallModelText } from '../small-model/index.js';
import { buildDigest } from './digest.js';
import { indexHunks } from './hunks.js';
import { normalizeLanguage } from './languages.js';
import { buildPrompt, JSON_SHAPE_INSTRUCTION } from './prompt.js';
import { normalizeWalkthrough, parseModelJson, responseSchema } from './schema.js';
import {
  buildCacheKey,
  pruneMissingRepositories,
  readCachedWalkthrough,
  readPointer,
  writeCachedWalkthrough,
  writePointer,
} from './store.js';
import { readWalkthroughModelOverride } from './model-settings.js';
import { loadSourceSections, parseSource, sourceKey, WalkthroughSourceError } from './sources.js';

// Walkthrough generation is always user-initiated and never automatic: it costs
// tokens, and a background regeneration on every keystroke would be a way to
// spend a budget without anyone deciding to.

// This module is imported lazily, which means module-level work lands on the
// first walkthrough request. Housekeeping has no business being there, so it is
// deferred and never awaited: the request proceeds immediately and the prune
// interleaves behind it.
setTimeout(() => {
  void pruneMissingRepositories().catch(() => {
    // Housekeeping failing is not worth surfacing or retrying.
  });
}, 0).unref?.();

// A hang guard, not a pace-setter. Losing a nearly-finished generation wastes
// real money and minutes, while an over-long deadline only holds a job slot, so
// this errs long. It scales because a three-hunk edit and a 500-hunk pull
// request have no business sharing a deadline.
/** 生成超时的基础时长（毫秒）：小 diff 的保底等待窗口。 */
const GENERATION_TIMEOUT_BASE_MS = 120_000;
/** 每个 hunk 额外累加的超时（毫秒），让大 diff 按规模延长限。 */
const GENERATION_TIMEOUT_PER_HUNK_MS = 1_000;
/** 生成超时的封顶值（毫秒），防止超大 diff 把时限推得不可接受。 */
const GENERATION_TIMEOUT_MAX_MS = 900_000;

/** 计算某次生成的超时：基础值 + 每 hunk 加成，封顶 GENERATION_TIMEOUT_MAX_MS。 */
const generationTimeoutMs = (hunkCount) => Math.min(
  GENERATION_TIMEOUT_MAX_MS,
  GENERATION_TIMEOUT_BASE_MS + Math.max(0, hunkCount) * GENERATION_TIMEOUT_PER_HUNK_MS,
);
// A full walkthrough is a few thousand tokens of JSON. The budget exists for
// what comes before it: reasoning models spend the same allowance thinking and
// return nothing when it runs out, which is a bill for no answer.
//
// So the ask is derived from the model rather than fixed. A flat 24k was the
// same number for a 64k-context model and for one that admits to 384k output
// tokens, and on the latter it was the only reason generation failed.
//
// The reserve subtracted from the input budget is the same number, always: ask
// for more than was reserved and a large diff overruns the context mid-answer,
// which surfaces as a truncation bug rather than a budgeting one.
/** 输出 token 预算下限（历史上固定的请求值），任何模型都至少请求这么多。 */
const MIN_OUTPUT_TOKENS = 24_000;
// A ceiling, because the reserve is taken out of the input allowance: a model
// that would let us ask for 384k tokens of answer would also let us spend a
// third of a million tokens of context reserving them, and no walkthrough needs
// that much thinking.
/** 输出 token 预算上限：预留额要从输入预算里扣，上不封顶会白白占掉大量上下文。 */
const MAX_OUTPUT_TOKENS = 96_000;
// Above this share of the context, the reserve starts costing more diff than
// the extra room is worth.
/** 输出预留最多占模型上下文的比例，再高就开始挤占能送入的 diff。 */
const OUTPUT_CONTEXT_SHARE = 0.25;

/**
 * Answer allowance for a specific model: as much as it admits it can emit,
 * bounded by a share of its context and never below what this feature always
 * asked for.
 */
/**
 * 为具体模型计算回答的输出 token 预算：先取"上下文的固定比例"并夹在
 * MIN/MAX 之间，再被模型自身声明的输出上限压低。该值同时作为输入预算的
 * 预留数额（经 describeSmallModel 的 outputReserveTokens 回调传入），保证
 * 预留与请求永远一致。
 */
const walkthroughOutputTokens = ({ contextTokens, outputTokenLimit }) => {
  const wanted = Math.min(
    MAX_OUTPUT_TOKENS,
    Math.max(MIN_OUTPUT_TOKENS, Math.floor((Number(contextTokens) || 0) * OUTPUT_CONTEXT_SHARE)),
  );
  // A model whose own limit is below the floor gets its limit: asking for more
  // than a provider allows is rejected outright by some and ignored by others.
  return Number(outputTokenLimit) > 0 ? Math.min(wanted, Number(outputTokenLimit)) : wanted;
};

/** 构造带 statusCode 与附加字段（如 code、model）的 Error，供路由层映射 HTTP 状态。 */
const fail = (message, statusCode, extra = {}) =>
  Object.assign(new Error(message), { statusCode, ...extra });

// Generation outlives the request that started it.
//
// A dropped connection and a deliberate cancel look identical at the socket, so
// tying the work to the request lifetime meant an accidental refresh threw away
// a minute of paid-for work. Jobs are keyed by repository + source, so a client
// that comes back attaches to the running job instead of starting a second one,
// and cancelling is an explicit request rather than a side effect of leaving.
/** 运行中的生成任务表：jobKey（仓库+来源）-> { controller, promise, stage }，与请求生命周期解耦。 */
const jobs = new Map();

// Providers that answered a schema request with a 4xx. Retrying the schema on
// every generation means paying for a call we already know will fail, so the
// refusal is remembered and the fallback goes first next time.
//
// Process-lifetime only, on purpose: a provider that gains structured-output
// support should not need a settings change to be tried again — a restart is
// enough, and the cost of one wasted first attempt after that is small.
/** 已拒绝 schema 请求的 provider 集合（仅进程级记忆）：命中则直接走 prompt 内嵌 JSON 形状的降级路径。 */
const schemaRefusedBy = new Set();

/** 模型的稳定键：providerID/modelID。 */
const modelKey = (model) => `${model.providerID}/${model.modelID}`;

/** 任务表键：仓库根目录与 sourceKey 以 NUL 分隔拼接（两个成分都不会包含 NUL）。 */
const jobKey = (repoRoot, sourceKeyValue) => `${repoRoot}\0${sourceKeyValue}`;

/**
 * Coarse stages, reported so a long wait is legible.
 *
 * Only phases a person can actually wait on are named. Building the digest and
 * reading the cache take single-digit milliseconds; giving them their own rows
 * would imply progress where there is none. `retrying` appears only when a
 * provider rejects the schema and the prompt-side fallback runs.
 */
/**
 * 更新运行中任务的粗粒度阶段（collecting/asking/retrying/assembling），供
 * 前端轮询展示等待进度；任务不存在时静默忽略。
 */
const setStage = (repoRoot, sourceKeyValue, stage) => {
  const job = jobs.get(jobKey(repoRoot, sourceKeyValue));
  if (job) job.stage = stage;
};

/**
 * Current stage of a running generation, or `null` when nothing is running.
 * Reads memory only — no git, no network — so it is cheap to poll.
 */
/** 读取某来源当前生成阶段，无运行任务返回 null；只查内存，无 git/网络开销。 */
export function getGenerationStage(repoRoot, sourceKeyValue) {
  return jobs.get(jobKey(repoRoot, sourceKeyValue))?.stage ?? null;
}

/**
 * Whether a generation is currently running for a source. Lets a reconnecting
 * client show progress instead of an empty panel.
 */
/** 某来源是否有生成正在进行，供重连的客户端显示进度而非空白面板。 */
export function isGenerating(repoRoot, sourceKeyValue) {
  return jobs.has(jobKey(repoRoot, sourceKeyValue));
}

/**
 * Resolve the pair the job registry is keyed by, for callers that need to look
 * a job up without doing any diff work.
 */
/** 解析任务表使用的键对（仓库根 + sourceKey），供不做 diff 的调用方查询任务。 */
export async function getRepositoryRootFor(directory, rawSource) {
  const source = parseSource(rawSource);
  return { repoRoot: await getRepositoryRoot(directory), sourceKey: sourceKey(source) };
}

/**
 * Stop a running generation. Only an explicit request does this — leaving the
 * page does not.
 */
/** 显式取消某来源的运行中生成（abort 其 controller）；无任务返回 cancelled: false。离开页面不会触发取消。 */
export async function cancelWalkthroughGeneration({ directory, source: rawSource }) {
  const source = parseSource(rawSource);
  const repoRoot = await getRepositoryRoot(directory);
  const job = jobs.get(jobKey(repoRoot, sourceKey(source)));
  if (!job) return { cancelled: false };
  job.controller.abort();
  return { cancelled: true };
}

/** 模型的展示标签：providerID/modelID，用于错误消息文案。 */
const modelLabel = (model) => `${model.providerID}/${model.modelID}`;

/**
 * Resolve the model for this feature: the walkthrough override when set,
 * otherwise whatever the small-model chain resolves to.
 */
/**
 * Resolve the model for this feature. An explicit per-review choice outranks the
 * saved setting, which in turn outranks the small-model chain — the user picking
 * a roomier model for a risky change is the most specific intent there is.
 */
/**
 * 解析本功能使用的模型：显式选择的模型 > 已保存的 walkthrough 覆盖设置 >
 * small-model 链默认值。同时把 walkthroughOutputTokens 作为输出预留回调传入，
 * 使"预留多少"与"请求多少"永远一致。
 */
const resolveModel = (directory, explicitModel) => describeSmallModel({
  directory,
  outputReserveTokens: walkthroughOutputTokens,
  overrideModel: explicitModel || readWalkthroughModelOverride(),
});

/** 测试导出：暴露超时与输出预算两个纯函数供单元测试直接调用。 */
export const __testing = { generationTimeoutMs, walkthroughOutputTokens };

/**
 * Current diff for a source, parsed into files and hunks.
 */
/** 读取某来源当前 diff 并构建 digest（文件、hunk、计数），读取与生成共用的入口。 */
async function loadCurrentDiff(directory, source, deps) {
  const { sections } = await loadSourceSections(directory, source, deps);
  const built = buildDigest(sections);
  return built;
}

/** 收集 walkthrough 全部停靠点引用的 hunk id（顺序无关，仅用于构造覆盖集合）。 */
const stopHunkIds = (walkthrough) =>
  walkthrough.chapters.flatMap((chapter) => chapter.stops.flatMap((stop) => stop.hunkIds));

/**
 * Compare a stored walkthrough against the diff as it is right now.
 *
 * Staleness is not a heuristic here: a hunk id is a hash of the hunk's content,
 * so an anchor that no longer resolves is proof that the code it described has
 * changed or gone. Anchors that still resolve are still accurate.
 */
/**
 * 把已存的 walkthrough 与当前 diff 对账：hunk id 是内容哈希，锚点失配即证明
 * 所描述的代码已变。返回是否过期（isStale）、缺失的 hunk id、过期的停靠点 id
 * 以及当前 diff 中未被任何停靠点覆盖的 hunk id。
 */
function resolveAgainstCurrent(walkthrough, hunkIndex) {
  const missingHunkIds = [];
  const staleStopIds = [];

  for (const chapter of walkthrough.chapters) {
    for (const stop of chapter.stops) {
      const missing = stop.hunkIds.filter((id) => !hunkIndex.has(id));
      if (missing.length === 0) continue;
      missingHunkIds.push(...missing);
      staleStopIds.push(stop.id);
    }
  }

  const covered = new Set(stopHunkIds(walkthrough));
  const uncoveredHunkIds = [...hunkIndex.keys()].filter((id) => !covered.has(id));

  return {
    isStale: missingHunkIds.length > 0,
    missingHunkIds,
    staleStopIds,
    uncoveredHunkIds,
  };
}

/** 把解析出的 files 拍平为 hunk 视图数组（含路径、状态、头信息、patch 正文），返回给前端。 */
const serializeHunks = (files) => files.flatMap((file) => file.hunks.map((hunk) => ({
  id: hunk.id,
  path: file.path,
  oldPath: file.oldPath || null,
  status: file.status,
  scope: file.scope,
  header: hunk.header,
  newStart: hunk.newStart,
  added: hunk.added,
  deleted: hunk.deleted,
  patch: hunk.patch,
})));

/**
 * Read the last walkthrough for a source, resolved against the current diff.
 * Never generates and never spends tokens.
 */
/**
 * 读取某来源最近一次 walkthrough 并对账当前 diff；从不生成、不花 token。优先
 * 返回"本次请求的模型+语言"对应的缓存（切换语言后面板不再停留旧语言），命中
 * 不同缓存时顺带更新指针。返回体含 hunks、readiness、generating 与过期信息。
 */
export async function getWalkthrough({ directory, source: rawSource, model: explicitModel, language: rawLanguage }, deps = {}) {
  const source = parseSource(rawSource);
  const repoRoot = await getRepositoryRoot(directory);
  const key = sourceKey(source);
  const language = normalizeLanguage(rawLanguage);

  const pointer = readPointer(repoRoot, key);
  // One diff, one model lookup, both answers. These used to be separate
  // endpoints the client called in parallel, which meant every panel open ran
  // the whole git pipeline twice.
  const [built, model] = await Promise.all([
    loadCurrentDiff(directory, source, deps),
    resolveModel(directory, explicitModel).catch(() => null),
  ]);
  const { files } = built;
  const hunkIndex = indexHunks(files);
  const readiness = computeReadiness({ ...built, model, source, language });

  const base = {
    source,
    hunks: serializeHunks(files),
    hunkCount: hunkIndex.size,
    readiness,
    generating: isGenerating(repoRoot, key),
  };

  // Ask the cache for *this* request before falling back to the pointer.
  //
  // The pointer only knows which walkthrough was generated here last, which
  // after a model or language switch is the answer to a different question:
  // the panel would keep showing the English review while the picker said
  // Ukrainian, even though the Ukrainian one was sitting in the cache. The key
  // is computed from the diff this read already parsed, so this costs a file
  // read and no git work at all.
  const requestedKey = model
    ? buildCacheKey({
      repoRoot,
      sourceKey: key,
      providerID: model.providerID,
      modelID: model.modelID,
      language,
      files,
    })
    : null;

  const requested = requestedKey ? readCachedWalkthrough(requestedKey) : null;
  const entry = requested ?? (pointer ? readCachedWalkthrough(pointer.cacheKey) : null);
  if (!entry) {
    // No pointer, or the pointer outlived its entry (eviction, manual cleanup).
    // "No walkthrough" is the truthful answer either way; the pointer is left
    // for the next generation to overwrite.
    return { ...base, walkthrough: null };
  }

  // Showing it makes it the last walkthrough shown here, and a regeneration
  // re-authors from whatever the reader is actually looking at. Only written
  // when it moved, so an unchanged read stays a pure read.
  if (requested && pointer?.cacheKey !== requestedKey) {
    writePointer(repoRoot, key, {
      repoRoot,
      sourceKey: key,
      cacheKey: requestedKey,
      generatedAt: requested.generatedAt,
    });
  }

  return {
    ...base,
    walkthrough: entry.walkthrough,
    model: entry.model,
    // The language the text on screen is actually written in, which is not
    // necessarily the one being asked for now. The picker needs the difference:
    // it is what lets it default to what produced this rather than to a setting.
    language: entry.language ?? null,
    generatedAt: entry.generatedAt,
    ...resolveAgainstCurrent(entry.walkthrough, hunkIndex),
  };
}

/**
 * Whether the resolved model can do this job, computed from a digest the caller
 * already built.
 *
 * Folded into the walkthrough read rather than living on its own endpoint: both
 * answers need the same diff, and computing it twice doubled the git work on
 * every panel open.
 */
/**
 * 判断解析出的模型能否胜任本次生成，失败时返回带可操作 reason 的结果：
 * no-model / empty-diff / only-generated / no-provider-login /
 * structured-output-unsupported / context-too-small。提示词长度用与真正生成
 * 相同的语言计算，避免测量一个不会发出的请求。
 */
function computeReadiness({ model, digest, files, fileCount, hunkCount, generatedFileCount, source, language }) {
  if (!model) return { ready: false, reason: 'no-model' };

  if (hunkCount === 0) {
    // "Only a lockfile changed" is a different answer from "nothing changed",
    // and the user can act on it (commit and move on) rather than wonder why
    // the review refuses.
    const reason = files.length > 0 && generatedFileCount === files.length ? 'only-generated' : 'empty-diff';
    return { ready: false, reason, model, generatedFileCount };
  }

  // A resolved override/config model can still have no usable login. Refuse up
  // front and omit the model — offering an unauthenticated selection in the
  // picker is what made the old raw auth error feel like a product bug.
  if (model.hasLogin === false) {
    return { ready: false, reason: 'no-provider-login' };
  }

  // Built with the same language the generation would use: the instruction is
  // part of the prompt, so a readiness answer computed without it would be
  // measuring a request nobody is going to send.
  const { prompt, system } = buildPrompt({ digest, fileCount, hunkCount, source, language });
  const requiredChars = prompt.length + system.length;

  if (model.structuredOutput === false) {
    return { ready: false, reason: 'structured-output-unsupported', model, requiredChars };
  }

  if (requiredChars > model.inputCharBudget) {
    return {
      ready: false,
      reason: 'context-too-small',
      model,
      requiredChars,
      availableChars: model.inputCharBudget,
    };
  }

  return { ready: true, model, requiredChars, availableChars: model.inputCharBudget, hunkCount, fileCount };
}

/**
 * Generate a walkthrough for a source.
 *
 * Returns the cached entry when the diff, model, and prompt are all unchanged —
 * which also means returning to a previous state of the working tree costs
 * nothing.
 */
/**
 * 生成（或复用）某来源的 walkthrough：同键任务进行中直接复用其 promise，避免
 * 重复计费；未强制（force=false）且缓存命中时返回缓存并更新指针；否则启动
 * runGeneration，任务登记进 jobs 表，结束（无论成败）后自动摘除。
 */
export async function generateWalkthrough({ directory, source: rawSource, force = false, model: explicitModel, language: rawLanguage }, deps = {}) {
  const source = parseSource(rawSource);
  const repoRoot = await getRepositoryRoot(directory);
  const key = sourceKey(source);
  const language = normalizeLanguage(rawLanguage);

  // Attach to a running job rather than starting a second one. A user who
  // refreshed and pressed the button again wants the answer, not two bills.
  const existing = jobs.get(jobKey(repoRoot, key));
  if (existing) return existing.promise;

  const controller = new AbortController();
  const promise = runGeneration({ directory, source, repoRoot, key, force, explicitModel, language, signal: controller.signal }, deps)
    .finally(() => {
      if (jobs.get(jobKey(repoRoot, key))?.controller === controller) {
        jobs.delete(jobKey(repoRoot, key));
      }
    });

  jobs.set(jobKey(repoRoot, key), { controller, promise, stage: 'collecting' });
  return promise;
}

/**
 * 生成的实际执行体：解析模型（含登录与结构化输出校验）-> 构建 digest -> 计算
 * 缓存键 -> 非强制时查缓存 -> 组装提示词（可携带上一版叙事）-> 先带 responseSchema
 * 请求，被 provider 以 4xx 拒绝则记入 schemaRefusedBy 并改用 prompt 内嵌 JSON
 * 形状重试 -> 解析并归一化响应 -> 写缓存与指针。各类失败映射为带 statusCode 与
 * code 的错误；缓存写入失败不会让已产出好结果的请求失败。
 */
async function runGeneration({ directory, source, repoRoot, key, force, explicitModel, language, signal }, deps) {

  const model = await resolveModel(directory, explicitModel);
  if (!model) {
    throw fail('No model is available — sign in to a provider first', 404, { code: 'no-model' });
  }
  if (model.hasLogin === false) {
    throw fail(
      `No OpenCode login found for provider "${model.providerID}" — sign in or choose a different model`,
      401,
      { code: 'no-provider-login', model },
    );
  }

  const { digest, files, idByAlias, fileCount, hunkCount, generatedFileCount } = await loadCurrentDiff(directory, source, deps);
  setStage(repoRoot, key, 'asking');
  if (hunkCount === 0) {
    if (files.length > 0 && generatedFileCount === files.length) {
      throw fail('Only generated files changed — there is nothing to review', 400, { code: 'only-generated' });
    }
    throw fail('There are no changes to review', 400, { code: 'empty-diff' });
  }

  const cacheKey = buildCacheKey({
    repoRoot,
    sourceKey: key,
    providerID: model.providerID,
    modelID: model.modelID,
    language,
    files,
  });

  const hunkIndex = indexHunks(files);

  if (!force) {
    const cached = readCachedWalkthrough(cacheKey);
    if (cached) {
      writePointer(repoRoot, key, {
        repoRoot,
        sourceKey: key,
        cacheKey,
        generatedAt: cached.generatedAt,
      });
      return {
        source,
        walkthrough: cached.walkthrough,
        model: cached.model,
        language: cached.language ?? null,
        generatedAt: cached.generatedAt,
        fromCache: true,
        hunks: serializeHunks(files),
        hunkCount,
        ...resolveAgainstCurrent(cached.walkthrough, hunkIndex),
      };
    }
  }

  // A forced regeneration hands the model its own previous narrative so it can
  // keep what is still true instead of starting from a blank page. The old
  // anchors are deliberately not included — they belong to code that has moved.
  let previousWalkthrough = null;
  const pointer = readPointer(repoRoot, key);
  if (pointer) {
    const previousEntry = readCachedWalkthrough(pointer.cacheKey);
    if (previousEntry && previousEntry.cacheKey !== cacheKey) {
      previousWalkthrough = previousEntry.walkthrough;
    }
  }

  const { prompt, system } = buildPrompt({ digest, fileCount, hunkCount, source, previousWalkthrough, language });

  if (model.structuredOutput === false) {
    throw fail(
      `${modelLabel(model)} cannot produce structured output — choose a different small model`,
      409,
      { code: 'structured-output-unsupported', model },
    );
  }

  const run = (options) => generateSmallModelText({
    prompt: options.prompt,
    system: options.system,
    directory,
    model: `${model.providerID}/${model.modelID}`,
    responseSchema: options.responseSchema,
    onOverflow: 'error',
    timeoutMs: generationTimeoutMs(hunkCount),
    // The number the input budget was already reduced by, not a fresh guess.
    maxOutputTokens: model.outputTokens ?? MIN_OUTPUT_TOKENS,
    signal,
  });

  // Roughly half the catalog does not declare `structured_output`, and some of
  // those providers reject the schema outright. A rejected request shape is not
  // a dead end: the shape can travel in the prompt instead, and the response
  // parser is already tolerant of imperfect JSON.
  const withSchema = () => run({ prompt, system, responseSchema });
  const withoutSchema = () => run({
    prompt,
    system: `${system}\n${JSON_SHAPE_INSTRUCTION}`,
    responseSchema: undefined,
  });

  const asRequestFailure = (error) => {
    if (error?.code === 'context-too-small') {
      return fail(error.message, 409, {
        code: 'context-too-small',
        model,
        requiredChars: error.requiredChars,
        availableChars: error.availableChars,
      });
    }
    if (error?.code === 'output-exhausted') {
      return fail(error.message, 409, { code: 'output-exhausted', model });
    }
    if (error?.code === 'no-provider-login') {
      return fail(error.message, 401, { code: 'no-provider-login', model });
    }
    return null;
  };

  const refusesSchema = (error) => error?.code === 'structured-output-unsupported'
    || (Number(error?.status) >= 400 && Number(error?.status) < 500);

  let raw;
  let usedSchema = false;

  if (schemaRefusedBy.has(modelKey(model))) {
    // Already known to refuse: skip straight to the fallback rather than pay
    // for a call whose failure is a foregone conclusion.
    setStage(repoRoot, key, 'retrying');
    try {
      raw = await withoutSchema();
    } catch (error) {
      throw asRequestFailure(error) ?? error;
    }
  } else {
    try {
      raw = await withSchema();
      usedSchema = true;
    } catch (error) {
      const failure = asRequestFailure(error);
      if (failure) throw failure;
      if (!refusesSchema(error)) throw error;

      schemaRefusedBy.add(modelKey(model));
      setStage(repoRoot, key, 'retrying');
      try {
        raw = await withoutSchema();
      } catch (fallbackError) {
        throw asRequestFailure(fallbackError) ?? fallbackError;
      }
    }
  }

  setStage(repoRoot, key, 'assembling');

  let walkthrough;
  try {
    walkthrough = normalizeWalkthrough(parseModelJson(raw.text), idByAlias);
  } catch (error) {
    // Without schema support the model was asked for JSON in prose and did not
    // deliver: that is a capability problem the user can fix by switching model,
    // so it gets the picker rather than a parser error.
    if (!usedSchema) {
      throw fail(
        `${modelLabel(model)} could not return the structured response a walkthrough needs`,
        409,
        { code: 'structured-output-unsupported', model },
      );
    }
    throw fail(
      `${modelLabel(model)} did not return a usable walkthrough — try a different small model`,
      502,
      { code: 'invalid-walkthrough', model, cause: error?.message },
    );
  }

  const generatedAt = new Date().toISOString();
  const entry = {
    cacheKey,
    generatedAt,
    repoRoot,
    sourceKey: key,
    model: { providerID: model.providerID, modelID: model.modelID, source: model.source },
    language,
    walkthrough,
  };

  // A failed write costs a regeneration next time; it must never fail the
  // request that already produced a good walkthrough.
  writeCachedWalkthrough(cacheKey, entry);
  writePointer(repoRoot, key, { repoRoot, sourceKey: key, cacheKey, generatedAt });

  return {
    source,
    walkthrough,
    model: entry.model,
    language,
    generatedAt,
    fromCache: false,
    hunks: serializeHunks(files),
    hunkCount,
    ...resolveAgainstCurrent(walkthrough, hunkIndex),
  };
}

// 转导出来源解析错误类型，路由层用它区分"来源不合法"之类的用户错误。
export { WalkthroughSourceError };
