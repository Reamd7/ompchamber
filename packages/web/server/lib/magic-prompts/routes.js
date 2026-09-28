/**
 * Magic prompts 的 Express 路由注册模块。
 *
 * 在 <数据目录>/magic-prompts.json 上构建提示词覆盖运行时，并暴露
 * /api/magic-prompts 的 GET/PUT/DELETE（含 /:id）端点：读取全部覆盖、设置单条、
 * 重置单条与全部重置；输入类错误映射 400，其余异常映射 500。
 */
import { createMagicPromptRuntime } from './runtime.js';

/**
 * 注册 magic prompts 相关路由：以注入的 fsPromises/path 与数据目录构造运行时，
 * 状态文件固定为 <数据目录>/magic-prompts.json。
 * @param {import('express').Express} app Express 应用实例
 * @param {{ fsPromises: object, path: object, ompchamberDataDir: string }} dependencies 注入的依赖
 */
export const registerMagicPromptRoutes = (app, dependencies) => {
  const {
    fsPromises,
    path,
    ompchamberDataDir,
  } = dependencies;

  // 绑定到 magic-prompts.json 的运行时实例。
  const runtime = createMagicPromptRuntime({
    fsPromises,
    path,
    filePath: path.join(ompchamberDataDir, 'magic-prompts.json'),
  });

  // 读取全部提示词覆盖状态；读取失败返回 500。
  app.get('/api/magic-prompts', async (_req, res) => {
    try {
      const state = await runtime.readPromptState();
      res.json(state);
    } catch (error) {
      res.status(500).json({ error: error instanceof Error ? error.message : 'Failed to read magic prompts' });
    }
  });

  // 设置单条覆盖：body.text 必填（缺失返回 400）；id 非法、文本过长或为空同样返回 400。
  app.put('/api/magic-prompts/:id', async (req, res) => {
    const id = typeof req.params?.id === 'string' ? req.params.id : '';
    const text = typeof req.body?.text === 'string' ? req.body.text : null;
    if (text === null) {
      return res.status(400).json({ error: 'text is required' });
    }

    try {
      const state = await runtime.setOverride(id, text);
      return res.json(state);
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error);
      const status = message.includes('Invalid prompt id') || message.includes('too long') || message.includes('cannot be empty') ? 400 : 500;
      return res.status(status).json({ error: message });
    }
  });

  // 重置单条覆盖：id 非法返回 400，成功返回最新状态。
  app.delete('/api/magic-prompts/:id', async (req, res) => {
    const id = typeof req.params?.id === 'string' ? req.params.id : '';
    try {
      const state = await runtime.resetOverride(id);
      return res.json(state);
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error);
      const status = message.includes('Invalid prompt id') ? 400 : 500;
      return res.status(status).json({ error: message });
    }
  });

  // 重置全部覆盖；失败返回 500。
  app.delete('/api/magic-prompts', async (_req, res) => {
    try {
      const state = await runtime.resetAllOverrides();
      return res.json(state);
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error);
      return res.status(500).json({ error: message || 'Failed to reset magic prompts' });
    }
  });
};
