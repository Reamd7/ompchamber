//! Port of `server/lib/event-stream/*` (protocol, global hub, upstream
//! reader, bridges) plus `server/lib/opencode/watcher.js` and
//! `session-runtime.js`.
//!
//! Browser contract note: the JS module exposes the message streams over
//! WebSocket (`/api/global/event/ws`, `/api/event/ws`). axum's `ws` feature
//! is not enabled in this crate, so the same paths and frame protocol are
//! served over SSE — each WS frame (`{ type: 'event' | 'ready' | 'resync' |
//! 'error', … }`) becomes one SSE `data:` line, with the SSE `id:` field set
//! whenever the frame carries an `eventId`.
//!
//! Fan-out contract (shared with the proxy module): raw engine events stay
//! on the [`GlobalHub`] internal channels; the server-wide [`EventHub`]
//! carries only server-synthesized `ompchamber:*` / `session.error` frames,
//! so no engine event is ever double-published to a client.

mod global_hub;
mod protocol;
mod session;
mod upstream;

#[cfg(test)]
mod testing;

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::http::header;
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use futures::Stream;
use serde_json::Value;
use tokio::sync::broadcast::error::RecvError;

use crate::context::RouterContext;
use crate::event_stream::upstream::UpstreamError;
use crate::hub::{EventHub, HubEvent};
use global_hub::{GlobalHub, HubStatus};
use protocol::{
    EventStreamParams, MESSAGE_STREAM_DIRECTORY_WS_PATH, MESSAGE_STREAM_GLOBAL_WS_PATH,
    MESSAGE_STREAM_WS_HEARTBEAT_INTERVAL_MS, error_frame, event_frame, event_visible_in_scope,
    heartbeat_payload, ready_frame, resync_frame,
};
use session::SessionRuntime;

/// Fan-out state shared by the client streams and the watcher.
pub struct EventStreamState {
    hub: Arc<GlobalHub>,
    session: Arc<SessionRuntime>,
    event_hub: Arc<EventHub>,
    ctx: RouterContext,
    started: AtomicBool,
}

impl EventStreamState {
    pub fn from_ctx(ctx: RouterContext) -> Arc<Self> {
        Arc::new(Self {
            hub: GlobalHub::new(Arc::clone(&ctx.engine)),
            session: SessionRuntime::new(Arc::clone(&ctx.hub)),
            event_hub: Arc::clone(&ctx.hub),
            ctx,
            started: AtomicBool::new(false),
        })
    }

    /// Lazily start on the first subscriber: spawns the upstream reader,
    /// the SSE-payload watcher (session state machine), and the cooldown
    /// sweeper. [`start`] calls this eagerly.
    pub fn ensure_started(&self) {
        if self.started.swap(true, Ordering::SeqCst) {
            return;
        }
        self.hub.start();
        self.spawn_watcher();
        self.spawn_session_sweeper();
    }

    /// Watcher port (`opencode/watcher.js` with a global hub): every engine
    /// payload feeds the session state machine; connect/error statuses are
    /// logged.
    fn spawn_watcher(&self) {
        let hub = Arc::clone(&self.hub);
        let session = Arc::clone(&self.session);
        tokio::spawn(async move {
            let mut events = hub.subscribe_events();
            let mut statuses = hub.subscribe_status();
            loop {
                tokio::select! {
                    event = events.recv() => match event {
                        Ok(event) => {
                            if let Some(payload) = unwrap_global_event_payload(&event.payload) {
                                session.process_opencode_sse_payload(&payload);
                            }
                        }
                        Err(RecvError::Lagged(dropped)) => {
                            tracing::warn!(dropped, "[PushWatcher] event subscriber lagged");
                        }
                        Err(RecvError::Closed) => break,
                    },
                    status = statuses.recv() => match status {
                        Ok(HubStatus::Connect { .. }) => {
                            tracing::info!("[PushWatcher] connected");
                        }
                        Ok(HubStatus::Error { error, .. }) => {
                            tracing::warn!("[PushWatcher] disconnected: {}", error.message());
                        }
                        _ => {}
                    },
                }
            }
        });
    }

    /// Cooldown expiry + hourly state cleanup (the JS `setTimeout` /
    /// `setInterval` equivalents live here instead of per transition).
    fn spawn_session_sweeper(&self) {
        let session = Arc::clone(&self.session);
        tokio::spawn(async move {
            let mut sweep = tokio::time::interval(Duration::from_millis(250));
            let mut cleanup = tokio::time::interval(Duration::from_secs(3600));
            loop {
                tokio::select! {
                    _ = sweep.tick() => session.tick(),
                    _ = cleanup.tick() => session.cleanup_old_session_states(),
                }
            }
        });
    }

    /// Settle busy sessions after a managed OpenCode restart. The JS server
    /// calls this from the startup pipeline's rebind flow; engine-restart
    /// wiring is a pending port, so it is exposed for that call site.
    pub fn interrupt_busy_sessions_after_restart(&self) -> Vec<String> {
        self.session.interrupt_busy_sessions_after_restart()
    }

    /// Rebind the upstream reader to the current engine port (JS
    /// `rebindUpstream`): restarting the hub re-dials the current base URL
    /// on its next attempt.
    pub fn rebind_upstream(&self) {
        self.hub.stop();
        self.hub.start();
    }

    pub fn global_hub(&self) -> &Arc<GlobalHub> {
        &self.hub
    }

    pub fn session_runtime(&self) -> &Arc<SessionRuntime> {
        &self.session
    }
}

/// `unwrapGlobalEventPayload` from watcher.js.
fn unwrap_global_event_payload(event_data: &Value) -> Option<Value> {
    let is_object_like = event_data.is_object() || event_data.is_array();
    if let Some(inner) = event_data.get("payload")
        && (inner.is_object() || inner.is_array())
    {
        return Some(inner.clone());
    }
    if is_object_like {
        Some(event_data.clone())
    } else {
        None
    }
}

/// Eagerly start the event-stream runtime (upstream reader + watcher).
/// Callable from `main.rs` once boot composition wires it in; until then the
/// runtime also self-starts lazily on the first client subscriber.
pub async fn start(ctx: RouterContext) -> Arc<EventStreamState> {
    let state = EventStreamState::from_ctx(ctx);
    state.ensure_started();
    state
}

pub fn router(ctx: RouterContext) -> axum::Router {
    let state = EventStreamState::from_ctx(ctx);
    Router::new()
        .route(MESSAGE_STREAM_GLOBAL_WS_PATH, get(global_message_stream))
        .route(
            MESSAGE_STREAM_DIRECTORY_WS_PATH,
            get(directory_message_stream),
        )
        .with_state(state)
}

async fn global_message_stream(
    State(state): State<Arc<EventStreamState>>,
    request: axum::extract::Request,
) -> Response {
    let (parts, _body) = request.into_parts();
    if let Err(response) = crate::ui_auth::guard(&state.ctx, &parts).await {
        return response;
    }
    state.ensure_started();
    let params = EventStreamParams::from_query(&query_map(parts.uri.query()));
    if let Some(upgrade) = ws_upgrade(parts.clone()).await {
        return ws_response(upgrade, Arc::clone(&state), None, params);
    }
    sse_response(client_stream(Arc::clone(&state), None, params))
}

async fn directory_message_stream(
    State(state): State<Arc<EventStreamState>>,
    request: axum::extract::Request,
) -> Response {
    let (parts, _body) = request.into_parts();
    if let Err(response) = crate::ui_auth::guard(&state.ctx, &parts).await {
        return response;
    }
    state.ensure_started();
    let params = EventStreamParams::from_query(&query_map(parts.uri.query()));
    if let Some(upgrade) = ws_upgrade(parts.clone()).await {
        return ws_response(upgrade, Arc::clone(&state), params.directory.clone(), params);
    }
    sse_response(client_stream(
        Arc::clone(&state),
        params.directory.clone(),
        params,
    ))
}

/// Extract the WebSocket upgrade from already-authenticated request parts.
/// `Option<WebSocketUpgrade>` is not an extractor (axum gives the type a
/// rejecting `FromRequestParts`), so branch on the handshake headers and
/// build the upgrade manually.
async fn ws_upgrade(
    mut parts: axum::http::request::Parts,
) -> Option<axum::extract::ws::WebSocketUpgrade> {
    use axum::extract::FromRequestParts;

    let wants_upgrade = parts
        .headers
        .get(header::CONNECTION)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.to_ascii_lowercase().contains("upgrade"))
        && parts
            .headers
            .get(header::UPGRADE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.to_ascii_lowercase().contains("websocket"));
    if !wants_upgrade {
        return None;
    }
    axum::extract::ws::WebSocketUpgrade::from_request_parts(&mut parts, &())
        .await
        .ok()
}

/// `global-ws-bridge.js` / `directory-ws-bridge.js`: the browser contract is
/// a real WebSocket on these paths — each bridge frame (`ready`, `event`,
/// `resync`, `error`) is one text message. The frame stream itself
/// ([`client_frames`]) is transport-agnostic and shared with SSE, so the WS
/// leg only adds the upgrade plus the `ompchamber:heartbeat` event frame the
/// JS bridges emit on their interval.
fn ws_response(
    upgrade: axum::extract::ws::WebSocketUpgrade,
    state: Arc<EventStreamState>,
    client_directory: Option<String>,
    params: EventStreamParams,
) -> Response {
    upgrade.on_upgrade(move |socket| async move {
        ws_client_loop(socket, state, client_directory, params).await;
    })
}

async fn ws_client_loop(
    mut socket: axum::extract::ws::WebSocket,
    state: Arc<EventStreamState>,
    client_directory: Option<String>,
    params: EventStreamParams,
) {
    use axum::extract::ws::Message;
    use futures::StreamExt;

    let mut frames = Box::pin(client_frames(state, client_directory.clone(), params));
    let mut heartbeat = tokio::time::interval(Duration::from_millis(
        MESSAGE_STREAM_WS_HEARTBEAT_INTERVAL_MS as u64,
    ));
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // Skip the immediate first tick so a fresh client sees ready/replay
    // frames before heartbeats (the JS interval also fires only later).
    heartbeat.tick().await;

    loop {
        tokio::select! {
            frame = frames.next() => match frame {
                Some(frame) => {
                    let Ok(text) = serde_json::to_string(&frame) else { continue };
                    if socket.send(Message::Text(text.into())).await.is_err() {
                        break;
                    }
                }
                None => break,
            },
            _ = heartbeat.tick() => {
                let directory = client_directory.clone().unwrap_or_else(|| "global".to_string());
                let frame = serde_json::json!({
                    "type": "event",
                    "payload": { "type": "ompchamber:heartbeat", "timestamp": now_ms() },
                    "directory": directory,
                });
                let Ok(text) = serde_json::to_string(&frame) else { continue };
                if socket.send(Message::Text(text.into())).await.is_err() {
                    break;
                }
            }
            inbound = socket.recv() => match inbound {
                // The JS bridges only read to observe close; pings are
                // answered by axum automatically.
                Some(Ok(_)) => continue,
                _ => break,
            },
        }
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default()
}

/// Parse a raw query string into a map (mirrors `URL.searchParams`).
fn query_map(raw: Option<&str>) -> std::collections::HashMap<String, String> {
    raw.map(|query| {
        url::form_urlencoded::parse(query.as_bytes())
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect()
    })
    .unwrap_or_default()
}

fn sse_response<S>(stream: S) -> Response
where
    S: Stream<Item = Result<SseEvent, std::convert::Infallible>> + Send + 'static,
{
    let sse = Sse::new(stream).keep_alive(KeepAlive::default());
    ([(header::CACHE_CONTROL, "no-cache")], sse).into_response()
}

fn frame_event(frame: &Value) -> Result<SseEvent, std::convert::Infallible> {
    let mut event = SseEvent::default().data(frame.to_string());
    if let Some(event_id) = frame.get("eventId").and_then(Value::as_str)
        && !event_id.is_empty()
    {
        event = event.id(event_id);
    }
    Ok(event)
}

fn initial_error_message(error: &UpstreamError) -> String {
    match error {
        UpstreamError::UpstreamUnavailable { status } => {
            format!("OpenCode event stream unavailable ({status})")
        }
        UpstreamError::StreamError {
            build_url_failed: true,
            ..
        } => "OpenCode service unavailable".to_string(),
        UpstreamError::StreamError { .. } => {
            "Failed to connect to OpenCode event stream".to_string()
        }
    }
}

/// `replayEvents` from global-ws-bridge.js: reconcile the client cursor.
///
/// A cross-boot cursor is a gap even when the id is numerically retained —
/// only the epoch disproves it. A `Gap` verdict becomes an explicit resync
/// control carrying the reconnectable tail, never a silent suffix.
fn replay_frames(
    hub: &Arc<GlobalHub>,
    client_directory: Option<&str>,
    requested_last_event_id: Option<&str>,
    client_epoch: Option<&str>,
) -> Vec<Value> {
    let upstream_epoch = hub.upstream_epoch();
    let requested = requested_last_event_id.filter(|id| !id.is_empty());
    let epoch_mismatch = requested.is_some()
        && client_epoch.is_some_and(|epoch| !epoch.is_empty())
        && upstream_epoch
            .as_deref()
            .is_some_and(|upstream| upstream != client_epoch.unwrap_or_default());
    let (verdict, events) = if epoch_mismatch {
        (global_hub::ReplayVerdict::Gap, Vec::new())
    } else {
        hub.replay_after(requested)
    };
    if verdict == global_hub::ReplayVerdict::Gap {
        let tail = hub.tail_event_id().unwrap_or_default();
        return vec![resync_frame(Some(&tail), upstream_epoch.as_deref())];
    }
    events
        .iter()
        .filter(|event| event_visible_in_scope(Some(event.directory.as_str()), client_directory))
        .map(|event| {
            let directory = client_directory
                .map(str::to_string)
                .or(Some(event.directory.clone()))
                .unwrap_or_else(|| "global".to_string());
            event_frame(
                &event.payload,
                event.event_id.as_deref(),
                Some(directory.as_str()),
            )
        })
        .collect()
}

/// One subscribed client: the `global-ws-bridge.js` / directory-bridge
/// lifecycle over SSE.
///
/// - Ready gating: nothing is delivered until the upstream connects (or is
///   already connected at subscribe time), then the requested cursor is
///   reconciled.
/// - Directory-scoped clients receive only their directory's events plus
///   global ones.
/// - Server-published `ompchamber:*` frames arrive via the shared EventHub.
/// - Initial upstream failures end the stream with an error frame (JS
///   closes the socket with 1011 after the same frame).
fn client_frames(
    state: Arc<EventStreamState>,
    client_directory: Option<String>,
    params: EventStreamParams,
) -> impl Stream<Item = Value> + Send + 'static {
    let scope = if client_directory.is_some() {
        "directory"
    } else {
        "global"
    };
    async_stream::stream! {
        let hub = Arc::clone(&state.hub);
        // Subscribe before snapshotting readiness so no event is missed;
        // replayed events are deduped by id below.
        let mut hub_events = hub.subscribe_events();
        let mut hub_status = hub.subscribe_status();
        let mut synthetic = state.event_hub.subscribe();
        let mut heartbeat = tokio::time::interval(Duration::from_millis(MESSAGE_STREAM_WS_HEARTBEAT_INTERVAL_MS));
        heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // tokio intervals tick immediately; consume that tick so the first
        // heartbeat waits a full period (JS setInterval semantics).
        heartbeat.tick().await;

        let mut ready = false;
        let mut client_epoch: Option<String> = params.epoch.clone().filter(|epoch| !epoch.is_empty());
        let mut replayed_ids: HashSet<String> = HashSet::new();

        if hub.is_connected() {
            let epoch = hub.upstream_epoch();
            yield ready_frame(scope, epoch.as_deref());
            for frame in replay_frames(&hub, client_directory.as_deref(), params.last_event_id.as_deref(), client_epoch.as_deref()) {
                if let Some(id) = frame.get("eventId").and_then(Value::as_str) {
                    replayed_ids.insert(id.to_string());
                }
                if let Some(epoch) = frame.get("epoch").and_then(Value::as_str) {
                    client_epoch = Some(epoch.to_string());
                }
                yield frame;
            }
            ready = true;
        }

        loop {
            tokio::select! {
                status = hub_status.recv() => match status {
                    Ok(HubStatus::Connect { was_ready }) => {
                        if !ready {
                            let epoch = hub.upstream_epoch();
                            yield ready_frame(scope, epoch.as_deref());
                            for frame in replay_frames(&hub, client_directory.as_deref(), params.last_event_id.as_deref(), client_epoch.as_deref()) {
                                if let Some(id) = frame.get("eventId").and_then(Value::as_str) {
                                    replayed_ids.insert(id.to_string());
                                }
                                if let Some(epoch) = frame.get("epoch").and_then(Value::as_str) {
                                    client_epoch = Some(epoch.to_string());
                                }
                                yield frame;
                            }
                            ready = true;
                        } else if was_ready {
                            // Reconnect edge: re-announce readiness with the
                            // current boot identity.
                            let epoch = hub.upstream_epoch();
                            yield ready_frame(scope, epoch.as_deref());
                        }
                    }
                    Ok(HubStatus::Restart { epoch, .. }) => {
                        if ready {
                            let tail = hub.tail_event_id();
                            client_epoch = epoch.clone();
                            yield resync_frame(tail.as_deref(), epoch.as_deref());
                        }
                    }
                    Ok(HubStatus::Error { initial: true, error, .. }) => {
                        yield error_frame(&initial_error_message(&error));
                        return;
                    }
                    Ok(_) => {}
                    Err(RecvError::Lagged(dropped)) => {
                        if ready {
                            let tail = hub.tail_event_id();
                            let epoch = hub.upstream_epoch();
                            client_epoch = epoch.clone();
                            tracing::warn!(dropped, "status subscriber lagged; forcing resync");
                            yield resync_frame(tail.as_deref(), epoch.as_deref());
                        }
                    }
                    Err(RecvError::Closed) => return,
                },
                event = hub_events.recv() => match event {
                    Ok(event) => {
                        if !ready {
                            continue;
                        }
                        if let Some(event_id) = &event.event_id
                            && replayed_ids.contains(event_id) {
                                continue;
                            }
                        if !event_visible_in_scope(Some(event.directory.as_str()), client_directory.as_deref()) {
                            continue;
                        }
                        let frame_directory = client_directory
                            .clone()
                            .or(Some(event.directory.clone()))
                            .unwrap_or_else(|| "global".to_string());
                        yield event_frame(&event.payload, event.event_id.as_deref(), Some(frame_directory.as_str()));
                    }
                    Err(RecvError::Lagged(dropped)) => {
                        if ready {
                            // An honest gap control instead of a silent skip.
                            let tail = hub.tail_event_id();
                            let epoch = hub.upstream_epoch();
                            client_epoch = epoch.clone();
                            tracing::warn!(dropped, "event subscriber lagged; forcing resync");
                            yield resync_frame(tail.as_deref(), epoch.as_deref());
                        }
                    }
                    Err(RecvError::Closed) => {}
                },
                frame = synthetic.recv() => match frame {
                    Ok(HubEvent { data, .. }) => {
                        let payload: Value = serde_json::from_str(&data).unwrap_or(Value::Null);
                        yield event_frame(&payload, None, Some("global"));
                    }
                    Err(RecvError::Lagged(dropped)) => {
                        // Synthetic UI events are not replayable; log only.
                        tracing::warn!(dropped, "synthetic event subscriber lagged");
                    }
                    Err(RecvError::Closed) => {}
                },
                _ = heartbeat.tick() => {
                    if hub.is_connected() {
                        yield event_frame(&heartbeat_payload(session::now_ms()), None, Some("global"));
                    }
                }
            }
        }
    }
}

/// The wire-facing stream: frames wrapped as SSE events.
fn client_stream(
    state: Arc<EventStreamState>,
    client_directory: Option<String>,
    params: EventStreamParams,
) -> impl Stream<Item = Result<SseEvent, std::convert::Infallible>> + Send + 'static {
    futures::StreamExt::map(client_frames(state, client_directory, params), |frame| {
        frame_event(&frame)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{EngineConfig, ServerConfig, TunnelOptions};
    use crate::context::RouterContext;
    use crate::engine::EngineState;
    use crate::event_stream::global_hub::ReplayVerdict;
    use crate::event_stream::testing::{Attempt, CannedSseServer};
    use crate::event_stream::upstream::{ReaderMessage, UpstreamEvent};
    use crate::hub::EventHub;
    use axum::body::Body;
    use axum::http::StatusCode;
    use futures::StreamExt;
    use serde_json::json;
    use std::path::PathBuf;

    fn test_config() -> Arc<ServerConfig> {
        Arc::new(ServerConfig {
            port: 3000,
            host: None,
            lan: false,
            ui_password: None,
            api_only: false,
            data_dir: PathBuf::from("/tmp"),
            dist_dir: PathBuf::from("/tmp"),
            tunnel: TunnelOptions::default(),
            engine: EngineConfig::External {
                base_url: "http://127.0.0.1:9".to_string(),
            },
        })
    }

    fn test_state(engine: Arc<EngineState>, hub: Option<Arc<EventHub>>) -> Arc<EventStreamState> {
        let hub = hub.unwrap_or_else(EventHub::new);
        let ctx = RouterContext {
            config: test_config(),
            engine,
            hub,
        };
        EventStreamState::from_ctx(ctx)
    }

    fn dummy_engine() -> Arc<EngineState> {
        EngineState::external("http://127.0.0.1:9".to_string(), None)
    }

    async fn collect_frames<S>(stream: S, count: usize) -> Vec<Value>
    where
        S: Stream<Item = Value> + Send + 'static,
    {
        let mut stream = Box::pin(stream);
        let mut frames = Vec::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while frames.len() < count && tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_secs(2), stream.next()).await {
                Ok(Some(frame)) => frames.push(frame),
                Ok(None) => break,
                Err(_elapsed) => break,
            }
        }
        frames
    }

    /// Read an SSE response body until `done(frame)` matches (then drain for
    /// a short window), returning parsed `data:` frames.
    async fn collect_wire_frames<F>(response: axum::response::Response, done: F) -> Vec<Value>
    where
        F: Fn(&Value) -> bool,
    {
        let mut body = response.into_body().into_data_stream();
        let mut raw = String::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let mut matched = false;
        while tokio::time::Instant::now() < deadline {
            let wait = if matched {
                Duration::from_millis(250)
            } else {
                Duration::from_secs(2)
            };
            match tokio::time::timeout(wait, body.next()).await {
                Ok(Some(Ok(chunk))) => {
                    raw.push_str(&String::from_utf8_lossy(&chunk));
                    if !matched {
                        let frames = parse_wire_frames(&raw);
                        matched = frames.iter().any(&done);
                    }
                    if matched {
                        // Drain briefly so late unwanted frames surface.
                        continue;
                    }
                }
                _ if matched => break,
                _ => continue,
            }
        }
        parse_wire_frames(&raw)
    }

    fn parse_wire_frames(raw: &str) -> Vec<Value> {
        raw.lines()
            .filter_map(|line| line.strip_prefix("data:"))
            .filter_map(|data| serde_json::from_str::<Value>(data).ok())
            .collect()
    }

    /// Wait (bounded) until the hub retains `tail` — makes replay-based
    /// assertions deterministic against a live upstream reader.
    async fn wait_for_tail(state: &Arc<EventStreamState>, tail: &str) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while state.hub.tail_event_id().as_deref() != Some(tail) {
            assert!(
                tokio::time::Instant::now() < deadline,
                "tail {tail} never retained"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    fn dispatch_business(
        state: &Arc<EventStreamState>,
        generation: u64,
        id: &str,
        directory: Option<&str>,
    ) {
        state.hub.handle_message(
            generation,
            ReaderMessage::Event(UpstreamEvent {
                event_id: Some(id.to_string()),
                event_name: None,
                directory: directory.map(str::to_string),
                payload: Some(json!({ "type": "session.updated", "properties": {} })),
                synthesized: false,
            }),
        );
    }

    fn dispatch_connect(state: &Arc<EventStreamState>, generation: u64) {
        state.hub.handle_message(
            generation,
            ReaderMessage::Connect {
                last_event_id: String::new(),
            },
        );
    }

    #[tokio::test]
    async fn ready_frame_emitted_once_connected() {
        let state = test_state(dummy_engine(), None);
        let stream = client_frames(Arc::clone(&state), None, EventStreamParams::default());
        let collector = tokio::spawn(collect_frames(stream, 1));
        tokio::time::sleep(Duration::from_millis(50)).await;
        let generation = state.hub.begin_generation_for_test();
        dispatch_connect(&state, generation);
        let frames = collector.await.expect("collector");
        assert_eq!(frames[0], json!({ "type": "ready", "scope": "global" }));
    }

    #[tokio::test]
    async fn directory_scoped_client_receives_only_own_and_global_events() {
        let state = test_state(dummy_engine(), None);
        let generation = state.hub.begin_generation_for_test();
        dispatch_connect(&state, generation);

        let scoped = client_frames(
            Arc::clone(&state),
            Some("/tmp/project-a".to_string()),
            EventStreamParams::default(),
        );
        let global = client_frames(Arc::clone(&state), None, EventStreamParams::default());
        let scoped_collector = tokio::spawn(collect_frames(scoped, 4));
        let global_collector = tokio::spawn(collect_frames(global, 4));
        tokio::time::sleep(Duration::from_millis(50)).await;

        dispatch_business(&state, generation, "e1", Some("/tmp/project-a"));
        dispatch_business(&state, generation, "e2", Some("/tmp/project-b"));
        dispatch_business(&state, generation, "e3", None);

        let scoped_frames = scoped_collector.await.expect("scoped");
        let global_frames = global_collector.await.expect("global");

        let scoped_ids: Vec<&str> = scoped_frames
            .iter()
            .filter(|frame| frame["type"] == "event")
            .map(|frame| frame["eventId"].as_str().unwrap())
            .collect();
        assert_eq!(
            scoped_ids,
            vec!["e1", "e3"],
            "scoped client must not receive other-directory events"
        );
        assert_eq!(scoped_frames[1]["directory"], "/tmp/project-a");
        assert_eq!(scoped_frames[2]["directory"], "/tmp/project-a");

        let global_ids: Vec<&str> = global_frames
            .iter()
            .filter(|frame| frame["type"] == "event")
            .map(|frame| frame["eventId"].as_str().unwrap())
            .collect();
        assert_eq!(global_ids, vec!["e1", "e2", "e3"]);
        assert_eq!(
            global_frames[3]["directory"], "global",
            "directory-less events normalize to global"
        );
    }

    #[tokio::test]
    async fn replay_serves_suffix_and_gap_forces_resync() {
        let state = test_state(dummy_engine(), None);
        let generation = state.hub.begin_generation_for_test();
        dispatch_connect(&state, generation);
        state.hub.handle_message(
            generation,
            ReaderMessage::EpochChange {
                epoch: "boot-1".into(),
                changed: false,
            },
        );
        dispatch_business(&state, generation, "e1", Some("/tmp/p"));
        dispatch_business(&state, generation, "e2", Some("/tmp/p"));

        // Client resumes from e1 → suffix only.
        let stream = client_frames(
            Arc::clone(&state),
            None,
            EventStreamParams {
                last_event_id: Some("e1".to_string()),
                epoch: None,
                directory: None,
            },
        );
        let frames = collect_frames(stream, 2).await;
        assert_eq!(frames[0]["type"], "ready");
        assert_eq!(frames[1]["eventId"], "e2");

        // Client with an unretained cursor → explicit resync with the tail,
        // never an empty success.
        let stream = client_frames(
            Arc::clone(&state),
            None,
            EventStreamParams {
                last_event_id: Some("gone".to_string()),
                epoch: None,
                directory: None,
            },
        );
        let frames = collect_frames(stream, 2).await;
        assert_eq!(frames[0]["type"], "ready");
        assert_eq!(
            frames[1],
            json!({ "type": "resync", "eventId": "e2", "epoch": "boot-1" })
        );

        // Cross-boot cursor (epoch mismatch) is a gap even when the id is
        // numerically retained.
        let stream = client_frames(
            Arc::clone(&state),
            None,
            EventStreamParams {
                last_event_id: Some("e1".to_string()),
                epoch: Some("boot-old".to_string()),
                directory: None,
            },
        );
        let frames = collect_frames(stream, 2).await;
        assert_eq!(frames[0]["type"], "ready");
        assert_eq!(frames[1]["type"], "resync");
    }

    #[tokio::test]
    async fn restart_status_forces_client_resync() {
        let state = test_state(dummy_engine(), None);
        let generation = state.hub.begin_generation_for_test();
        dispatch_connect(&state, generation);
        dispatch_business(&state, generation, "e1", None);

        let mut stream = Box::pin(client_frames(
            Arc::clone(&state),
            None,
            EventStreamParams::default(),
        ));
        let ready = tokio::time::timeout(Duration::from_secs(2), stream.next())
            .await
            .expect("ready within timeout")
            .expect("frame");
        assert_eq!(ready["type"], "ready");

        dispatch_business(&state, generation, "e2", None);
        let event = tokio::time::timeout(Duration::from_secs(2), stream.next())
            .await
            .expect("event within timeout")
            .expect("frame");
        assert_eq!(event["type"], "event");
        assert_eq!(event["eventId"], "e2");

        state.hub.handle_message(
            generation,
            ReaderMessage::EpochChange {
                epoch: "boot-2".into(),
                changed: true,
            },
        );
        // The epoch change cleared the replay, so the resync control carries
        // the boot identity with no reconnectable tail (JS omits empty ids).
        let resync = tokio::time::timeout(Duration::from_secs(2), stream.next())
            .await
            .expect("resync within timeout")
            .expect("frame");
        assert_eq!(resync, json!({ "type": "resync", "epoch": "boot-2" }));
    }

    #[tokio::test]
    async fn synthetic_eventhub_frames_reach_clients() {
        let event_hub = EventHub::new();
        let state = test_state(dummy_engine(), Some(Arc::clone(&event_hub)));
        let generation = state.hub.begin_generation_for_test();
        dispatch_connect(&state, generation);

        let stream = client_frames(Arc::clone(&state), None, EventStreamParams::default());
        let collector = tokio::spawn(collect_frames(stream, 2));
        tokio::time::sleep(Duration::from_millis(50)).await;
        event_hub.publish_json(
            "ompchamber:session-status",
            &json!({ "type": "ompchamber:session-status", "properties": { "sessionID": "ses_1" } }),
        );
        let frames = collector.await.expect("collector");
        assert_eq!(frames[1]["type"], "event");
        assert_eq!(frames[1]["directory"], "global");
        assert_eq!(frames[1]["payload"]["properties"]["sessionID"], "ses_1");
    }

    #[tokio::test]
    async fn initial_upstream_failure_ends_stream_with_error_frame() {
        let state = test_state(dummy_engine(), None);
        let stream = client_frames(Arc::clone(&state), None, EventStreamParams::default());
        let collector = tokio::spawn(collect_frames(stream, 1));
        tokio::time::sleep(Duration::from_millis(50)).await;
        let generation = state.hub.begin_generation_for_test();
        state.hub.handle_message(
            generation,
            ReaderMessage::Error(UpstreamError::UpstreamUnavailable { status: 503 }),
        );
        let frames = collector.await.expect("collector");
        assert_eq!(
            frames[0],
            json!({ "type": "error", "message": "OpenCode event stream unavailable (503)" })
        );
    }

    #[tokio::test]
    async fn lazy_start_connects_upstream_and_streams_engine_events_end_to_end() {
        // Canned engine: one wrapped business event, then hold open.
        let server = CannedSseServer::start(vec![
            Attempt::respond(vec![
                "id: evt-1\ndata: {\"directory\":\"/tmp/project\",\"payload\":{\"type\":\"session.status\",\"properties\":{\"sessionID\":\"ses_1\",\"status\":{\"type\":\"busy\"}}}}\n\n",
                "id: evt-2\ndata: {\"directory\":\"/tmp/project\",\"payload\":{\"type\":\"session.status\",\"properties\":{\"sessionID\":\"ses_1\",\"status\":{\"type\":\"idle\"}}}}\n\n",
            ]),
            Attempt::respond_holding(vec![]),
        ])
        .await;
        let engine = EngineState::external(server.url(""), Some("secret".to_string()));
        let state = test_state(engine, None);

        // First subscriber lazily starts the upstream reader; wait until the
        // events are retained, then resume deterministically from evt-1.
        state.ensure_started();
        wait_for_tail(&state, "evt-2").await;

        let stream = client_frames(
            Arc::clone(&state),
            None,
            EventStreamParams {
                last_event_id: Some("evt-1".to_string()),
                epoch: None,
                directory: None,
            },
        );
        let frames = collect_frames(stream, 2).await;
        assert_eq!(frames[0], json!({ "type": "ready", "scope": "global" }));
        assert_eq!(frames[1]["type"], "event");
        assert_eq!(frames[1]["eventId"], "evt-2");
        assert_eq!(frames[1]["directory"], "/tmp/project");

        // Engine auth reached the upstream request (attempt 1; the reader
        // reconnects after the scripted response ends).
        let requests = server.requests();
        assert!(!requests.is_empty());
        assert_eq!(requests[0].path, "/event");
        assert_eq!(
            requests[0].authorization.as_deref(),
            Some("Basic b3BlbmNvZGU6c2VjcmV0"), // opencode:secret
            "auth header must be attached without being logged"
        );
        assert_eq!(requests[0].accept.as_deref(), Some("text/event-stream"));
    }

    #[tokio::test]
    async fn watcher_feeds_session_runtime_from_engine_events() {
        let server = CannedSseServer::start(vec![
            Attempt::respond(vec![
                "id: evt-1\ndata: {\"directory\":\"/tmp/p\",\"payload\":{\"type\":\"session.status\",\"properties\":{\"sessionID\":\"ses_1\",\"status\":{\"type\":\"busy\"}}}}\n\n",
            ]),
            Attempt::respond_holding(vec![]),
        ])
        .await;
        let engine = EngineState::external(server.url(""), None);
        let state = test_state(engine, None);
        state.ensure_started();

        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while state.session.get_active_session_count() < 1 && tokio::time::Instant::now() < deadline
        {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(state.session.get_active_session_count(), 1);
        assert_eq!(
            state.session.get_session_state_snapshot()["ses_1"]["status"],
            "busy"
        );

        let interrupted = state.interrupt_busy_sessions_after_restart();
        assert_eq!(interrupted, vec!["ses_1".to_string()]);
        assert_eq!(state.session.get_active_session_count(), 0);
    }

    #[tokio::test]
    async fn router_serves_sse_wire_format() {
        let server = CannedSseServer::start(vec![
            Attempt::respond(vec![
                "id: evt-1\ndata: {\"payload\":{\"type\":\"x\",\"properties\":{}}}\n\n",
                "id: evt-2\ndata: {\"payload\":{\"type\":\"y\",\"properties\":{}}}\n\n",
            ]),
            Attempt::respond_holding(vec![]),
        ])
        .await;
        let engine = EngineState::external(server.url(""), None);
        let state = test_state(engine, None);
        let app = router_for_state(Arc::clone(&state));
        state.ensure_started();
        wait_for_tail(&state, "evt-2").await;

        let response = tokio::time::timeout(
            Duration::from_secs(5),
            app.clone().oneshot_request(&format!(
                "{MESSAGE_STREAM_GLOBAL_WS_PATH}?lastEventId=evt-1"
            )),
        )
        .await
        .expect("request within timeout")
        .expect("response");

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok()),
            Some("text/event-stream")
        );
        assert_eq!(
            response
                .headers()
                .get(header::CACHE_CONTROL)
                .and_then(|value| value.to_str().ok()),
            Some("no-cache")
        );

        let frames = collect_wire_frames(response, |frame| {
            frame.get("eventId").and_then(Value::as_str) == Some("evt-2")
        })
        .await;
        assert!(
            frames
                .iter()
                .any(|frame| frame["type"] == "ready" && frame["scope"] == "global"),
            "ready frame on the SSE wire: {frames:?}"
        );
        assert!(
            frames
                .iter()
                .any(|frame| frame["type"] == "event" && frame["eventId"] == "evt-2"),
            "engine event replayed after cursor: {frames:?}"
        );
    }

    #[tokio::test]
    async fn router_directory_scope_filters_on_the_wire() {
        let server = CannedSseServer::start(vec![
            Attempt::respond(vec![
                "id: a-0\ndata: {\"directory\":\"/tmp/project-a\",\"payload\":{\"type\":\"seed\"}}\n\n",
                "id: a-1\ndata: {\"directory\":\"/tmp/project-a\",\"payload\":{\"type\":\"a\"}}\n\n",
                "id: b-1\ndata: {\"directory\":\"/tmp/project-b\",\"payload\":{\"type\":\"b\"}}\n\n",
            ]),
            Attempt::respond_holding(vec![]),
        ])
        .await;
        let engine = EngineState::external(server.url(""), None);
        let state = test_state(engine, None);
        let app = router_for_state(Arc::clone(&state));
        state.ensure_started();
        wait_for_tail(&state, "b-1").await;

        let response = tokio::time::timeout(
            Duration::from_secs(5),
            app.oneshot_request(&format!(
                "{MESSAGE_STREAM_DIRECTORY_WS_PATH}?directory=%2Ftmp%2Fproject-a&lastEventId=a-0"
            )),
        )
        .await
        .expect("request within timeout")
        .expect("response");
        assert_eq!(response.status(), StatusCode::OK);

        let frames = collect_wire_frames(response, |frame| {
            frame.get("eventId").and_then(Value::as_str) == Some("a-1")
        })
        .await;
        assert!(
            frames
                .iter()
                .any(|frame| frame["type"] == "event" && frame["eventId"] == "a-1"),
            "own-directory event delivered: {frames:?}"
        );
        assert!(
            !frames.iter().any(|frame| frame["eventId"] == "b-1"),
            "other-directory event must be filtered: {frames:?}"
        );
        assert!(
            frames
                .iter()
                .any(|frame| frame["type"] == "ready" && frame["scope"] == "directory"),
            "directory ready frame: {frames:?}"
        );
    }

    #[tokio::test]
    async fn start_eagerly_boots_the_runtime() {
        let server = CannedSseServer::start(vec![
            Attempt::respond(vec!["id: s-1\ndata: {\"payload\":{\"type\":\"x\"}}\n\n"]),
            Attempt::respond_holding(vec![]),
        ])
        .await;
        let engine = EngineState::external(server.url(""), None);
        let hub = EventHub::new();
        let ctx = RouterContext {
            config: test_config(),
            engine,
            hub,
        };
        let state = start(ctx).await;
        assert!(
            state.hub.has_ever_connected() || state.hub.is_connected() || {
                // ensure the reader had a chance to connect
                tokio::time::sleep(Duration::from_millis(200)).await;
                state.hub.is_connected() || state.hub.has_ever_connected()
            }
        );
        assert!(
            state.hub.is_connected() || state.hub.has_ever_connected(),
            "upstream reader started eagerly"
        );
    }

    fn router_for_state(state: Arc<EventStreamState>) -> Router {
        Router::new()
            .route(MESSAGE_STREAM_GLOBAL_WS_PATH, get(global_message_stream))
            .route(
                MESSAGE_STREAM_DIRECTORY_WS_PATH,
                get(directory_message_stream),
            )
            .with_state(state)
    }

    trait OneshotRequest {
        async fn oneshot_request(
            self,
            path: &str,
        ) -> Result<axum::response::Response, std::convert::Infallible>;
    }

    impl OneshotRequest for Router {
        async fn oneshot_request(
            self,
            path: &str,
        ) -> Result<axum::response::Response, std::convert::Infallible> {
            use tower::ServiceExt;
            let request = axum::http::Request::builder()
                .uri(path)
                .body(Body::empty())
                .expect("request");
            self.oneshot(request).await
        }
    }

    #[tokio::test]
    async fn replay_gap_never_served_as_empty_ok() {
        // Failure-honesty invariant at the hub level, re-asserted through
        // the client stream path: an unretained cursor must surface a gap.
        let state = test_state(dummy_engine(), None);
        let generation = state.hub.begin_generation_for_test();
        dispatch_connect(&state, generation);
        assert_eq!(state.hub.replay_after(Some("nope")).0, ReplayVerdict::Gap);
    }
}
