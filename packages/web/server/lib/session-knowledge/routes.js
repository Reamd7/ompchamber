/**
 * What a session still owes in project knowledge, and the record that it was
 * delivered.
 *
 * Two calls rather than one, because only the sender knows whether the message
 * carrying the block actually went out. Handing over the text and recording it
 * as delivered in the same request would leave a failed send believing the
 * agent has context it never received.
 *
 * The body parser is attached per route: there is no global one, because the
 * generic OpenCode proxy needs an unread request stream.
 */

/**
 * 会话项目知识 HTTP 路由：查询待下发的知识文本、面板摘要、置顶管理、
 * 以及「已送达」回执。
 *
 * 刻意拆成「取文本」与「回报已送达」两个请求：只有发送方知道携带知识块的
 * 消息是否真的发出去了。JSON body parser 按路由单独挂载而不设全局的，
 * 因为全局 parser 会消费掉 OpenCode 通用 proxy 依赖的未读请求流。
 */
import express from 'express';

/** 按路由挂载的 express JSON body parser（1mb 上限），不注册为全局中间件。 */
const parseJsonBody = express.json({ limit: '1mb' });

/** 判断值是否为普通对象（非 null、非数组），用于校验请求体。 */
const isRecord = (value) => Boolean(value) && typeof value === 'object' && !Array.isArray(value);

/** 字符串 trim 后非空则返回 trim 结果，否则返回空串，用于宽松取参。 */
const asNonEmptyString = (value) => (
  typeof value === 'string' && value.trim().length > 0 ? value.trim() : ''
);

/**
 * 在 express app 上注册 /api/session-knowledge 系列路由。
 * @param {object} app express 应用实例
 * @param {object} dependencies 须包含 sessionKnowledgeRuntime（见 runtime.js）
 */
export const registerSessionKnowledgeRoutes = (app, dependencies) => {
  const { sessionKnowledgeRuntime } = dependencies;

  /**
   * Answers with the text to attach and the signature to report back once it
   * has gone. An empty text means the session is already carrying it.
   */
  /** GET /api/session-knowledge：返回待附带的文本与签名；text 为空表示会话已携带。 */
  app.get('/api/session-knowledge', async (req, res) => {
    const directory = asNonEmptyString(req.query.directory);
    const sessionId = asNonEmptyString(req.query.sessionId);
    if (!directory) {
      return res.status(400).json({ error: 'directory is required' });
    }

    try {
      const pending = sessionId
        ? await sessionKnowledgeRuntime.resolvePendingForSession(sessionId, directory)
        // A session that does not exist yet — a draft about to be created —
        // has been told nothing, so everything is still owed.
        : await sessionKnowledgeRuntime.resolvePending(directory, '');
      return res.json(pending);
    } catch (error) {
      // Never fails the caller's send: a message without its background is far
      // better than no message at all.
      return res.json({ text: '', signature: '', unavailable: true, reason: error?.message ?? 'unknown' });
    }
  });

  /** Counts and names for the work status panel; assembles no text. */
  /** GET /api/session-knowledge/summary：工作状态面板的计数与名称，不组装文本。 */
  app.get('/api/session-knowledge/summary', async (req, res) => {
    const directory = asNonEmptyString(req.query.directory);
    if (!directory) {
      return res.json({ notes: [], plans: [], memory: { global: 0, project: 0 } });
    }
    const sessionId = asNonEmptyString(req.query.sessionId);
    try {
      return res.json(sessionId
        ? await sessionKnowledgeRuntime.collectSummaryForSession(sessionId, directory)
        : await sessionKnowledgeRuntime.collectSummary(directory));
    } catch {
      // A panel that cannot read this shows nothing rather than an error.
      return res.json({ notes: [], plans: [], memory: { global: 0, project: 0 } });
    }
  });

  /**
   * POST /api/session-knowledge/pin：置顶或取消置顶某条 note/plan。
   * 校验 sessionId、directory、kind、id、pinned，缺一即返回 400。
   */
  app.post('/api/session-knowledge/pin', parseJsonBody, async (req, res) => {
    const body = req.body;
    if (!isRecord(body)) return res.status(400).json({ error: 'Body must be an object' });
    const sessionId = asNonEmptyString(body.sessionId);
    const directory = asNonEmptyString(body.directory);
    const id = asNonEmptyString(body.id);
    const kind = body.kind === 'note' || body.kind === 'plan' ? body.kind : '';
    if (!sessionId || !directory || !id || !kind || typeof body.pinned !== 'boolean') {
      return res.status(400).json({ error: 'sessionId, directory, kind, id and pinned are required' });
    }
    try {
      return res.json({ pins: await sessionKnowledgeRuntime.setPin(sessionId, directory, kind, id, body.pinned) });
    } catch (error) {
      return res.status(500).json({ error: error?.message ?? 'Unable to update pin' });
    }
  });

  /**
   * POST /api/session-knowledge/delivered：消息发出后回传签名以记录已送达。
   * 记录失败仅返回 recorded: false —— 消息已发出，代价至多是多发一次知识块。
   */
  app.post('/api/session-knowledge/delivered', parseJsonBody, async (req, res) => {
    const body = req.body;
    if (!isRecord(body)) {
      return res.status(400).json({ error: 'Body must be an object' });
    }
    const sessionId = asNonEmptyString(body.sessionId);
    const directory = asNonEmptyString(body.directory);
    const signature = asNonEmptyString(body.signature);
    if (!sessionId || !directory || !signature) {
      return res.status(400).json({ error: 'sessionId, directory and signature are required' });
    }

    try {
      await sessionKnowledgeRuntime.recordDelivered(sessionId, directory, signature);
      return res.json({ recorded: true });
    } catch (error) {
      // The message is already sent; failing here only means the block may be
      // sent once more, which is far better than reporting the send as failed.
      return res.json({ recorded: false, reason: error?.message ?? 'unknown' });
    }
  });
};
