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

#[cfg(test)]
mod trigger_tests;

/// Serializes env-mutating tests across the whole notifications module
/// (parallel test threads share the process environment).
#[cfg(test)]
pub(crate) static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub mod apns_runtime;
pub mod crypto;
pub mod emitter_runtime;
pub mod message;
pub mod push_runtime;
pub mod routes;
pub mod template_runtime;
pub mod transport;
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
pub struct DesktopHooks {
    pub on_notification: Arc<dyn Fn(&serde_json::Value) + Send + Sync>,
    pub is_focused: Arc<dyn Fn() -> bool + Send + Sync>,
}

static DESKTOP_HOOKS: std::sync::RwLock<Option<DesktopHooks>> = std::sync::RwLock::new(None);

pub fn set_desktop_hooks(hooks: DesktopHooks) {
    *DESKTOP_HOOKS.write().unwrap_or_else(|e| e.into_inner()) = Some(hooks);
}

/// Test teardown: the control-channel test registers process-global hooks
/// that would otherwise leak "window focused" into other notification tests.
#[cfg(test)]
pub fn clear_desktop_hooks() {
    *DESKTOP_HOOKS.write().unwrap_or_else(|e| e.into_inner()) = None;
}

pub struct NotificationsState {
    pub ctx: RouterContext,
    pub events: Arc<EventStreamState>,
    pub settings: Arc<SettingsStore>,
    pub push: Arc<PushRuntime>,
    pub apns: Arc<ApnsRuntime>,
    pub templates: Arc<TemplateRuntime>,
    pub emitter: Arc<EmitterRuntime>,
    pub trigger: Arc<TriggerRuntime>,
    /// Serialized synthetic payloads for `/api/notifications/stream`
    /// clients (the JS `uiNotificationClients` set).
    pub notification_tx: broadcast::Sender<String>,
}

impl NotificationsState {
    pub fn new(ctx: RouterContext, events: Arc<EventStreamState>) -> Arc<Self> {
        Self::new_with_transport(ctx, events, reqwest_post(default_http_client()))
    }

    pub fn new_with_transport(
        ctx: RouterContext,
        events: Arc<EventStreamState>,
        transport: HttpPost,
    ) -> Arc<Self> {
        let fetch = engine_json_fetch(Arc::clone(&ctx.engine));
        Self::new_with(ctx, events, transport, fetch)
    }

    /// Full-injection constructor (tests drive the engine seam).
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

fn default_http_client() -> reqwest::Client {
    // A plain rustls + HTTP/2 capable client (APNs direct mode needs h2).
    reqwest::Client::builder().build().unwrap_or_default()
}

/// `notifications::router(ctx)` — module contract entrypoint. Owns a
/// private event-stream state; see [`router_shared`] for the shared
/// composition.
pub fn router(ctx: RouterContext) -> axum::Router {
    let events = EventStreamState::from_ctx(ctx.clone());
    router_shared(ctx, events)
}

/// Shared-state composition: reuse the composition root's
/// `EventStreamState` so one upstream reader feeds both the session state
/// machine and the notification triggers.
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
pub fn engine_of(state: &NotificationsState) -> &Arc<EngineState> {
    &state.ctx.engine
}
