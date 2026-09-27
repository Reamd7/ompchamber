/**
 * OMPChamber 控制动作的 HTTP 入口（POST /api/ompchamber/control）。
 *
 * 这是一个薄适配层：解析 { action, input, contextDirectory } 请求体、
 * 在客户端断连时通过 AbortController 取消底层动作、把服务结果原样
 * JSON 返回；错误经 asControlError 归一后按其 statusCode 响应，并透
 * 传 partial 部分结果详情。业务逻辑全部在 service.js。
 */
import express from 'express';
import { asControlError } from './error.js';

/** 在 express app 上注册控制动作路由；controlService 提供 execute(action, input, contextDirectory, options)。 */
export const registerOMPChamberControlRoutes = (app, { controlService }) => {
  // 单一 POST 端点：body 限 1MB，经 AbortSignal 感知客户端断连。
  app.post('/api/ompchamber/control', express.json({ limit: '1mb' }), async (req, res) => {
    const controller = new AbortController();
    // 客户端断开（请求中止或响应关闭）且响应尚未写完时，取消进行中的动作。
    const abortOnDisconnect = () => {
      if (!res.writableEnded) controller.abort();
    };
    req.once('aborted', abortOnDisconnect);
    res.once('close', abortOnDisconnect);
    try {
      // 只接受字符串 action；input 必须是纯对象，其余一律视为空对象。
      const action = typeof req.body?.action === 'string' ? req.body.action : '';
      const requestInput = req.body?.input;
      const input = requestInput && typeof requestInput === 'object' && !Array.isArray(requestInput)
        ? requestInput
        : {};
      const data = await controlService.execute(action, input, req.body?.contextDirectory, { signal: controller.signal });
      return res.json(data);
    } catch (error) {
      const controlError = asControlError(error, 'OMPChamber control action failed');
      return res.status(controlError.statusCode).json({
        error: controlError.message,
        ...(controlError.partial === true ? {
          partial: true,
          partialAction: controlError.partialAction,
          sessionId: controlError.sessionId,
          directory: controlError.directory,
        } : {}),
      });
    // 无论成败都摘掉断连监听，避免泄漏到已结束的请求上。
    } finally {
      req.off('aborted', abortOnDisconnect);
      res.off('close', abortOnDisconnect);
    }
  });
};
