//! Port of `server/lib/notifications/*` — the notification subsystem:
//! web-push (hand-rolled RFC 8291/8292 on the staged crypto crates), APNs
//! relay/direct delivery, the trigger state machine fed from engine
//! events, templates, the emitter channels, and the `/api/push/*` +
//! `/api/notifications/*` + `/api/sessions/*` route family
//! (`server/lib/notifications/routes.js`).
//!
//! Composition (mirrors `server/index.js`): one event-stream watcher feeds
//! BOTH the session state machine (owned by the `event_stream` module) and
//! the notification trigger runtime. [`router`] builds a private
//! `EventStreamState`; [`router_shared`] accepts the shared one so the
//! composition root can run a single upstream reader.
//!
//! Known composition gaps (main.rs wiring, not module behavior):
//! - `session_goal`'s settle notifier still uses its built-in hub notifier;
//!   wiring `TriggerRuntime::send_goal_settle_push` into
//!   `SessionGoalRuntimeOptions::notifier` restores the full desktop +
//!   push fanout.
//! - The permission auto-accept resolver (`setGetIsSessionAutoAccepting`)
//!   defaults to this module's own mirror set until the composition root
//!   hands over the permission module's shared instance.
//!
//! 中文概述：通知子系统门面：组装 PushRuntime / ApnsRuntime /
//! TemplateRuntime / EmitterRuntime / TriggerRuntime 与 settings 存储，
//! 对外暴露 HTTP 路由族（`router` / `router_shared`）、engine 事件
//! 桥接（`spawn_trigger_bridge`）与桌面壳钩子注入点（`set_desktop_hooks`）。
//! 状态在 `NotificationsState::new_with` 中一次性构造完毕，之后各子
//! 运行时独立工作、共享同一份 settings 与事件流。

/// 触发链路集成测试模块：假 transport + 罐头 engine 应答驱动完整 fanout。
#[cfg(test)]
mod trigger_tests;

/// Serializes env-mutating tests across the whole notifications module
/// (parallel test threads share the process environment).
/// 中文：进程级互斥锁；触发测试持锁直到用例结束，防止并行用例互相
/// 覆盖 OMPCHAMBER_* 环境变量。
#[cfg(test)]
pub(crate) static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// APNs 推送：token 注册表、provider JWT 签名、relay 与直连两种投递模式。
pub mod apns_runtime;
/// Web-push/APNs 密码学原语（RFC 8291/8292、P1363 签名、JWK、.p8 解析）。
pub mod crypto;
/// 通知发射通道：桌面回调、stdout 协议、hub 广播与 SSE。
pub mod emitter_runtime;
/// 通知文案归一化与截断（JS message.js 的移植）。
pub mod message;
/// Web-push 订阅注册表与 VAPID 加密发送。
pub mod push_runtime;
/// `/api/push/*`、`/api/notifications/*`、`/api/sessions/*` 路由族。
pub mod routes;
/// 通知模板解析：settings 模板 + engine 取数 + session 信息缓存。
pub mod template_runtime;
/// 可注入 HTTP POST 接缝（生产 reqwest / 测试假实现）。
pub mod transport;
/// 触发状态机：ready/error/question/permission/goal 等事件的门控、
/// 冷却、去抖与角标管理。
pub mod trigger_runtime;

use std::sync::Arc;

use tokio::sync::broadcast;

use crate::context::RouterContext;
use crate::engine::EngineState;
use crate::event_stream::EventStreamState;
use crate::notifications::apns_runtime::ApnsRuntime;
use crate::notifications::emitter_runtime::{EmitterRuntime, env_desktop_notify};
use crate::notifications::push_runtime::PushRuntime;
use crate::notifications::template_runtime::{TemplateRuntime, engine_json_fetch};
use crate::notifications::transport::{HttpPost, reqwest_post};
use crate::notifications::trigger_runtime::TriggerRuntime;
use crate::settings::SettingsStore;

/// Everything the route family and the trigger bridge share.
/// Desktop-shell integration hooks (control channel, see desktop_control.rs).
/// Registered at boot before composition; consumed once when the state is built.
/// 中文：两个回调槽分别接入 EmitterRuntime（原生通知）与
/// TriggerRuntime（窗口聚焦抑制门控）；必须在 NotificationsState
/// 构造之前注册，构造时被一次性消费。
pub struct DesktopHooks {
/// 桌面壳的原生通知回调（注入到 EmitterRuntime 的桌面槽位）。
    pub on_notification: Arc<dyn Fn(&serde_json::Value) + Send + Sync>,
/// 查询主窗口当前是否聚焦（注入到 TriggerRuntime 的抑制判断）。
    pub is_focused: Arc<dyn Fn() -> bool + Send + Sync>,
}

/// 进程全局钩子槽：boot 阶段 `set_desktop_hooks` 写入，
/// `NotificationsState::new_with` 构造时读取一次。
static DESKTOP_HOOKS: std::sync::RwLock<Option<DesktopHooks>> = std::sync::RwLock::new(None);

/// 注册桌面壳钩子；晚于状态构造调用不会生效（状态已消费完毕）。
pub fn set_desktop_hooks(hooks: DesktopHooks) {
    *DESKTOP_HOOKS.write().unwrap_or_else(|e| e.into_inner()) = Some(hooks);
}

/// Test teardown: the control-channel test registers process-global hooks
/// that would otherwise leak "window focused" into other notification tests.
/// 中文：仅测试清理使用 —— 控制通道用例注册的「窗口聚焦」若不清除，
/// 会泄漏并抑制其它通知用例的推送。
#[cfg(test)]
pub fn clear_desktop_hooks() {
    *DESKTOP_HOOKS.write().unwrap_or_else(|e| e.into_inner()) = None;
}

/// 通知子系统共享状态：路由族与事件桥接都以 `Arc<Self>` 持有它；
/// 各字段在构造后不可变，子运行时内部自行管理可变状态。
pub struct NotificationsState {
/// 服务器上下文（配置、engine 客户端、全局 hub）。
    pub ctx: RouterContext,
/// 共享事件流状态：session 状态机与通知触发器共用同一上游读取者。
    pub events: Arc<EventStreamState>,
/// settings 存储：通知开关、VAPID/APNs 凭据、模板等。
    pub settings: Arc<SettingsStore>,
/// Web-push 订阅注册与加密发送运行时。
    pub push: Arc<PushRuntime>,
/// APNs token 注册与投递运行时（relay 或直连）。
    pub apns: Arc<ApnsRuntime>,
/// 模板运行时，兼作 session 标题/信息缓存。
    pub templates: Arc<TemplateRuntime>,
/// 通知发射器：桌面回调、stdout、hub、SSE 四路出口。
    pub emitter: Arc<EmitterRuntime>,
/// 触发状态机：门控、冷却、去抖、角标与 goal 结算推送。
    pub trigger: Arc<TriggerRuntime>,
    /// Serialized synthetic payloads for `/api/notifications/stream`
    /// clients (the JS `uiNotificationClients` set).
/// 中文：容量 128 的广播通道；SSE 客户端滞后（Lagged）时丢帧，
/// 不会阻塞发送方。
    pub notification_tx: broadcast::Sender<String>,
}

/// 三个构造层级：生产默认 → 注入 transport → 全注入（测试用）。
impl NotificationsState {
/// 生产构造：使用默认 reqwest 客户端（rustls + h2）作 HTTP transport。
    pub fn new(ctx: RouterContext, events: Arc<EventStreamState>) -> Arc<Self> {
        Self::new_with_transport(ctx, events, reqwest_post(default_http_client()))
    }

/// 注入 HTTP transport 的构造变体（替换网络层而不动 engine 取数）。
    pub fn new_with_transport(
        ctx: RouterContext,
        events: Arc<EventStreamState>,
        transport: HttpPost,
    ) -> Arc<Self> {
        let fetch = engine_json_fetch(Arc::clone(&ctx.engine));
        Self::new_with(ctx, events, transport, fetch)
    }

    /// Full-injection constructor (tests drive the engine seam).
/// 中文：读取 settings、创建 broadcast 通道与五个子运行时，并在末尾
/// 一次性消费 DESKTOP_HOOKS 中已注册的桌面钩子；测试经由此入口注入
/// 假 transport 与罐头 engine 取数闭包。
    pub fn new_with(
        ctx: RouterContext,
        events: Arc<EventStreamState>,
        transport: HttpPost,
        fetch: template_runtime::EngineJsonFetch,
    ) -> Arc<Self> {
        let settings = crate::settings::store(&ctx);
        let (notification_tx, _) = broadcast::channel(128);
        let push = PushRuntime::new(
            ctx.config.data_dir.join("push-subscriptions.json"),
            Arc::clone(&settings),
            Arc::clone(&transport),
        );
        let apns = ApnsRuntime::new(
            ctx.config.data_dir.join("apns-tokens.json"),
            Arc::clone(&settings),
            transport,
        );
        let templates = TemplateRuntime::new(Arc::clone(&settings), fetch, "git".to_string());
        let desktop_notify = Arc::new(env_desktop_notify);
        let emitter = EmitterRuntime::new(
            desktop_notify,
            emitter_runtime::DESKTOP_NOTIFY_PREFIX,
            notification_tx.clone(),
            Arc::clone(&ctx.hub),
        );
        let trigger = TriggerRuntime::new(
            Arc::clone(&templates),
            Arc::clone(&emitter),
            Arc::clone(&push),
            Arc::clone(&apns),
            Arc::clone(&settings),
        );
        if let Some(hooks) = DESKTOP_HOOKS.read().unwrap_or_else(|e| e.into_inner()).as_ref() {
            emitter.set_on_desktop_notification(Some(Arc::clone(&hooks.on_notification) as crate::notifications::emitter_runtime::DesktopNotificationCallback));
            trigger.set_get_is_window_focused(Some(Arc::clone(&hooks.is_focused)));
        }
        Arc::new(Self {
            ctx,
            events,
            settings,
            push,
            apns,
            templates,
            emitter,
            trigger,
            notification_tx,
        })
    }
}

/// 默认 reqwest 客户端：rustls + HTTP/2（APNs 直连模式需与 Apple 协商
/// h2）；builder 失败时回退 `Client::default()`，保证启动不崩溃。
fn default_http_client() -> reqwest::Client {
    // A plain rustls + HTTP/2 capable client (APNs direct mode needs h2).
    reqwest::Client::builder().build().unwrap_or_default()
}

/// `notifications::router(ctx)` — module contract entrypoint. Owns a
/// private event-stream state; see [`router_shared`] for the shared
/// composition.
/// 中文：内部新建私有 `EventStreamState`（独立事件读取者）；仅模块
/// 单独挂载时使用，组合根应调用 `router_shared` 共享同一读取者。
pub fn router(ctx: RouterContext) -> axum::Router {
    let events = EventStreamState::from_ctx(ctx.clone());
    router_shared(ctx, events)
}

/// Shared-state composition: reuse the composition root's
/// `EventStreamState` so one upstream reader feeds both the session state
/// machine and the notification triggers.
/// 中文：启动共享事件流、spawn 触发桥接任务、再挂载路由族 —— 这是
/// `server/index.js` 组合方式的对等物，返回可直接 merge 的 axum Router。
pub fn router_shared(ctx: RouterContext, events: Arc<EventStreamState>) -> axum::Router {
    let state = NotificationsState::new(ctx, events);
    state.events.ensure_started();
    spawn_trigger_bridge(Arc::clone(&state));
    routes::router(state)
}

/// Watcher bridge (`index.js` `onPayload`): every engine payload feeds the
/// session title cache and `maybeSendPushForTrigger`. The payload unwrap
/// mirrors `unwrapGlobalEventPayload` (a `payload` object wins over the
/// envelope).
/// 中文：循环消费全局 hub 广播：先解包 payload（非对象直接跳过），
/// 再刷新模板的 session 信息缓存、最后交给触发器判定；Lagged 仅告警
/// 并继续，Closed（所有发布者消失）才退出循环。
pub fn spawn_trigger_bridge(state: Arc<NotificationsState>) -> tokio::task::JoinHandle<()> {
    let mut receiver = state.events.global_hub().subscribe_events();
    let templates = Arc::clone(&state.templates);
    let trigger = Arc::clone(&state.trigger);
    tokio::spawn(async move {
        loop {
            match receiver.recv().await {
                Ok(event) => {
                    let payload = unwrap_engine_payload(&event.payload);
                    let Some(payload) = payload.filter(|payload| payload.is_object()) else {
                        continue;
                    };
                    templates.maybe_cache_session_info_from_event(&payload);
                    trigger.maybe_send_push_for_trigger(&payload).await;
                }
                Err(broadcast::error::RecvError::Lagged(dropped)) => {
                    tracing::warn!("[PushWatcher] notification bridge lagged: {dropped}");
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    })
}

/// 解包 engine 事件：顶层 `payload` 字段是对象则优先返回它；否则事件
/// 本身是对象就原样返回；两者都不满足返回 None（对齐 JS
/// `unwrapGlobalEventPayload` 的「payload 胜过信封」语义）。
fn unwrap_engine_payload(event_data: &serde_json::Value) -> Option<serde_json::Value> {
    if let Some(inner) = event_data.get("payload").filter(|inner| inner.is_object()) {
        return Some(inner.clone());
    }
    if event_data.is_object() {
        Some(event_data.clone())
    } else {
        None
    }
}

/// Engine accessor for tests building their own states.
/// 中文：仅测试使用 —— 自建状态的用例借此拿到内部 engine 引用，
/// 以便构造指向它的罐头应答表。
pub fn engine_of(state: &NotificationsState) -> &Arc<EngineState> {
    &state.ctx.engine
}
