//! Port of `server/lib/browser-control/broker.js` — the request/response
//! broker between the agent tool and the in-app browser.
//!
//! The browser lives in the renderer, not in the server, so the server cannot
//! act on a page directly: it publishes one action through the injected
//! [`EmitRequest`] and waits for the client that owns the browser view to post
//! the result back (see `routes.rs`). The request goes to every client that
//! could serve it, because the server cannot know which one is showing a
//! page. Exactly one must act on it, so a client claims the request before
//! touching anything and only the first claim is granted — without that, two
//! connected desktop clients would both click, and the losing one's late
//! result would not undo what it had already done.
//!
//! Two failure modes are handled explicitly rather than as timeouts:
//!
//! - No client is listening ([`EmitRequest`] returned 0): the caller is told
//!   immediately with [`NO_CLIENTS_MESSAGE`] instead of blocking for the full
//!   timeout and then reporting something ambiguous.
//! - The client accepted the request and then went away: that still times
//!   out, because the alternative — assuming success — would be a lie.
//!
//! 中文说明：本模块是 `server/lib/browser-control/broker.js` 的移植。浏览器位于渲染
//! 进程而非服务器，服务器只能广播动作、等待持有浏览器视图的客户端认领并回传结果；
//! 首个认领者独占执行权，未认领者必须按兵不动。两种失败被显式处理：无人监听时立即
//! 以 [`NO_CLIENTS_MESSAGE`]（503）失败，不空等超时；客户端接受请求后消失则照常超时，
//! 绝不假设成功。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::future::BoxFuture;
use serde_json::Value;
use tokio::sync::{oneshot, watch};

/// 超时下限：任何请求至少等待 1 秒。
const MIN_TIMEOUT_MS: u64 = 1_000;

/// `DEFAULT_TIMEOUT_MS` in broker.js.
/// 中文：未指定或传 0 时的默认等待时长。
pub const DEFAULT_TIMEOUT_MS: u64 = 20_000;
/// `MAX_TIMEOUT_MS` in broker.js.
/// 中文：任何请求的等待上限。
pub const MAX_TIMEOUT_MS: u64 = 120_000;

/// The `message` of the `BrowserControlError` a caller gets when no connected
/// client can serve the action. Written for the agent reading it, not the
/// user: state what this environment can do, and leave deciding whether it
/// matters to the caller rather than handing it an instruction it cannot
/// carry out.
/// 中文：面向 agent 的措辞——陈述环境能力边界，不发出无法执行的指令。
pub const NO_CLIENTS_MESSAGE: &str = "No OMPChamber client connected here can control a page. \
Reading and interacting with a page works when OMPChamber runs as its desktop application; \
a web browser tab can display a page but cannot be driven. Nothing was changed. \
Mention this to the user only if it affects what they asked for.";

/// 调用方主动取消时的错误消息。
const CANCELLED_MESSAGE: &str = "Browser action was cancelled";
/// 客户端未给出具体原因时的兜底失败消息。
const FAILURE_MESSAGE: &str = "Browser action failed";

/// `BrowserControlError` — `message` plus the HTTP-style `status` the owning
/// service maps onto its error response (defaults to 400 in the JS).
/// 中文：`message` 加 HTTP 风格 `status`，服务层据此映射错误响应（JS 侧默认 400）。
#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct BrowserControlError {
    /// 人类可读的错误消息；面向 agent 呈现。
    pub message: String,
    /// HTTP 风格状态码（503 无人监听、504 超时、499 取消等）。
    pub status: u16,
}

/// 构造助手。
impl BrowserControlError {
    /// 以消息与状态码构造错误。
    pub fn new(message: impl Into<String>, status: u16) -> Self {
        Self {
            message: message.into(),
            status,
        }
    }
}

/// Minimal `AbortSignal` subset: clones share one aborted flag, and dropping
/// every clone without aborting never fires (like a garbage-collected signal
/// whose listeners stay silent).
/// 中文：所有 clone 共享同一 aborted 标志；全部 clone 被丢弃而未 abort 时永不触发
///（对应被 GC 回收的 JS signal 保持沉默）。
#[derive(Clone)]
pub struct CancelSignal {
    /// 发送端，所有 clone 经 `Arc` 共享；`send(true)` 即触发。
    tx: Arc<watch::Sender<bool>>,
    /// 接收端快照，用于非阻塞查询。
    rx: watch::Receiver<bool>,
}

/// 触发与查询取消。
impl CancelSignal {
    /// 创建未取消的信号。
    pub fn new() -> Self {
        let (tx, rx) = watch::channel(false);
        Self {
            tx: Arc::new(tx),
            rx,
        }
    }

    /// 触发取消；重复调用无副作用。
    pub fn abort(&self) {
        let _ = self.tx.send(true);
    }

    /// 非阻塞查询当前是否已取消。
    pub fn is_aborted(&self) -> bool {
        *self.rx.borrow()
    }
}

/// `Default` 委托到 [`CancelSignal::new`]。
impl Default for CancelSignal {
    /// 等价于 [`CancelSignal::new`]：创建未取消的信号。
    fn default() -> Self {
        Self::new()
    }
}

/// One action broadcast to every client that could serve it — the argument
/// index.js passes to `emitRequest`.
/// 中文：即 index.js 传给 `emitRequest` 的载荷。
#[derive(Debug, Clone)]
pub struct OutgoingRequest {
    /// 请求唯一标识，客户端凭它认领与回传。
    pub request_id: String,
    /// 动作名（如 `browser.snapshot`）。
    pub action: String,
    /// 动作参数，原样透传给客户端。
    pub parameters: Value,
}

/// Injected transport: writes one request to the connected OMPChamber
/// clients and returns how many were reached, so the broker can fail fast
/// when nobody is listening.
/// 中文：返回到达的客户端数量，0 即可快速失败。
pub type EmitRequest = Arc<dyn Fn(&OutgoingRequest) -> usize + Send + Sync>;

/// Injected id factory (`createId` in index.js: `browser-<uuid>`).
/// 中文：index.js 传入 `browser-<uuid>` 形式的工厂。
pub type CreateId = Arc<dyn Fn() -> String + Send + Sync>;

/// Injected timer (`setTimer`/`clearTimer` in broker.js — a delay future per
/// request). Injectable so timeout behavior is testable without wall-clock
/// waits; the default is a plain tokio sleep.
/// 中文：默认实现是 tokio sleep；注入实现使超时行为可测试且无需真实等待。
type SleepFactory = Arc<dyn Fn(Duration) -> BoxFuture<'static, ()> + Send + Sync>;

/// 默认计时器：每个时长一个 tokio sleep future。
fn default_sleep() -> SleepFactory {
    Arc::new(|duration: Duration| {
        let delay: BoxFuture<'static, ()> = Box::pin(tokio::time::sleep(duration));
        delay
    })
}

/// The result a client posts back to `/api/browser-control/result`.
/// 中文：`ok` 为真时 `data` 有效，否则 `error` 给出原因。
#[derive(Debug, Clone)]
pub struct ClientResult {
    /// 客户端是否成功执行。
    pub ok: bool,
    /// 成功时的返回数据（如页面快照）。
    pub data: Value,
    /// 失败原因；可能为空，由 broker 兜底。
    pub error: String,
}

/// Per-call options (`{ timeoutMs, signal }` in broker.js).
/// 中文：`timeout_ms` 缺省或为 0 时使用默认值。
#[derive(Default)]
pub struct RequestOptions {
    /// 期望的超时毫秒数；会被夹在上下限之间。
    pub timeout_ms: Option<u64>,
    /// 调用方取消信号。
    pub signal: Option<CancelSignal>,
}

/// 一次请求的终局，经 oneshot 交回等待中的调用方。
enum Outcome {
    /// 客户端成功，携带返回数据。
    Success { data: Value },
    /// 失败，携带消息与 HTTP 风格状态码。
    Failure { message: String, status: u16 },
}

/// 一条在途请求的服务端状态，由 `Shared::pending` 持有。
struct PendingEntry {
    /// The `request()` future's settle point (`finish` in broker.js).
    /// 中文：`request()` future 的收结点。
    finish: oneshot::Sender<Outcome>,
    /// Timeout + abort watchdog; aborted on settle (`clearTimer`). `None`
    /// only in the window between registering the entry and spawning the
    /// watchdog.
    /// 中文：注册条目与 spawn 看门狗之间存在短暂窗口，此时为 `None`。
    timer: Option<tokio::task::JoinHandle<()>>,
    /// 是否已被某客户端认领；首认领独占执行权。
    claimed: bool,
}

/// 所有在途请求的共享表，以 request id 为键、受 Mutex 保护。
#[derive(Default)]
struct Shared {
    /// request id → 在途条目。
    pending: Mutex<HashMap<String, PendingEntry>>,
}

/// The broker. Constructed once per server (index.js:1323) and shared between
/// the result routes and the `openchamber_web` tool service.
/// 中文：每服务器构造一次，在结果路由与 `openchamber_web` 工具服务之间共享。
pub struct BrowserControlBroker {
    /// 在途请求表（跨 clone 共享）。
    shared: Arc<Shared>,
    /// 注入的下行传输。
    emit_request: EmitRequest,
    /// 注入的 id 工厂；缺省时用时间戳加表大小的兜底格式。
    create_id: Option<CreateId>,
    /// 注入的计时器工厂（测试可替换）。
    sleep: SleepFactory,
}

/// 构造、请求生命周期与认领/回传入口。
impl BrowserControlBroker {
    /// `createBrowserControlBroker({ emitRequest })` with the JS fallback id
    /// (`browser-<millis>-<pending size>`).
/// 中文：不带 id 工厂的构造形式。
    pub fn new(
        emit_request: impl Fn(&OutgoingRequest) -> usize + Send + Sync + 'static,
    ) -> Arc<Self> {
        Self::build(Arc::new(emit_request), None, default_sleep())
    }

    /// `createBrowserControlBroker({ emitRequest, createId })` — the form
    /// index.js uses (`createId: () => browser-<uuid>`).
/// 中文：index.js 实际使用的构造形式。
    pub fn with_id_factory(
        emit_request: impl Fn(&OutgoingRequest) -> usize + Send + Sync + 'static,
        create_id: impl Fn() -> String + Send + Sync + 'static,
    ) -> Arc<Self> {
        Self::build(
            Arc::new(emit_request),
            Some(Arc::new(create_id)),
            default_sleep(),
        )
    }

    /// Test seam mirroring the JS `setTimer` injection: a broker whose
    /// timeout delay is the injected future factory.
/// 中文：仅测试可见，镜像 JS 的 `setTimer` 注入。
    #[cfg(test)]
    pub(crate) fn new_with_sleep(
        emit_request: impl Fn(&OutgoingRequest) -> usize + Send + Sync + 'static,
        sleep: SleepFactory,
    ) -> Arc<Self> {
        Self::build(Arc::new(emit_request), None, sleep)
    }

    /// 三个构造形式的公共实现。
    fn build(
        emit_request: EmitRequest,
        create_id: Option<CreateId>,
        sleep: SleepFactory,
    ) -> Arc<Self> {
        Arc::new(Self {
            shared: Arc::new(Shared::default()),
            emit_request,
            create_id,
            sleep,
        })
    }

    /// Number of requests still awaiting a client response (`pendingCount`).
/// 中文：对应 JS 的 `pendingCount`。
    pub fn pending_count(&self) -> usize {
        self.shared
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .len()
    }

    /// Publishes one browser action and resolves with the client's result;
    /// rejects with a [`BrowserControlError`] the agent can act on.
/// 中文：无人监听立即 503；先发布再检查已取消（与 JS 执行顺序一致）；先登记条目
/// 再 spawn 看门狗，避免注入的零延迟计时器与登记竞态。
    pub async fn request(
        &self,
        action: impl Into<String>,
        parameters: Value,
        options: RequestOptions,
    ) -> Result<Value, BrowserControlError> {
        let action = action.into();
        let request_id = self.next_request_id();
        let bounded = Duration::from_millis(bounded_timeout_ms(options.timeout_ms));

        let listener_count = (self.emit_request)(&OutgoingRequest {
            request_id: request_id.clone(),
            action,
            parameters,
        });
        if listener_count == 0 {
            return Err(BrowserControlError::new(NO_CLIENTS_MESSAGE, 503));
        }

        // broker.js checks `signal.aborted` inside the promise executor, i.e.
        // after `emitRequest`: an already-cancelled caller still publishes.
        if options
            .signal
            .as_ref()
            .is_some_and(CancelSignal::is_aborted)
        {
            return Err(BrowserControlError::new(CANCELLED_MESSAGE, 499));
        }

        let (finish, rx) = oneshot::channel::<Outcome>();
        // Register before spawning so an injected zero-delay timer can never
        // race the entry it settles (broker.js gets the same ordering from
        // the single-threaded event loop).
        {
            let mut pending = self
                .shared
                .pending
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            pending.insert(
                request_id.clone(),
                PendingEntry {
                    finish,
                    timer: None,
                    claimed: false,
                },
            );
        }
        let timer = tokio::spawn(watchdog(
            Arc::clone(&self.shared),
            request_id.clone(),
            bounded,
            options.signal.clone(),
            Arc::clone(&self.sleep),
        ));
        // If the watchdog already settled (zero-delay timer), the entry is
        // gone and the handle is simply dropped. Scoped so the guard drops
        // before the await below (the future must stay Send).
        {
            let mut pending = self
                .shared
                .pending
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if let Some(entry) = pending.get_mut(&request_id) {
                entry.timer = Some(timer);
            }
        }

        match rx.await {
            Ok(Outcome::Success { data }) => Ok(data),
            Ok(Outcome::Failure { message, status }) => Err(BrowserControlError::new(
                if message.is_empty() {
                    FAILURE_MESSAGE.to_owned()
                } else {
                    message
                },
                if status == 0 { 400 } else { status },
            )),
            // Unreachable in practice: every pending removal goes through
            // `settle`, which always sends. Mapped to the generic failure
            // rather than hanging like an unsettleable JS promise would.
            Err(_) => Err(BrowserControlError::new(FAILURE_MESSAGE, 400)),
        }
    }

    /// 优先调用注入的 id 工厂；否则兜底为 `browser-<millis>-<pending>`。
    fn next_request_id(&self) -> String {
        if let Some(create_id) = &self.create_id {
            return create_id();
        }
        let millis = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or_default();
        let pending = self.pending_count();
        format!("browser-{millis}-{pending}")
    }

    /// Grants the right to perform one request, to one client. The first
    /// caller wins; everyone else is told no and must do nothing. An unknown
    /// id is also a refusal: the request has already been settled, and acting
    /// on it now would change a page nobody is waiting on.
/// 中文：未知 id 同样拒绝——请求已收结，此刻动手会改动无人等待的页面。
    pub fn claim(&self, request_id: &str) -> bool {
        if request_id.is_empty() {
            return false;
        }
        let mut pending = self
            .shared
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        match pending.get_mut(request_id) {
            Some(entry) if !entry.claimed => {
                entry.claimed = true;
                true
            }
            _ => false,
        }
    }

    /// Accepts a result posted by the client. Returns false for an unknown
    /// id, which is the normal outcome for a response that lost a race with
    /// the timeout and must not be treated as an error.
/// 中文：失败结果缺消息时用兜底文案，状态码固定 400。
    pub fn resolve(&self, request_id: &str, result: ClientResult) -> bool {
        if request_id.is_empty() {
            return false;
        }
        if result.ok {
            settle(
                &self.shared,
                request_id,
                Outcome::Success { data: result.data },
            )
        } else {
            let message = if result.error.is_empty() {
                FAILURE_MESSAGE.to_owned()
            } else {
                result.error
            };
            settle(
                &self.shared,
                request_id,
                Outcome::Failure {
                    message,
                    status: 400,
                },
            )
        }
    }

    /// Fails everything in flight, e.g. when the owning client disconnects.
/// 中文：全部以 503 失败收结（如持有客户端断开），并清空在途表。
    pub fn reject_all(&self, message: &str) {
        let ids: Vec<String> = self
            .shared
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .keys()
            .cloned()
            .collect();
        for id in ids {
            settle(
                &self.shared,
                &id,
                Outcome::Failure {
                    message: message.to_owned(),
                    status: 503,
                },
            );
        }
    }
}

/// `settle` in broker.js: drop the pending entry, clear the timer, hand the
/// outcome to the waiting caller. Idempotent — an unknown id settles nothing.
/// 中文：未知 id 是无操作；收结同时中止看门狗任务。
fn settle(shared: &Shared, request_id: &str, outcome: Outcome) -> bool {
    let mut pending = shared.pending.lock().unwrap_or_else(|e| e.into_inner());
    let Some(entry) = pending.remove(request_id) else {
        return false;
    };
    if let Some(timer) = &entry.timer {
        // Self-abort (the watchdog settling its own request) is harmless: it
        // has no await point left after settle, so the task simply finishes.
        timer.abort();
    }
    let _ = entry.finish.send(outcome);
    true
}

/// Per-request timer plus abort listener (`setTimer` + the `signal` wiring).
/// Settles exactly once: the timeout branch and the abort branch are mutually
/// exclusive, and a settle from the result route aborts this task first.
/// 中文：超时分支与取消分支互斥，先到者收结；发送端消失则退回纯超时等待。
async fn watchdog(
    shared: Arc<Shared>,
    request_id: String,
    timeout: Duration,
    signal: Option<CancelSignal>,
    sleep: SleepFactory,
) {
    let timed_out = async {
        sleep(timeout).await;
        settle(
            &shared,
            &request_id,
            Outcome::Failure {
                message: timeout_message(timeout),
                status: 504,
            },
        );
    };
    tokio::pin!(timed_out);

    let Some(signal) = signal else {
        timed_out.await;
        return;
    };
    let mut aborted = signal.rx.clone();
    tokio::select! {
        _ = &mut timed_out => {}
        changed = aborted.changed() => match changed {
            Ok(()) => {
                settle(
                    &shared,
                    &request_id,
                    Outcome::Failure {
                        message: CANCELLED_MESSAGE.to_owned(),
                        status: 499,
                    },
                );
            }
            // Sender dropped without abort(): the JS signal was simply
            // garbage-collected, which never fires its listeners — fall
            // through to the timeout.
            Err(_) => timed_out.await,
        },
    }
}

/// `Math.min(Math.max(1_000, Number(timeoutMs) || DEFAULT), 120_000)`.
/// 中文：夹在 [1s, 120s] 区间；`Some(0)` 与 `None` 等同默认值。
fn bounded_timeout_ms(timeout_ms: Option<u64>) -> u64 {
    let requested = match timeout_ms {
        Some(0) | None => DEFAULT_TIMEOUT_MS,
        Some(ms) => ms,
    };
    requested.clamp(MIN_TIMEOUT_MS, MAX_TIMEOUT_MS)
}

/// `The in-app browser did not respond within ${Math.round(ms / 1000)}s`.
/// 中文：秒数四舍五入后嵌入消息。
fn timeout_message(timeout: Duration) -> String {
    let seconds = (timeout.as_millis() as f64 / 1000.0).round() as u64;
    format!("The in-app browser did not respond within {seconds}s")
}

/// index.js `createId`: `browser-${crypto.randomUUID()}` — a random v4 UUID.
/// 中文：手工置位 version/variant 位，输出标准 8-4-4-4-12 分组。
pub fn uuid_request_id() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 16];
    rand::rng().fill_bytes(&mut bytes);
    bytes[6] = (bytes[6] & 0x0f) | 0x40; // version 4
    bytes[8] = (bytes[8] & 0x3f) | 0x80; // RFC 4122 variant
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!(
        "browser-{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}
/// 单元测试：覆盖结果回传、快速失败、超时、取消、认领互斥与能力区分等行为契约。
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 记录每次广播的请求并返回固定的监听数。
    fn recording_emit(
        log: &Arc<Mutex<Vec<OutgoingRequest>>>,
        listeners: usize,
    ) -> impl Fn(&OutgoingRequest) -> usize + Send + Sync + 'static {
        let log = Arc::clone(log);
        move |payload| {
            log.lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(payload.clone());
            listeners
        }
    }

    /// The JS tests inject `setTimer` and fire it manually; the Rust seam
    /// equivalent is a timer that is already due.
    /// 中文：计时器立即到期，等价于 JS 测试中手动触发 `setTimer`。
    fn instant_sleep() -> SleepFactory {
        Arc::new(|_duration: Duration| {
            let fired: BoxFuture<'static, ()> = Box::pin(futures::future::ready(()));
            fired
        })
    }

    /// 构造成功回传。
    fn ok_result(data: Value) -> ClientResult {
        ClientResult {
            ok: true,
            data,
            error: String::new(),
        }
    }

    /// 构造携带错误消息的失败回传。
    fn failed_result(error: &str) -> ClientResult {
        ClientResult {
            ok: false,
            data: Value::Null,
            error: error.to_string(),
        }
    }

    /// Starts a request the way the JS tests do: `broker.request(...)` runs
    /// its promise executor synchronously, so by the time the call returns
    /// the action is emitted and the request registered. A spawned Rust task
    /// reaches that same point after one yield.
    /// 中文：spawn 后让出一次，确保动作已广播、请求已登记。
    async fn start_request(
        broker: &Arc<BrowserControlBroker>,
        action: &str,
        parameters: Value,
        options: RequestOptions,
    ) -> tokio::task::JoinHandle<Result<Value, BrowserControlError>> {
        let handle = {
            let broker = Arc::clone(broker);
            let action = action.to_string();
            tokio::spawn(async move { broker.request(action, parameters, options).await })
        };
        tokio::task::yield_now().await;
        handle
    }

    /// 取广播日志中的首条请求。
    fn first_emitted(log: &Mutex<Vec<OutgoingRequest>>) -> OutgoingRequest {
        log.lock().unwrap_or_else(|e| e.into_inner())[0].clone()
    }

    /// 客户端回传的数据原样成为请求结果。
    #[tokio::test]
    async fn resolves_with_the_data_the_client_posted_back() {
        let emitted = Arc::new(Mutex::new(Vec::new()));
        let broker = BrowserControlBroker::new(recording_emit(&emitted, 1));
        let inflight = start_request(
            &broker,
            "browser.snapshot",
            json!({}),
            RequestOptions::default(),
        )
        .await;

        let payload = first_emitted(&emitted);
        assert_eq!(payload.action, "browser.snapshot");

        assert!(broker.resolve(
            &payload.request_id,
            ok_result(json!({ "url": "http://localhost:5173/" }))
        ));
        assert_eq!(
            inflight.await.unwrap().unwrap(),
            json!({ "url": "http://localhost:5173/" })
        );
    }

    /// 无人监听时立即以 503 失败，而不是阻塞到超时。
    #[tokio::test]
    async fn fails_fast_when_no_client_is_connected_instead_of_blocking() {
        let broker = BrowserControlBroker::new(|_| 0);
        let error = broker
            .request(
                "browser.open",
                json!({ "url": "http://a/" }),
                RequestOptions::default(),
            )
            .await
            .unwrap_err();
        assert_eq!(error.status, 503);
    }

    /// 无人监听的错误消息陈述环境限制，而非给 agent 下达无法执行的指令。
    #[tokio::test]
    async fn describes_the_environment_rather_than_telling_the_agent_what_to_do() {
        let broker = BrowserControlBroker::new(|_| 0);
        let error = broker
            .request("browser.snapshot", json!({}), RequestOptions::default())
            .await
            .unwrap_err();
        assert_eq!(error.status, 503);
        // The agent reads this, not the user: it must state the limitation and
        // where the capability exists, without issuing an instruction the
        // agent cannot carry out.
        assert!(error.message.contains("desktop application"));
        assert!(error.message.contains("Nothing was changed"));
        assert!(!error.message.contains("Ask the user to open"));
    }

    /// 客户端报告的失败按其原始消息透出。
    #[tokio::test]
    async fn surfaces_a_client_reported_failure_with_its_message() {
        let emitted = Arc::new(Mutex::new(Vec::new()));
        let broker = BrowserControlBroker::new(recording_emit(&emitted, 1));
        let inflight = start_request(
            &broker,
            "browser.click",
            json!({ "selector": "#missing" }),
            RequestOptions::default(),
        )
        .await;

        let payload = first_emitted(&emitted);
        assert!(broker.resolve(
            &payload.request_id,
            failed_result("No element matches #missing")
        ));
        assert_eq!(
            inflight.await.unwrap().unwrap_err().message,
            "No element matches #missing"
        );
    }

    /// 客户端接受请求后未应答则按超时失败（504）。
    #[tokio::test]
    async fn times_out_when_the_client_accepted_the_request_and_never_answered() {
        let broker = BrowserControlBroker::new_with_sleep(|_| 1, instant_sleep());
        let error = broker
            .request(
                "browser.snapshot",
                json!({}),
                RequestOptions {
                    timeout_ms: Some(5_000),
                    ..RequestOptions::default()
                },
            )
            .await
            .unwrap_err();
        assert_eq!(
            error.message,
            "The in-app browser did not respond within 5s"
        );
        assert_eq!(error.status, 504);
    }

    /// 输给超时的迟到回传被静默忽略，不视为错误。
    #[tokio::test]
    async fn ignores_a_late_response_that_lost_the_race_with_the_timeout() {
        let emitted = Arc::new(Mutex::new(Vec::new()));
        let broker =
            BrowserControlBroker::new_with_sleep(recording_emit(&emitted, 1), instant_sleep());

        let error = broker
            .request("browser.snapshot", json!({}), RequestOptions::default())
            .await
            .unwrap_err();
        assert!(error.message.contains("did not respond within 20s"));
        let payload = first_emitted(&emitted);
        assert!(!broker.resolve(&payload.request_id, ok_result(json!({}))));
    }

    /// 未知或空 request id 的回传返回 false，不 panic。
    #[tokio::test]
    async fn rejects_an_unknown_request_id_without_throwing() {
        let broker = BrowserControlBroker::new(|_| 1);
        assert!(!broker.resolve("nope", ok_result(Value::Null)));
        assert!(!broker.resolve("", ok_result(Value::Null)));
    }

    /// 请求收结后待处理表清空。
    #[tokio::test]
    async fn clears_pending_state_once_a_request_settles() {
        let emitted = Arc::new(Mutex::new(Vec::new()));
        let broker = BrowserControlBroker::new(recording_emit(&emitted, 1));
        let inflight = start_request(
            &broker,
            "browser.snapshot",
            json!({}),
            RequestOptions::default(),
        )
        .await;
        assert_eq!(broker.pending_count(), 1);

        let payload = first_emitted(&emitted);
        assert!(broker.resolve(&payload.request_id, ok_result(Value::Null)));
        assert_eq!(inflight.await.unwrap().unwrap(), Value::Null);
        assert_eq!(broker.pending_count(), 0);
    }

    /// 持有客户端断开时所有在途请求以 503 失败。
    #[tokio::test]
    async fn fails_everything_in_flight_when_the_owning_client_disconnects() {
        let emitted = Arc::new(Mutex::new(Vec::new()));
        let broker = BrowserControlBroker::new(recording_emit(&emitted, 1));
        let inflight = start_request(
            &broker,
            "browser.snapshot",
            json!({}),
            RequestOptions::default(),
        )
        .await;

        broker.reject_all("The OMPChamber client disconnected");
        let error = inflight.await.unwrap().unwrap_err();
        assert!(error.message.contains("disconnected"));
        assert_eq!(error.status, 503);
        assert_eq!(broker.pending_count(), 0);
    }

    /// 调用方 abort 信号在请求进行中触发取消（499）。
    #[tokio::test]
    async fn propagates_cancellation_from_the_caller() {
        let emitted = Arc::new(Mutex::new(Vec::new()));
        let broker = BrowserControlBroker::new(recording_emit(&emitted, 1));
        let signal = CancelSignal::new();
        let inflight = start_request(
            &broker,
            "browser.snapshot",
            json!({}),
            RequestOptions {
                signal: Some(signal.clone()),
                ..RequestOptions::default()
            },
        )
        .await;

        signal.abort();
        let error = inflight.await.unwrap().unwrap_err();
        assert_eq!(error.message, "Browser action was cancelled");
        assert_eq!(error.status, 499);
    }

    /// 调用方在发起前已取消则立即拒绝。
    #[tokio::test]
    async fn rejects_immediately_when_the_caller_is_already_cancelled() {
        let emitted = Arc::new(Mutex::new(Vec::new()));
        let broker = BrowserControlBroker::new(recording_emit(&emitted, 1));
        let signal = CancelSignal::new();
        signal.abort();

        let error = broker
            .request(
                "browser.snapshot",
                json!({}),
                RequestOptions {
                    signal: Some(signal),
                    ..RequestOptions::default()
                },
            )
            .await
            .unwrap_err();
        assert_eq!(error.message, "Browser action was cancelled");
        assert_eq!(error.status, 499);
    }

    // Whether a page can be driven depends on which client is connected, not
    // on the server: a desktop shell and a browser tab can be attached to one
    // server at once. The broker is told how many clients could actually
    // perform each action.
    /// 按动作名返回可服务该动作的客户端数，用于能力区分测试。
    fn capability_emit(
        log: &Arc<Mutex<Vec<OutgoingRequest>>>,
        capable_for: impl Fn(&str) -> usize + Send + Sync + 'static,
    ) -> impl Fn(&OutgoingRequest) -> usize + Send + Sync + 'static {
        let log = Arc::clone(log);
        move |payload| {
            log.lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(payload.clone());
            capable_for(&payload.action)
        }
    }

    /// 只能展示页面、不能驱动页面的客户端也可完成 browser.open。
    #[tokio::test]
    async fn opening_a_page_works_with_a_client_that_cannot_drive_one() {
        // A browser tab can display a page even though it cannot be controlled.
        let emitted = Arc::new(Mutex::new(Vec::new()));
        let broker = BrowserControlBroker::new(capability_emit(&emitted, |action| {
            if action == "browser.open" { 1 } else { 0 }
        }));
        let inflight = start_request(
            &broker,
            "browser.open",
            json!({ "url": "http://localhost:3000/" }),
            RequestOptions::default(),
        )
        .await;

        let payload = first_emitted(&emitted);
        assert!(broker.resolve(&payload.request_id, ok_result(json!({ "opened": true }))));
        assert_eq!(inflight.await.unwrap().unwrap(), json!({ "opened": true }));
    }

    /// 无任何客户端可驱动页面时立即失败并说明环境。
    #[tokio::test]
    async fn driving_a_page_fails_immediately_when_no_client_can() {
        let emitted = Arc::new(Mutex::new(Vec::new()));
        let broker = BrowserControlBroker::new(capability_emit(&emitted, |action| {
            if action == "browser.open" { 1 } else { 0 }
        }));
        let error = broker
            .request(
                "browser.click",
                json!({ "selector": "#a" }),
                RequestOptions::default(),
            )
            .await
            .unwrap_err();
        assert!(error.message.contains("desktop application"));
    }

    /// 可驱动的客户端一接入即可服务请求，无需重启或改配置。
    #[tokio::test]
    async fn driving_a_page_works_as_soon_as_a_capable_client_is_connected() {
        // No restart, no setting: a desktop client attaching is enough.
        let emitted = Arc::new(Mutex::new(Vec::new()));
        let broker = BrowserControlBroker::new(capability_emit(&emitted, |_| 1));
        let inflight = start_request(
            &broker,
            "browser.snapshot",
            json!({}),
            RequestOptions::default(),
        )
        .await;

        let payload = first_emitted(&emitted);
        assert!(broker.resolve(
            &payload.request_id,
            ok_result(json!({ "url": "http://localhost:3000/" }))
        ));
        assert_eq!(
            inflight.await.unwrap().unwrap(),
            json!({ "url": "http://localhost:3000/" })
        );
    }

    /// 首个认领者获得执行权，其余客户端被拒绝。
    #[tokio::test]
    async fn grants_the_request_to_the_first_claimant_and_refuses_the_rest() {
        let broker = BrowserControlBroker::with_id_factory(|_| 2, || "req-1".to_string());
        let pending = start_request(
            &broker,
            "browser.click",
            json!({ "selector": "button" }),
            RequestOptions::default(),
        )
        .await;

        assert!(broker.claim("req-1"));
        // A second desktop client is told no, so it never clicks.
        assert!(!broker.claim("req-1"));

        assert!(broker.resolve("req-1", ok_result(json!({ "clicked": true }))));
        assert_eq!(pending.await.unwrap().unwrap(), json!({ "clicked": true }));
    }

    /// 已收结请求的认领被拒绝（此刻动手无人等待）。
    #[tokio::test]
    async fn refuses_a_claim_for_a_request_that_is_already_over() {
        let broker = BrowserControlBroker::with_id_factory(|_| 1, || "req-1".to_string());
        let pending = start_request(
            &broker,
            "browser.click",
            json!({}),
            RequestOptions::default(),
        )
        .await;
        assert!(broker.resolve("req-1", ok_result(Value::Null)));
        assert_eq!(pending.await.unwrap().unwrap(), Value::Null);

        // Acting now would change a page nobody is waiting on.
        assert!(!broker.claim("req-1"));
        assert!(!broker.claim("unknown"));
    }

    /// 超时被夹在 1–120 秒之间，0 回退默认值。
    #[tokio::test]
    async fn bounds_the_timeout_between_one_and_120_seconds() {
        // The clamp is observable through the timeout message.
        let under = BrowserControlBroker::new_with_sleep(|_| 1, instant_sleep());
        let error = under
            .request(
                "browser.snapshot",
                json!({}),
                RequestOptions {
                    timeout_ms: Some(10),
                    ..RequestOptions::default()
                },
            )
            .await
            .unwrap_err();
        assert_eq!(
            error.message,
            "The in-app browser did not respond within 1s"
        );

        let over = BrowserControlBroker::new_with_sleep(|_| 1, instant_sleep());
        let error = over
            .request(
                "browser.snapshot",
                json!({}),
                RequestOptions {
                    timeout_ms: Some(600_000),
                    ..RequestOptions::default()
                },
            )
            .await
            .unwrap_err();
        assert_eq!(
            error.message,
            "The in-app browser did not respond within 120s"
        );

        // `timeoutMs: 0` falls back to the default like `Number(0) || DEFAULT`.
        let zero = BrowserControlBroker::new_with_sleep(|_| 1, instant_sleep());
        let error = zero
            .request(
                "browser.snapshot",
                json!({}),
                RequestOptions {
                    timeout_ms: Some(0),
                    ..RequestOptions::default()
                },
            )
            .await
            .unwrap_err();
        assert_eq!(
            error.message,
            "The in-app browser did not respond within 20s"
        );
    }

    /// 生成的 id 为 `browser-` 前缀、标准 v4 UUID 分组。
    #[test]
    fn uuid_request_ids_are_browser_prefixed_uuids() {
        let id = uuid_request_id();
        let uuid = id.strip_prefix("browser-").unwrap_or_default();
        let groups: Vec<&str> = uuid.split('-').collect();
        assert_eq!(groups.len(), 5);
        assert_eq!(
            groups.iter().map(|g| g.len()).collect::<Vec<_>>(),
            vec![8, 4, 4, 4, 12]
        );
        assert!(
            groups
                .iter()
                .flat_map(|g| g.chars())
                .all(|c| c.is_ascii_hexdigit())
        );
        // version 4 nibble
        assert!(groups[2].starts_with('4'));
    }
}
