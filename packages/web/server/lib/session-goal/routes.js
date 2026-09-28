/**
 * 文件形态目标 objective 的 HTTP 路由（OMPChamber 自有端点，按 session id
 * 寻址；一个会话至多一个目标，新目标覆盖旧文件）：PUT 写入、GET 读回
 * 展示、DELETE 随目标移除清理。
 */
import { deleteObjective, readObjective, writeObjective } from './objectives.js';

// OMPChamber-owned routes for file-backed goal objectives, keyed by session
// id (one goal per session; a new goal overwrites the old file). The UI
// writes the objective file before stamping the goal metadata (which only
// carries an `objectiveFile: true` flag), reads it back for display, and
// deletes it when the goal is removed.
/**
 * 中文补充：注册上述三个 objective 端点。PUT 的错误按 statusCode 透传
 * （writeObjective 对非法 session id / 空内容抛 400）；GET 未命中返回
 * 404；DELETE 幂等，文件不存在也返回 ok。
 *
 * @param {object} app express 应用
 */
export function registerSessionGoalRoutes(app) {
  // 写入目标文件（body.content）；非法 session id 或空内容返回 400。
  app.put('/api/goals/objective/:sessionId', async (req, res) => {
    try {
      const { content } = req.body || {};
      await writeObjective(req.params.sessionId, content);
      res.json({ ok: true });
    } catch (error) {
      const statusCode = Number(error?.statusCode) || 500;
      if (statusCode >= 500) {
        console.error('Failed to write goal objective:', error);
      }
      res.status(statusCode).json({ error: error?.message || 'Failed to write goal objective' });
    }
  });

  // 读回目标文本供展示；文件不存在（或 id 非法）返回 404。
  app.get('/api/goals/objective/:sessionId', async (req, res) => {
    const content = await readObjective(req.params.sessionId);
    if (content === null) {
      res.status(404).json({ error: 'objective not found' });
      return;
    }
    res.json({ content });
  });

  // 删除目标文件；幂等，不存在也返回 ok。
  app.delete('/api/goals/objective/:sessionId', async (req, res) => {
    await deleteObjective(req.params.sessionId);
    res.json({ ok: true });
  });
}
