/**
 * Dictation runtime: registers the streaming dictation WebSocket endpoint and
 * the HTTP status/model routes.
 *
 * WebSocket protocol (JSON text frames) on /api/dictation/ws:
 *   client -> server:
 *     { type: 'start',  dictationId, format, options? }
 *       options: { provider?, language?, localModel?, openaiCompatible? }
 *     { type: 'chunk',  dictationId, seq, audio }   // audio: base64 PCM16LE
 *     { type: 'finish', dictationId, finalSeq }
 *     { type: 'cancel', dictationId }
 *     { type: 'ping' }
 *   server -> client:
 *     { type: 'ready' }
 *     { type: 'ack',             dictationId, ackSeq }
 *     { type: 'partial',         dictationId, text }
 *     { type: 'finish_accepted', dictationId, timeoutMs }
 *     { type: 'final',           dictationId, text }
 *     { type: 'error',           dictationId, error, retryable, reasonCode? }
 *     { type: 'pong' }
 */

/**
 * 听写运行时（中文说明）：注册 /api/dictation/ws 流式听写 WebSocket
 * 端点与 HTTP 状态/模型管理路由；每条连接一个 DictationStreamManager，
 * upgrade 前做 UI 认证与 Origin 校验。
 */
import { WebSocketServer } from 'ws';

import { DictationStreamManager } from './stream-manager.js';
import { createDictationService } from './service.js';

/** 流式听写 WebSocket 端点路径。 */
const DICTATION_WS_PATH = '/api/dictation/ws';

/** 单帧最大载荷（base64 音频块），超限由 ws 直接断开。 */
const DICTATION_WS_MAX_PAYLOAD_BYTES = 512 * 1024;
/** 服务端 ping 心跳间隔，防止空闲连接被中间层掐断。 */
const DICTATION_WS_HEARTBEAT_INTERVAL_MS = 30000;

/** 解析请求 URL 的 pathname；URL 非法则退化为按 '?' 截断的字符串。 */
const parseRequestPathname = (url) => {
  try {
    return new URL(url, 'http://localhost').pathname;
  } catch {
    return typeof url === 'string' ? url.split('?')[0] : '';
  }
};

/**
 * 创建听写运行时：挂 HTTP 路由（TTS 合成、状态查询、模型下载/删除）
 * 与 WebSocket 端点，并返回 stop() 用于优雅关闭。
 * @param {object} app express 应用
 * @param {object} server HTTP 服务器（挂 upgrade 事件）
 * @param {object} express express 实例（用于 json 中间件）
 * @param {object} uiAuthController UI 认证控制器（enabled 时校验会话）
 * @param {(req: object) => Promise<boolean>} isRequestOriginAllowed Origin 白名单判定
 * @param {(socket: object, status: number, message: string) => void} rejectWebSocketUpgrade 拒绝 upgrade 的统一出口
 * @param {string} modelsDir 本地模型根目录
 */
export function createDictationRuntime({
  app,
  server,
  express,
  uiAuthController,
  isRequestOriginAllowed,
  rejectWebSocketUpgrade,
  modelsDir,
}) {
  // 听写/TTS 服务实例（本运行时独占）。
  const service = createDictationService({ modelsDir });

  // Local text-to-speech (Kokoro in the dictation worker). Returns WAV bytes;
  // 503 with a reason code while the model is still downloading.
  // 本地 TTS：成功返回 WAV 字节；模型缺失/下载中回 503 并带 reasonCode。
  app.post('/api/dictation/tts/speak', express.json({ limit: '1mb' }), async (req, res) => {
    try {
      const text = typeof req.body?.text === 'string' ? req.body.text.trim() : '';
      if (!text) {
        res.status(400).json({ error: 'Text is required' });
        return;
      }
      const result = await service.synthesizeSpeech({
        text,
        model: typeof req.body?.model === 'string' ? req.body.model : undefined,
        speakerId: Number.isInteger(req.body?.speakerId) ? req.body.speakerId : undefined,
        speed: typeof req.body?.speed === 'number' ? req.body.speed : undefined,
        language: req.body?.language === 'auto' ? 'auto' : undefined,
        languageSample: typeof req.body?.languageSample === 'string' ? req.body.languageSample.slice(0, 4000) : undefined,
      });
      if (result.error) {
        res.status(503).json({
          error: result.error,
          retryable: result.retryable !== false,
          ...(result.reasonCode ? { reasonCode: result.reasonCode } : {}),
        });
        return;
      }
      res.setHeader('Content-Type', result.format || 'audio/wav');
      res.setHeader('X-Speech-Model', result.modelId);
      if (result.language) res.setHeader('X-Speech-Language', result.language);
      res.send(result.audio);
    } catch (error) {
      res.status(500).json({ error: error?.message || 'Failed to synthesize speech' });
    }
  });

  // 状态查询：provider/localModel 查询参数决定要检查哪套配置。
  app.get('/api/dictation/status', async (req, res) => {
    try {
      const provider = typeof req.query.provider === 'string' ? req.query.provider : undefined;
      const localModel = typeof req.query.localModel === 'string' ? req.query.localModel : undefined;
      const status = await service.getStatus({ provider, localModel });
      res.json(status);
    } catch (error) {
      res.status(500).json({ error: error?.message || 'Failed to read dictation status' });
    }
  });

  // 触发指定模型的预下载（设置页使用）。
  app.post('/api/dictation/models/:modelId/download', async (req, res) => {
    try {
      const result = await service.requestModelDownload(req.params.modelId);
      if (!result.ok) {
        res.status(400).json({ error: result.error });
        return;
      }
      res.json(result);
    } catch (error) {
      res.status(500).json({ error: error?.message || 'Failed to start model download' });
    }
  });

  // 删除已安装模型（下载中会被服务层拒绝）。
  app.delete('/api/dictation/models/:modelId', async (req, res) => {
    try {
      const result = await service.deleteModel(req.params.modelId);
      if (!result.ok) {
        res.status(400).json({ error: result.error });
        return;
      }
      res.json(result);
    } catch (error) {
      res.status(500).json({ error: error?.message || 'Failed to delete model' });
    }
  });

  // noServer 模式：由 upgradeHandler 认证后手动完成握手。
  const wsServer = new WebSocketServer({
    noServer: true,
    maxPayload: DICTATION_WS_MAX_PAYLOAD_BYTES,
  });

  // 每条连接：一个发送函数 + 一个管理该连接全部听写流的 manager。
  wsServer.on('connection', (socket) => {
    /** 向客户端发送 JSON 消息；socket 已关闭或发送失败时静默忽略（close 时统一清理）。 */
    const send = (msg) => {
      if (socket.readyState !== 1) {
        return;
      }
      try {
        socket.send(JSON.stringify(msg));
      } catch {
        // socket is going away; the manager cleanup on close handles state
      }
    };

    // 本连接的听写流状态机：emit 回调统一走上面的 send。
    const manager = new DictationStreamManager({
      emit: ({ type, payload }) => send({ type, ...payload }),
      createSttSession: (options) => service.createSttSession(options),
    });

    send({ type: 'ready' });

    // 心跳：只要连接还开着就定期 ping。
    const heartbeatInterval = setInterval(() => {
      if (socket.readyState !== 1) {
        return;
      }
      try {
        socket.ping();
      } catch {
        // ignore
      }
    }, DICTATION_WS_HEARTBEAT_INTERVAL_MS);

    // 消息分发：只认 JSON 文本帧，字段不合法的帧直接忽略。
    socket.on('message', (raw, isBinary) => {
      if (isBinary) {
        return;
      }
      let message;
      try {
        message = JSON.parse(raw.toString('utf8'));
      } catch {
        return;
      }
      if (!message || typeof message !== 'object') {
        return;
      }

      switch (message.type) {
        case 'start': {
          if (typeof message.dictationId !== 'string' || typeof message.format !== 'string') {
            return;
          }
          const options =
            message.options && typeof message.options === 'object' ? message.options : {};
          void manager.handleStart(message.dictationId, message.format, options);
          return;
        }
        case 'chunk': {
          if (
            typeof message.dictationId !== 'string' ||
            typeof message.seq !== 'number' ||
            typeof message.audio !== 'string'
          ) {
            return;
          }
          manager.handleChunk({
            dictationId: message.dictationId,
            seq: message.seq,
            audioBase64: message.audio,
          });
          return;
        }
        case 'finish': {
          if (typeof message.dictationId !== 'string' || typeof message.finalSeq !== 'number') {
            return;
          }
          manager.handleFinish(message.dictationId, message.finalSeq);
          return;
        }
        case 'cancel': {
          if (typeof message.dictationId !== 'string') {
            return;
          }
          manager.handleCancel(message.dictationId);
          return;
        }
        case 'ping': {
          send({ type: 'pong' });
          return;
        }
        default:
      }
    });

    // 连接关闭：停心跳并清理该连接上的全部听写流。
    socket.on('close', () => {
      clearInterval(heartbeatInterval);
      manager.cleanupAll();
    });

    socket.on('error', () => {
      // 'close' follows and performs cleanup.
    });
  });

  /**
   * upgrade 事件处理：只认领听写路径；认证失败回 401、Origin 不允许回
   * 403，全部通过才完成 WebSocket 握手，异常统一回 500。
   */
  const upgradeHandler = (req, socket, head) => {
    const pathname = parseRequestPathname(req.url);
    if (pathname !== DICTATION_WS_PATH) {
      return;
    }

    /** 异步执行认证与握手；任何异常都转化为一次 upgrade 拒绝。 */
    const handleUpgrade = async () => {
      try {
        if (uiAuthController?.enabled) {
          const sessionToken = await uiAuthController?.ensureSessionToken?.(req, null);
          if (!sessionToken) {
            rejectWebSocketUpgrade(socket, 401, 'UI authentication required');
            return;
          }

          const originAllowed = await isRequestOriginAllowed(req);
          if (!originAllowed) {
            rejectWebSocketUpgrade(socket, 403, 'Invalid origin');
            return;
          }
        }

        wsServer.handleUpgrade(req, socket, head, (ws) => {
          wsServer.emit('connection', ws, req);
        });
      } catch {
        rejectWebSocketUpgrade(socket, 500, 'Upgrade failed');
      }
    };

    void handleUpgrade();
  };

  // 挂到 HTTP 服务器的 upgrade 事件上。
  server.on('upgrade', upgradeHandler);

  /** 优雅关闭：摘除 upgrade 监听、以 1001 关闭全部客户端、关 WSS 并停服务。 */
  const stop = () => {
    server.off('upgrade', upgradeHandler);
    for (const client of wsServer.clients) {
      try {
        client.close(1001, 'server shutting down');
      } catch {
        // ignore
      }
    }
    try {
      wsServer.close();
    } catch {
      // ignore
    }
    service.shutdown();
  };

  // 运行时对外的停止函数。
  return { stop };
}
