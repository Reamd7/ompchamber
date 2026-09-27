/**
 * Request/response broker between the agent tool and the in-app browser.
 *
 * The browser lives in the renderer, not the server, so the server cannot act
 * on a page directly. It publishes a request over the existing OMPChamber
 * event stream and waits for the client that owns the browser view to post the
 * result back.
 *
 * The request goes to every client that could serve it, because the server
 * cannot know which one is showing a page. Exactly one must act on it, so a
 * client claims the request before touching anything and only the first claim
 * is granted. Without that, two connected desktop clients would both click, and
 * the losing one's late result would not undo what it had already done.
 *
 * Two failure modes matter and are handled explicitly rather than as timeouts:
 *
 * - No client is listening. The agent is told immediately that the browser is
 *   not open, instead of blocking for the full timeout and then reporting
 *   something ambiguous.
 * - The client accepted the request and then went away. That still times out,
 *   because the alternative — assuming success — would be a lie.
 */
/**
 * agent 工具与应用内浏览器之间的请求/响应中介（中文说明）。
 *
 * 浏览器活在渲染进程而非服务端里，服务端无法直接操作页面。中介通过既
 * 有的 OMPChamber 事件流发布请求，等待持有浏览器视图的客户端把结果回
 * 传。
 *
 * 请求发给所有可能响应的客户端，因为服务端不知道谁开着页面。必须恰好
 * 有一个客户端执行它：客户端先认领（claim）请求再动手，只有第一个认
 * 领生效。否则两个已连接的桌面客户端都会去点击，落选者迟到的结果撤销
 * 不了它已经造成的后果。
 *
 * 两种失败模式被显式处理而不是一律靠超时：没有客户端在听——立刻告诉
 * agent 浏览器没开，而不是耗满超时再给出模糊答复；客户端接了请求然后
 * 掉线——仍然按超时处理，因为假定成功等于说谎。
 */

/** 默认等待客户端响应的超时（毫秒）。 */
const DEFAULT_TIMEOUT_MS = 20_000;
/** 允许设置的超时上限（毫秒）；更长的请求会被压到这里。 */
const MAX_TIMEOUT_MS = 120_000;

/**
 * 浏览器控制类错误：携带 HTTP 风格的 status（默认 400），让 agent 能
 * 据此区分环境不具备（503）、超时（504）、取消（499）与普通失败。
 */
export class BrowserControlError extends Error {
  /** 以 message 与 status 构造错误；name 固定为 BrowserControlError。 */
  constructor(message, status = 400) {
    super(message);
    this.name = 'BrowserControlError';
    this.status = status;
  }
}

/**
 * 创建 broker 实例。依赖：emitRequest（发布请求并返回能响应它的客户端
 * 数，必填，缺失抛 TypeError）、createId（生成请求 id）、setTimer 与
 * clearTimer（可注入的定时器，测试用）。返回
 * { pendingCount, request, claim, resolve, rejectAll }。
 */
export const createBrowserControlBroker = ({
  emitRequest,
  createId,
  setTimer = setTimeout,
  clearTimer = clearTimeout,
} = {}) => {
  if (typeof emitRequest !== 'function') {
    throw new TypeError('emitRequest is required');
  }

  // 在途请求表：requestId → { finish, timer, claimed }。
  const pending = new Map();

  /**
   * 结算一个在途请求：命中则移出表、清掉定时器并唤醒等待方，返回
   * true；未知或已结算的 id 返回 false 且不做任何事。
   */
  const settle = (requestId, outcome) => {
    const entry = pending.get(requestId);
    if (!entry) return false;
    pending.delete(requestId);
    clearTimer(entry.timer);
    entry.finish(outcome);
    return true;
  };

  return {
    /** Number of requests still awaiting a client response. */
    /** 仍在等待客户端响应的请求数（中文补充）。 */
    get pendingCount() {
      return pending.size;
    },

    /**
     * Publishes one browser action and resolves with the client's result.
     * Rejects with a BrowserControlError the agent can act on.
     */
    /**
     * 发布一个浏览器动作并等待客户端结果（中文补充）。没有客户端可响应
     * 时立即以 503 拒绝；超时被夹在 1 秒到 MAX_TIMEOUT_MS 之间；支持经
     * AbortSignal 取消（按 499 结算）。所有拒绝值都是 BrowserControlError。
     */
    request(action, parameters = {}, { timeoutMs = DEFAULT_TIMEOUT_MS, signal } = {}) {
      const requestId = typeof createId === 'function' ? createId() : `browser-${Date.now()}-${pending.size}`;
      const boundedTimeout = Math.min(Math.max(1_000, Number(timeoutMs) || DEFAULT_TIMEOUT_MS), MAX_TIMEOUT_MS);

      const listenerCount = emitRequest({ requestId, action, parameters });
      if (!listenerCount) {
        // Written for the agent reading it, not the user: state what this
        // environment can do, and leave deciding whether it matters to the
        // caller rather than handing it an instruction it cannot carry out.
        return Promise.reject(new BrowserControlError(
          'No OMPChamber client connected here can control a page. Reading and '
          + 'interacting with a page works when OMPChamber runs as its desktop '
          + 'application; a web browser tab can display a page but cannot be '
          + 'driven. Nothing was changed. Mention this to the user only if it '
          + 'affects what they asked for.',
          503,
        ));
      }

      return new Promise((resolve, reject) => {
        // 结算回调：ok 则兑现 data，否则按 message/status 拒绝，并解绑 abort 监听。
        const finish = (outcome) => {
          if (signal && onAbort) signal.removeEventListener('abort', onAbort);
          if (outcome.ok) resolve(outcome.data ?? null);
          else reject(new BrowserControlError(outcome.message || 'Browser action failed', outcome.status || 400));
        };

        // abort 监听：调用方取消时以 499 结算在途请求。
        const onAbort = signal
          ? () => settle(requestId, { ok: false, message: 'Browser action was cancelled', status: 499 })
          : null;
        if (signal) {
          if (signal.aborted) {
            reject(new BrowserControlError('Browser action was cancelled', 499));
            return;
          }
          signal.addEventListener('abort', onAbort, { once: true });
        }

        const timer = setTimer(() => {
          settle(requestId, {
            ok: false,
            message: `The in-app browser did not respond within ${Math.round(boundedTimeout / 1000)}s`,
            status: 504,
          });
        }, boundedTimeout);

        pending.set(requestId, { finish, timer, claimed: false });
      });
    },

    /**
     * Grants the right to perform one request, to one client.
     *
     * The first caller wins; everyone else is told no and must do nothing. An
     * unknown id is also a refusal: the request has already been settled, and
     * acting on it now would change a page nobody is waiting on.
     */
    /**
     * 把一个请求的执行权授予唯一客户端（中文补充）。先到先得，其余申
     * 请者被拒绝且必须什么都不做；未知 id 同样拒绝——请求已结算，此时
     * 再动手会改变一个没人等待的页面。
     */
    claim(requestId) {
      if (typeof requestId !== 'string' || !requestId) return false;
      const entry = pending.get(requestId);
      if (!entry || entry.claimed) return false;
      entry.claimed = true;
      return true;
    },

    /**
     * Accepts a result posted by the client. Returns false for an unknown id,
     * which is the normal outcome for a response that lost a race with the
     * timeout and must not be treated as an error.
     */
    /**
     * 接收客户端回传的结果（中文补充）。ok 为 true 时结算为成功并携带
     * data，否则按 error 文本结算为失败（400）；未知 id 返回 false，这
     * 是与超时竞争落败的正常结果，不视为错误。
     */
    resolve(requestId, result) {
      if (typeof requestId !== 'string' || !requestId) return false;
      if (result && result.ok === true) {
        return settle(requestId, { ok: true, data: result.data ?? null });
      }
      return settle(requestId, {
        ok: false,
        message: typeof result?.error === 'string' && result.error ? result.error : 'Browser action failed',
        status: 400,
      });
    },

    /** Fails everything in flight, e.g. when the owning client disconnects. */
    /** 让所有在途请求失败（例如持有页面的客户端断开），状态码 503。 */
    rejectAll(message) {
      for (const requestId of [...pending.keys()]) {
        settle(requestId, { ok: false, message, status: 503 });
      }
    },
  };
};
