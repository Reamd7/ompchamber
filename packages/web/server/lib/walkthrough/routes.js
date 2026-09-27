// `req.destroyed` is true for every healthy request once the body parser has
// consumed the stream, so using it as a disconnect check silently swallows every
// response. The response socket is the one that actually reflects whether the
// client is still there.
/**
 * walkthrough（代码变更导览）功能的 HTTP 路由模块。
 *
 * 在 express 应用上注册 /api/walkthrough 系列端点（读取、生成、进度、取消），
 * 全部委托给 getWalkthroughService() 惰性解析出的服务实现；本模块只负责
 * query/body 参数归一化，以及把服务错误（statusCode/code 等字段）转换成
 * 统一的 JSON 错误响应。
 */
/**
 * 判断请求客户端是否已经离开：响应已写完（writableEnded）或 socket 已销毁。
 * 生成接口耗时可达数分钟，在生成结束、回写 JSON 前用它探测客户端是否还在，
 * 避免向已断开的连接写入。刻意检查 res 而非 req —— 原因见上方英文注释：
 * body parser 消费完请求流后 req.destroyed 恒为 true，会吞掉所有正常响应。
 */
const clientIsGone = (res) => res.writableEnded || res.destroyed;

/**
 * 注册 /api/walkthrough 下的全部路由：GET /（读取缓存或现算的导览）、
 * POST /generate（触发生成）、GET /progress（生成进度轮询）、
 * POST /cancel（取消进行中的生成）。
 *
 * @param app express 应用实例
 * @param options.getWalkthroughService 按请求惰性解析 walkthrough 服务的
 *   异步访问器，使路由注册与服务的初始化时序解耦
 */
export function registerWalkthroughRoutes(app, { getWalkthroughService }) {
  // 统一错误出口：透传 error.statusCode（非法则按 500），5xx 额外打印到
  // console.error 便于排查；响应体按需附带 code、model、以及上下文超限时的
  // requiredChars/availableChars 字段，供前端给出可操作的提示。
  const respondWithError = (res, error, fallback) => {
    const statusCode = Number(error?.statusCode) || 500;
    if (statusCode >= 500) {
      console.error(`${fallback}:`, error);
    }
    res.status(statusCode).json({
      error: error?.message || fallback,
      ...(error?.code ? { code: error.code } : {}),
      ...(error?.model ? { model: error.model } : {}),
      ...(Number.isFinite(error?.requiredChars) ? { requiredChars: error.requiredChars } : {}),
      ...(Number.isFinite(error?.availableChars) ? { availableChars: error.availableChars } : {}),
    });
  };

  // 解析 query 中的 source 参数（JSON 字符串）：非字符串、空串或解析失败
  // 一律返回 null，交由服务层按默认来源（工作区变更）处理。
  const readSource = (value) => {
    if (typeof value !== 'string' || !value) return null;
    try {
      return JSON.parse(value);
    } catch {
      return null;
    }
  };

  // 读取指定目录的 walkthrough（命中缓存则直接返回）。
  // directory 为必填 query（缺失返回 400）；source/model/language 可选。
  app.get('/api/walkthrough', async (req, res) => {
    try {
      const { getWalkthrough, getPullRequestDiff } = await getWalkthroughService();
      const directory = typeof req.query.directory === 'string' ? req.query.directory : '';
      if (!directory) {
        return res.status(400).json({ error: 'directory parameter is required' });
      }

      const result = await getWalkthrough(
        {
          directory,
          source: readSource(req.query.source),
          model: typeof req.query.model === 'string' ? req.query.model : undefined,
          language: typeof req.query.language === 'string' ? req.query.language : undefined,
        },
        { getPullRequestDiff },
      );
      res.json(result);
    } catch (error) {
      respondWithError(res, error, 'Failed to load walkthrough');
    }
  });

  // Deliberately not aborted when the client disconnects: generation runs for
  // minutes and a refresh must not throw the work away. Leaving detaches the
  // client; the job finishes and caches its result. Stopping is an explicit
  // request below.
  // 生成接口刻意不在客户端断开时中止任务：页面刷新不应丢弃已运行数分钟的
  // 生成（任务会继续完成并写入缓存）；显式停止走下方的 cancel 接口。
  app.post('/api/walkthrough/generate', async (req, res) => {
    try {
      const { generateWalkthrough, getPullRequestDiff } = await getWalkthroughService();
      const { directory, source, force, model, language } = req.body || {};
      if (!directory || typeof directory !== 'string') {
        return res.status(400).json({ error: 'directory is required' });
      }

      const result = await generateWalkthrough(
        {
          directory,
          source,
          force: force === true,
          model: typeof model === 'string' ? model : undefined,
          language: typeof language === 'string' ? language : undefined,
        },
        { getPullRequestDiff },
      );
      if (clientIsGone(res)) return;
      res.json(result);
    } catch (error) {
      if (clientIsGone(res)) return;
      respondWithError(res, error, 'Failed to generate walkthrough');
    }
  });

  // Memory-only, so it is safe to poll while a generation runs. The full read
  // re-runs the whole git pipeline and must not be used for this.
  // 进度接口只读内存中的生成阶段，可在生成进行中安全轮询；完整读取接口
  // 会重跑整个 git 管线，绝不能拿来当轮询用。
  app.get('/api/walkthrough/progress', async (req, res) => {
    try {
      const { getGenerationStage, getRepositoryRootFor } = await getWalkthroughService();
      const directory = typeof req.query.directory === 'string' ? req.query.directory : '';
      if (!directory) {
        return res.status(400).json({ error: 'directory parameter is required' });
      }

      const { repoRoot, sourceKey } = await getRepositoryRootFor(directory, readSource(req.query.source));
      res.json({ stage: getGenerationStage(repoRoot, sourceKey) });
    } catch (error) {
      respondWithError(res, error, 'Failed to read walkthrough progress');
    }
  });

  // 取消指定目录（及来源）正在进行的 walkthrough 生成任务。
  app.post('/api/walkthrough/cancel', async (req, res) => {
    try {
      const { cancelWalkthroughGeneration } = await getWalkthroughService();
      const { directory, source } = req.body || {};
      if (!directory || typeof directory !== 'string') {
        return res.status(400).json({ error: 'directory is required' });
      }

      res.json(await cancelWalkthroughGeneration({ directory, source }));
    } catch (error) {
      respondWithError(res, error, 'Failed to cancel walkthrough generation');
    }
  });
}
