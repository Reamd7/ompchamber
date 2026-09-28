/**
 * Small Model 的 HTTP 路由层：在 express 应用上挂载 /api/small-model
 * （模型查询）与 /api/small-model/generate（文本生成）两个端点，把 HTTP
 * 参数透传给 small-model 服务层（index.js），并把服务层抛出的错误转换
 * 为面向用户的 HTTP 响应。
 */
/**
 * 注册 small-model 相关路由。
 * @param {object} app express 应用实例。
 * @param {object} options 路由依赖。
 * @param {() => Promise<object>} options.getSmallModelService 惰性获取服务模块
 *   （describeSmallModel / listAuthenticatedProviders / generateSmallModelText），
 *   延迟加载以避免路由注册阶段就拉起整条依赖链。
 */
export function registerSmallModelRoutes(app, { getSmallModelService }) {
// GET /api/small-model：报告当前会解析到哪个小模型及其能力（输入预算、
// structured output 支持等），附可用的已认证 provider 列表，供前端模型
// 选择器使用；解析失败时记录日志并返回 500。
  app.get('/api/small-model', async (req, res) => {
    try {
      const { describeSmallModel, listAuthenticatedProviders } = await getSmallModelService();
      const resolved = await describeSmallModel({
        directory: typeof req.query.directory === 'string' ? req.query.directory : undefined,
        preferredProviderID: typeof req.query.providerID === 'string' ? req.query.providerID : undefined,
        preferredModelID: typeof req.query.modelID === 'string' ? req.query.modelID : undefined,
      });
      res.json({
        available: Boolean(resolved),
        model: resolved,
        authenticatedProviders: await listAuthenticatedProviders(),
      });
    } catch (error) {
      console.error('Failed to resolve small model:', error);
      res.status(500).json({ error: error.message || 'Failed to resolve small model' });
    }
  });

// POST /api/small-model/generate：用小模型生成文本。错误按 error.statusCode
// 透传（缺省 500）：404 保留原始消息，其余状态返回引导用户换模型的通用
// 文案并附 error.code 供前端分支处理；5xx 额外记录服务端日志。
  app.post('/api/small-model/generate', async (req, res) => {
    try {
      const { generateSmallModelText } = await getSmallModelService();
      const { prompt, system, maxOutputTokens, model, directory, preferredProviderID, preferredModelID, restrictToPreferredProvider } = req.body || {};
      const result = await generateSmallModelText({
        prompt,
        system,
        maxOutputTokens,
        model,
        directory,
        preferredProviderID,
        preferredModelID,
        restrictToPreferredProvider: restrictToPreferredProvider === true,
      });
      res.json(result);
    } catch (error) {
      const statusCode = Number(error?.statusCode) || 500;
      if (statusCode >= 500) {
        console.error('Small model generation failed:', error);
      }
      res.status(statusCode).json({
        error: statusCode === 404
          ? (error.message || 'No small model is available')
          : 'The selected Small Model could not complete this action. Choose another model in Settings → Sessions → Small Model and try again.',
        ...(error?.code ? { code: error.code } : {}),
      });
    }
  });
}
