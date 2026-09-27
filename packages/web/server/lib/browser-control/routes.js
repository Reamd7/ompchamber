/**
 * Result callback for in-app browser actions.
 *
 * The client that owns the browser view posts here with the outcome of a
 * request it received over the event stream. Only the request id is trusted to
 * correlate; an unknown id is accepted with `matched: false` rather than an
 * error, because a client answering after a timeout has done nothing wrong.
 */
/**
 * 应用内浏览器动作的结果回调路由（中文说明）。
 *
 * 持有浏览器视图的客户端把它在事件流里收到的请求的结果回传到这里。
 * 只信请求 id 做关联；未知 id 以 matched: false 接受而不报错，因为超时
 * 之后才应答的客户端并没有做错什么。
 *
 * registerBrowserControlRoutes(app, { express, broker }) 注册两条路由：
 * POST /api/browser-control/claim 供客户端在动手前认领请求；
 * POST /api/browser-control/result 供客户端回传执行结果。
 */
export function registerBrowserControlRoutes(app, { express, broker }) {
  // Claiming is separate from answering so that a client learns whether it may
  // act *before* it acts. Deciding by whose result arrives first would be too
  // late: by then every client has already clicked.
  // 中文补充：认领与应答分开，客户端在动手之前就知道自己能不能动；若
  // 以谁的结果先到为准就太迟了——那时每个客户端都已经点过了。
  app.post('/api/browser-control/claim', express.json({ limit: '4kb' }), (req, res) => {
    const requestId = typeof req.body?.requestId === 'string' ? req.body.requestId.trim() : '';
    if (!requestId) {
      res.status(400).json({ error: 'requestId is required' });
      return;
    }
    res.json({ granted: broker.claim(requestId) });
  });

  // This server attaches body parsing per route rather than globally. Without
  // it `req.body` is undefined here, the client's result is rejected, and the
  // agent sees an unexplained timeout instead of its answer. A page snapshot
  // carries the visible text plus every interactive element, so the limit is
  // sized for that rather than for a small control message.
  // 中文补充：本服务的 body 解析按路由单独挂载，漏挂了它 req.body 就是
  // undefined，客户端结果被拒，agent 只会看到无来由的超时。页面快照携
  // 带可见文本加全部可交互元素，所以上限按此设定而非小控制消息。
  app.post('/api/browser-control/result', express.json({ limit: '2mb' }), (req, res) => {
    const body = req.body;
    if (!body || typeof body !== 'object') {
      res.status(400).json({ error: 'A JSON body is required' });
      return;
    }

    const requestId = typeof body.requestId === 'string' ? body.requestId.trim() : '';
    if (!requestId) {
      res.status(400).json({ error: 'requestId is required' });
      return;
    }

    const matched = broker.resolve(requestId, {
      ok: body.ok === true,
      data: body.data ?? null,
      error: typeof body.error === 'string' ? body.error : '',
    });

    res.json({ matched });
  });
}
