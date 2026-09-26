//! Port of `server/lib/relay/host-client.js` — long-lived relay host client.
//!
//! Maintains the signed `host-control` socket to the relay, and per connected
//! client a signed `host-data` socket that runs the responder E2EE handshake
//! and feeds decrypted frames into a tunnel-host dispatcher (spec Layer 1).

use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use tokio::sync::{mpsc, watch};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

use super::e2ee::{CLOSE_CHANNEL_FAILURE, Channel, HostHandshake, RELAY_PROTOCOL_VERSION};
use super::identity::RelayIdentity;
use super::tunnel_codec::{OutboundFrameBatcher, decode_frame_batch};
use super::tunnel_host::{BODY_DELIVERY_TIMEOUT_MS, TunnelHost, TunnelHostDeps};

const BACKOFF_BASE_MS: u64 = 1000;
const BACKOFF_CAP_MS: u64 = 30_000;
const DATA_SOCKET_OPEN_TIMEOUT_MS: u64 = 15_000;
// Clients send a tunnel Ping at least every ~30s when idle, so a data socket
// with no inbound traffic for 3 ping intervals belongs to a client that died
// without a WebSocket close (network loss, battery kill). The relay worker may
// not notice the dead client leg for a long time, so the host must reap these
// itself — both to free resources and to keep the "N devices connected" status
// honest instead of counting ghosts.
const DATA_SOCKET_IDLE_TIMEOUT_MS: u64 = 90_000;
const DATA_SOCKET_IDLE_SWEEP_INTERVAL_MS: u64 = 30_000;
// Protocol-level keepalive for the control socket. Without it, a network path
// that dies silently (NAT timeout, relay-edge eviction without close frames)
// leaves the host believing it is registered while the relay has forgotten it —
// every client tunnel then hangs in `connecting` forever. A missed pong window
// terminates the socket, which drives the normal reconnect + re-registration.
const CONTROL_PING_INTERVAL_MS: u64 = 30_000;
const CONTROL_PONG_GRACE_MS: u64 = 10_000;
const DEFAULT_BATCH_WINDOW_MS: u64 = 150;

/// Resolve the frame-batching flush window: explicit option wins, then env,
/// then the 150 ms default. Only applies on directions where batching was
/// negotiated.
fn resolve_batch_window_ms(option: Option<u64>) -> u64 {
    if let Some(value) = option {
        if value < u64::MAX {
            return value;
        }
    }
    if let Ok(raw) = std::env::var("OMPCHAMBER_RELAY_BATCH_WINDOW_MS") {
        if let Ok(value) = raw.trim().parse::<u64>() {
            return value;
        }
    }
    DEFAULT_BATCH_WINDOW_MS
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RelayHostStatus {
    pub state: String,
    pub last_error: Option<String>,
    pub connected_clients: usize,
}

pub type StatusFn = Arc<dyn Fn(RelayHostStatus) + Send + Sync>;
pub type GetLocalPortFn = Arc<dyn Fn() -> u16 + Send + Sync>;

pub struct RelayHostOptions {
    pub relay_url: String,
    pub identity: Arc<RelayIdentity>,
    pub get_local_port: GetLocalPortFn,
    pub on_status: Option<StatusFn>,
    pub batch_window_ms: Option<u64>,
    /// `batch !== false` in JS; default true.
    pub batch: bool,
}

enum OutboundMsg {
    Text(String),
    Binary(Vec<u8>),
    Close(u16, String),
}

struct DataSocketEntry {
    outbound_tx: mpsc::UnboundedSender<OutboundMsg>,
    /// Approximates `socket.bufferedAmount`: bytes handed to the writer but
    /// not yet written to the socket.
    buffered_bytes: Arc<AtomicU64>,
    tunnel: Option<Arc<TunnelHost>>,
    batcher: Option<OutboundFrameBatcher>,
    last_activity_ms: AtomicI64,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

struct Inner {
    relay_url: String,
    identity: Arc<RelayIdentity>,
    get_local_port: GetLocalPortFn,
    on_status: Option<StatusFn>,
    resolved_batch_window_ms: u64,
    local_batch: bool,
    status: Mutex<RelayHostStatus>,
    stopped: watch::Sender<bool>,
    data_sockets: Mutex<HashMap<String, DataSocketEntry>>,
}

/// The long-lived relay host client (JS `startRelayHost`).
pub struct RelayHostClient {
    inner: Arc<Inner>,
    supervisor: tokio::task::JoinHandle<()>,
    sweeper: tokio::task::JoinHandle<()>,
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// JS `URL.searchParams.set(...)`: replace the first occurrence, drop later
/// duplicates, append when absent.
fn set_query_param(url: &mut url::Url, key: &str, value: &str) {
    let original: Vec<(String, String)> = url
        .query_pairs()
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    let mut updated: Vec<(String, String)> = Vec::new();
    let mut replaced = false;
    for (k, v) in original {
        if k == key {
            if !replaced {
                updated.push((k, value.to_string()));
                replaced = true;
            }
        } else {
            updated.push((k, v));
        }
    }
    if !replaced {
        updated.push((key.to_string(), value.to_string()));
    }
    let mut pairs = url.query_pairs_mut();
    pairs.clear();
    for (k, v) in updated {
        pairs.append_pair(&k, &v);
    }
}

fn build_socket_url(
    relay_url: &str,
    identity: &RelayIdentity,
    role: &str,
    connection_id: Option<&str>,
) -> Result<String, String> {
    let mut url = url::Url::parse(relay_url).map_err(|error| error.to_string())?;
    let auth = (identity.sign_relay_auth)(role, connection_id);
    set_query_param(&mut url, "v", &RELAY_PROTOCOL_VERSION.to_string());
    set_query_param(&mut url, "role", role);
    set_query_param(&mut url, "serverId", &identity.server_id);
    if let Some(connection_id) = connection_id {
        set_query_param(&mut url, "connectionId", connection_id);
    }
    set_query_param(&mut url, "ts", &auth.ts.to_string());
    set_query_param(&mut url, "sig", &auth.sig);
    set_query_param(&mut url, "pk", &auth.pk);
    Ok(url.to_string())
}

impl RelayHostClient {
    pub fn start(options: RelayHostOptions) -> Arc<Self> {
        let (stopped_tx, stopped_rx) = watch::channel(false);
        let inner = Arc::new(Inner {
            relay_url: options.relay_url,
            identity: options.identity,
            get_local_port: options.get_local_port,
            on_status: options.on_status,
            resolved_batch_window_ms: resolve_batch_window_ms(options.batch_window_ms),
            local_batch: options.batch,
            status: Mutex::new(RelayHostStatus {
                state: "connecting".to_string(),
                last_error: None,
                connected_clients: 0,
            }),
            stopped: stopped_tx,
            data_sockets: Mutex::new(HashMap::new()),
        });
        let supervisor = tokio::spawn(run_control_supervisor(inner.clone(), stopped_rx.clone()));
        let sweeper = tokio::spawn(run_idle_sweeper(inner.clone(), stopped_rx));
        Arc::new(Self {
            inner,
            supervisor,
            sweeper,
        })
    }

    pub fn get_status(&self) -> RelayHostStatus {
        self.snapshot_status()
    }

    fn snapshot_status(&self) -> RelayHostStatus {
        self.inner
            .status
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn stop(&self) {
        let _ = self.inner.stopped.send(true);
        for connection_id in self
            .inner
            .data_sockets
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .keys()
            .cloned()
            .collect::<Vec<_>>()
        {
            teardown_data_socket(&self.inner, &connection_id, Some(1001), "host stopping");
        }
        self.supervisor.abort();
        self.sweeper.abort();
        set_state(&self.inner, "disabled", None, true);
    }
}

impl Drop for RelayHostClient {
    fn drop(&mut self) {
        self.stop();
    }
}

fn emit_status(inner: &Inner) {
    let status = inner
        .status
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    if let Some(on_status) = &inner.on_status {
        // Status consumers must not break the transport.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| on_status(status)));
    }
}

/// JS `setState(nextState, error)`: `error === undefined` keeps the old value.
fn set_state(inner: &Inner, state: &str, error: Option<String>, clear: bool) {
    {
        let mut status = inner.status.lock().unwrap_or_else(|e| e.into_inner());
        status.state = state.to_string();
        if error.is_some() || clear {
            status.last_error = error;
        }
    }
    emit_status(inner);
}

fn set_last_error(inner: &Inner, error: String) {
    inner
        .status
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .last_error = Some(error);
}

fn teardown_data_socket(inner: &Inner, connection_id: &str, close_code: Option<u16>, reason: &str) {
    let entry = inner
        .data_sockets
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(connection_id);
    let Some(mut entry) = entry else {
        return;
    };
    if let Some(batcher) = entry.batcher.take() {
        batcher.dispose();
    }
    if let Some(tunnel) = entry.tunnel.take() {
        tunnel.close();
    }
    match close_code {
        Some(code) => {
            // Graceful close: the pump task writes the close frame and exits
            // on its own (JS `socket.close(code, reason)`).
            let _ = entry
                .outbound_tx
                .send(OutboundMsg::Close(code, reason.to_string()));
        }
        None => {
            // JS `socket.terminate()`.
            for task in entry.tasks.drain(..) {
                task.abort();
            }
        }
    }
    emit_status(inner);
}

fn fail_channel(inner: &Inner, connection_id: &str, close_code: u16, reason: &str) {
    // connectionId + reason only — never payload contents.
    tracing::warn!("[Relay] data channel failed connectionId={connection_id} reason={reason}");
    teardown_data_socket(inner, connection_id, Some(close_code), reason);
}

// ---------------------------------------------------------------------------
// Control socket supervisor
// ---------------------------------------------------------------------------

async fn run_control_supervisor(inner: Arc<Inner>, mut stopped: watch::Receiver<bool>) {
    let mut consecutive_failures: u32 = 0;
    let mut stopped_rx = stopped.clone();
    loop {
        if *stopped_rx.borrow_and_update() {
            return;
        }
        set_state(
            &inner,
            if consecutive_failures == 0 {
                "connecting"
            } else {
                "reconnecting"
            },
            None,
            false,
        );

        let url = match build_socket_url(&inner.relay_url, &inner.identity, "host-control", None) {
            Ok(url) => url,
            Err(error) => {
                set_last_error(&inner, error);
                schedule_reconnect(&inner, &mut consecutive_failures, &mut stopped_rx).await;
                continue;
            }
        };

        let connected = match connect_async(url).await {
            Ok((ws, _)) => ws,
            Err(error) => {
                set_last_error(&inner, error.to_string());
                schedule_reconnect(&inner, &mut consecutive_failures, &mut stopped_rx).await;
                continue;
            }
        };

        consecutive_failures = 0;
        set_state(&inner, "connected", None, true);
        let (mut sink, mut inbound) = connected.split();
        let mut last_alive = tokio::time::Instant::now();
        let mut ping_tick = tokio::time::interval(Duration::from_millis(CONTROL_PING_INTERVAL_MS));
        ping_tick.tick().await; // first tick fires immediately
        let mut close_code: Option<u16> = None;
        let mut close_reason = String::new();

        loop {
            tokio::select! {
                _ = ping_tick.tick() => {
                    if last_alive.elapsed() > Duration::from_millis(CONTROL_PING_INTERVAL_MS + CONTROL_PONG_GRACE_MS) {
                        tracing::warn!("[Relay] control socket unresponsive (missed pong) — reconnecting");
                        break;
                    }
                    if sink.send(Message::Ping(Vec::new())).await.is_err() {
                        break;
                    }
                }
                message = inbound.next() => {
                    match message {
                        Some(Ok(Message::Text(text))) => {
                            last_alive = tokio::time::Instant::now();
                            handle_control_message(&inner, &text);
                        }
                        Some(Ok(Message::Pong(_))) | Some(Ok(Message::Binary(_))) => {
                            last_alive = tokio::time::Instant::now();
                        }
                        Some(Ok(Message::Ping(payload))) => {
                            last_alive = tokio::time::Instant::now();
                            let _ = sink.send(Message::Pong(payload)).await;
                        }
                        Some(Ok(Message::Close(frame))) => {
                            if let Some(frame) = frame {
                                close_code = Some(u16::from(frame.code));
                                close_reason = frame.reason.to_string();
                            } else {
                                close_code = Some(1006);
                            }
                            break;
                        }
                        Some(Ok(Message::Frame(_))) => {}
                        Some(Err(error)) => {
                            set_last_error(&inner, error.to_string());
                        }
                        None => {
                            if close_code.is_none() {
                                close_code = Some(1006);
                            }
                            break;
                        }
                    }
                }
                _ = stopped.changed() => {
                    let _ = sink.send(Message::Close(Some(
                        tokio_tungstenite::tungstenite::protocol::CloseFrame {
                            code: 1001.into(),
                            reason: "host stopping".into(),
                        },
                    ))).await;
                    return;
                }
            }
        }

        // JS close handler: only fill lastError when it is empty and the code
        // is abnormal; data sockets ride their own relay connections and the
        // relay keeps clients alive through a 30s control-reconnect grace
        // window, so leave them up.
        {
            let status = inner.status.lock().unwrap_or_else(|e| e.into_inner());
            if status.last_error.is_none() && close_code.is_some_and(|code| code != 1000) {
                drop(status);
                let code = close_code.unwrap_or_default();
                let detail = if close_reason.is_empty() {
                    format!("control socket closed ({code})")
                } else {
                    format!("control socket closed ({code}: {close_reason})")
                };
                set_last_error(&inner, detail);
            }
        }
        schedule_reconnect(&inner, &mut consecutive_failures, &mut stopped_rx).await;
    }
}

async fn schedule_reconnect(
    inner: &Inner,
    consecutive_failures: &mut u32,
    stopped: &mut watch::Receiver<bool>,
) {
    set_state(inner, "reconnecting", None, false);
    let delay = std::cmp::min(
        BACKOFF_BASE_MS.saturating_mul(1u64 << (*consecutive_failures).min(30)),
        BACKOFF_CAP_MS,
    );
    *consecutive_failures += 1;
    tokio::select! {
        _ = tokio::time::sleep(Duration::from_millis(delay)) => {}
        _ = stopped.changed() => {}
    }
}

fn handle_control_message(inner: &Arc<Inner>, raw: &str) {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(raw) else {
        return;
    };
    let Some(object) = value.as_object() else {
        return;
    };
    if object.get("type").and_then(|v| v.as_str()) == Some("sync") {
        if let Some(ids) = object.get("connectionIds").and_then(|v| v.as_array()) {
            let wanted: Vec<String> = ids
                .iter()
                .filter_map(|id| id.as_str())
                .filter(|id| !id.is_empty())
                .map(str::to_string)
                .collect();
            let existing: Vec<String> = inner
                .data_sockets
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .keys()
                .cloned()
                .collect();
            for connection_id in existing {
                if !wanted.contains(&connection_id) {
                    teardown_data_socket(inner, &connection_id, None, "");
                }
            }
            for connection_id in wanted {
                open_data_socket(inner, &connection_id);
            }
        }
        return;
    }
    if object.get("type").and_then(|v| v.as_str()) == Some("connected") {
        if let Some(connection_id) = object.get("connectionId").and_then(|v| v.as_str()) {
            open_data_socket(inner, connection_id);
        }
        return;
    }
    if object.get("type").and_then(|v| v.as_str()) == Some("disconnected") {
        if let Some(connection_id) = object.get("connectionId").and_then(|v| v.as_str()) {
            teardown_data_socket(inner, connection_id, None, "");
        }
    }
}

fn open_data_socket(inner: &Arc<Inner>, connection_id: &str) {
    if *inner.stopped.subscribe().borrow()
        || inner
            .data_sockets
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(connection_id)
    {
        return;
    }
    let url = match build_socket_url(
        &inner.relay_url,
        &inner.identity,
        "host-data",
        Some(connection_id),
    ) {
        Ok(url) => url,
        Err(error) => {
            tracing::warn!("[Relay] host-data dial failed: {error}");
            return;
        }
    };
    let (outbound_tx, outbound_rx) = mpsc::unbounded_channel();
    let buffered_bytes = Arc::new(AtomicU64::new(0));
    let entry = DataSocketEntry {
        outbound_tx: outbound_tx.clone(),
        buffered_bytes: buffered_bytes.clone(),
        tunnel: None,
        batcher: None,
        last_activity_ms: AtomicI64::new(now_ms()),
        tasks: Vec::new(),
    };
    inner
        .data_sockets
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(connection_id.to_string(), entry);
    emit_status(inner);
    let task = tokio::spawn(run_data_socket(
        inner.clone(),
        connection_id.to_string(),
        url,
        outbound_rx,
        outbound_tx.clone(),
        buffered_bytes,
    ));
    if let Some(entry) = inner
        .data_sockets
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get_mut(connection_id)
    {
        entry.tasks.push(task);
    }
}

/// Serializes encrypt+send so the per-direction IV counter reaches the wire in
/// encryption order (JS `sendChain`). One encrypt == one WS message == one
/// counter tick, whether it carries a batch or a lone frame.
async fn send_encrypted_plaintext(
    channel: &tokio::sync::Mutex<Option<Channel>>,
    outbound_tx: &mpsc::UnboundedSender<OutboundMsg>,
    buffered_bytes: &Arc<AtomicU64>,
    connection_id: &str,
    plaintext: Vec<u8>,
) {
    let mut guard = channel.lock().await;
    let Some(channel) = guard.as_mut() else {
        return;
    };
    match channel.encryptor.encrypt(&plaintext) {
        Ok(encrypted) => {
            buffered_bytes.fetch_add(encrypted.len() as u64, Ordering::SeqCst);
            let _ = outbound_tx.send(OutboundMsg::Binary(encrypted));
        }
        Err(error) => {
            tracing::warn!("[Relay] host-data send failed connectionId={connection_id}: {error}");
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_data_socket(
    inner: Arc<Inner>,
    connection_id: String,
    url: String,
    mut outbound_rx: mpsc::UnboundedReceiver<OutboundMsg>,
    outbound_tx: mpsc::UnboundedSender<OutboundMsg>,
    buffered_bytes: Arc<AtomicU64>,
) {
    // JS arms the open timeout before dialing and clears it on open: the
    // deadline covers the dial itself.
    let dial = tokio::time::timeout(
        Duration::from_millis(DATA_SOCKET_OPEN_TIMEOUT_MS),
        connect_async(url),
    )
    .await;
    let ws = match dial {
        Ok(Ok((ws, _))) => ws,
        Ok(Err(error)) => {
            tracing::warn!("[Relay] host-data dial failed: {error}");
            teardown_data_socket(&inner, &connection_id, None, "");
            return;
        }
        Err(_) => {
            tracing::warn!("[Relay] host-data socket open timeout");
            teardown_data_socket(&inner, &connection_id, None, "");
            return;
        }
    };
    let (mut sink, mut inbound) = ws.split();

    let handshake = HostHandshake::with_options(
        inner.identity.host_enc_private_key.clone(),
        inner.local_batch,
    );
    // JS keeps the handshake closure-local and only reachable from this
    // socket's message loop; the established channel moves into the send
    // state shared with the tunnel dispatcher.
    let mut handshake_state = handshake;
    let channel_state = Arc::new(tokio::sync::Mutex::new(None::<Channel>));
    let mut batch_negotiated = false;
    let mut tunnel: Option<Arc<TunnelHost>> = None;
    let mut stopped_rx = inner.stopped.subscribe();

    loop {
        tokio::select! {
            outbound = outbound_rx.recv() => {
                match outbound {
                    Some(OutboundMsg::Text(text)) => {
                        if sink.send(Message::Text(text)).await.is_err() {
                            break;
                        }
                    }
                    Some(OutboundMsg::Binary(bytes)) => {
                        let byte_len = bytes.len() as u64;
                        if sink.send(Message::Binary(bytes)).await.is_err() {
                            break;
                        }
                        buffered_bytes.fetch_sub(byte_len, Ordering::SeqCst);
                    }
                    Some(OutboundMsg::Close(code, reason)) => {
                        let _ = sink.send(Message::Close(Some(
                            tokio_tungstenite::tungstenite::protocol::CloseFrame {
                                code: code.into(),
                                reason: reason.into(),
                            },
                        ))).await;
                        break;
                    }
                    None => break,
                }
            }
            _ = stopped_rx.changed() => {
                return;
            }
            message = inbound.next() => {
                let incoming = match message {
                    Some(Ok(message)) => message,
                    Some(Err(error)) => {
                        tracing::warn!("[Relay] host-data socket error: {error}");
                        continue;
                    }
                    None => break,
                };
                // Any inbound message (including the client's keepalive Ping)
                // proves the client is alive.
                if let Some(entry) = inner.data_sockets.lock().unwrap_or_else(|e| e.into_inner()).get_mut(&connection_id) {
                    entry.last_activity_ms.store(now_ms(), Ordering::SeqCst);
                }
                match incoming {
                    Message::Text(text) => {
                        let action = handshake_state.handle_text(&text);
                        match action {
                            super::e2ee::HandshakeAction::SendText { text } => {
                                let _ = outbound_tx.send(OutboundMsg::Text(text));
                            }
                            super::e2ee::HandshakeAction::Established { batch, reply_text, channel } => {
                                batch_negotiated = batch;
                                *channel_state.lock().await = Some(channel);
                                let send_batch = {
                                    let channel_state = channel_state.clone();
                                    let outbound_tx = outbound_tx.clone();
                                    let buffered_bytes = buffered_bytes.clone();
                                    let connection_id = connection_id.clone();
                                    Arc::new(move |plaintext: Vec<u8>| {
                                        let channel_state = channel_state.clone();
                                        let outbound_tx = outbound_tx.clone();
                                        let buffered_bytes = buffered_bytes.clone();
                                        let connection_id = connection_id.clone();
                                        tokio::spawn(async move {
                                            send_encrypted_plaintext(
                                                &channel_state, &outbound_tx, &buffered_bytes,
                                                &connection_id, plaintext,
                                            ).await;
                                        });
                                    })
                                };
                                let batcher = if batch_negotiated {
                                    Some(OutboundFrameBatcher::start(inner.resolved_batch_window_ms, send_batch.clone()))
                                } else {
                                    None
                                };
                                let send_frame = {
                                    let channel_state = channel_state.clone();
                                    let outbound_tx = outbound_tx.clone();
                                    let buffered_bytes = buffered_bytes.clone();
                                    let connection_id = connection_id.clone();
                                    let batcher_for_send = batcher.clone();
                                    Arc::new(move |frame: Vec<u8>| {
                                        match &batcher_for_send {
                                            Some(batcher) => batcher.enqueue(frame),
                                            None => {
                                                let channel_state = channel_state.clone();
                                                let outbound_tx = outbound_tx.clone();
                                                let buffered_bytes = buffered_bytes.clone();
                                                let connection_id = connection_id.clone();
                                                tokio::spawn(async move {
                                                    send_encrypted_plaintext(
                                                        &channel_state, &outbound_tx, &buffered_bytes,
                                                        &connection_id, frame,
                                                    ).await;
                                                });
                                            }
                                        }
                                    })
                                };
                                let deps = TunnelHostDeps {
                                    connection_id: connection_id.clone(),
                                    get_local_port: inner.get_local_port.clone(),
                                    send_frame,
                                    get_buffered_amount: {
                                        let buffered_bytes = buffered_bytes.clone();
                                        Arc::new(move || buffered_bytes.load(Ordering::SeqCst))
                                    },
                                    body_delivery_timeout: Duration::from_millis(BODY_DELIVERY_TIMEOUT_MS),
                                };
                                let host = TunnelHost::new(deps);
                                tunnel = Some(host.clone());
                                if let Some(entry) = inner.data_sockets.lock().unwrap_or_else(|e| e.into_inner()).get_mut(&connection_id) {
                                    entry.tunnel = Some(host);
                                    entry.batcher = batcher.clone();
                                }
                                if !reply_text.is_empty() {
                                    let _ = outbound_tx.send(OutboundMsg::Text(reply_text));
                                }
                            }
                            super::e2ee::HandshakeAction::Fail { close_code, reason } => {
                                fail_channel(&inner, &connection_id, close_code, reason);
                                break;
                            }
                            super::e2ee::HandshakeAction::Ignore => {}
                        }
                    }
                    Message::Binary(data) => {
                        let tunnel = match (&tunnel, batch_negotiated) {
                            (Some(tunnel), _) => tunnel.clone(),
                            (None, _) => {
                                // Encrypted traffic before the handshake completed: fail closed.
                                fail_channel(&inner, &connection_id, CLOSE_CHANNEL_FAILURE, "binary frame before handshake");
                                break;
                            }
                        };
                        let plaintext = {
                            let mut guard = channel_state.lock().await;
                            let Some(channel) = guard.as_mut() else {
                                fail_channel(&inner, &connection_id, CLOSE_CHANNEL_FAILURE, "frame decryption failed");
                                break;
                            };
                            match channel.decryptor.decrypt(&data) {
                                Ok(plaintext) => plaintext,
                                Err(_) => {
                                    fail_channel(&inner, &connection_id, CLOSE_CHANNEL_FAILURE, "frame decryption failed");
                                    break;
                                }
                            }
                        };
                        let frames = if batch_negotiated {
                            match decode_frame_batch(&plaintext) {
                                Ok(frames) => frames,
                                Err(error) => {
                                    tracing::warn!("[Relay] tunnel frame handling failed: {error}");
                                    continue;
                                }
                            }
                        } else {
                            vec![plaintext]
                        };
                        for frame in frames {
                            if let Err(error) = tunnel.handle_frame(&frame) {
                                tracing::warn!("[Relay] tunnel frame handling failed: {error}");
                            }
                        }
                    }
                    Message::Ping(payload) => {
                        let _ = sink.send(Message::Pong(payload)).await;
                    }
                    Message::Close(_) => {
                        teardown_data_socket(&inner, &connection_id, None, "");
                        break;
                    }
                    _ => {}
                }
            }
        }
    }
    teardown_data_socket(&inner, &connection_id, None, "");
}

async fn run_idle_sweeper(inner: Arc<Inner>, mut stopped: watch::Receiver<bool>) {
    let mut tick = tokio::time::interval(Duration::from_millis(DATA_SOCKET_IDLE_SWEEP_INTERVAL_MS));
    tick.tick().await;
    loop {
        tokio::select! {
            _ = tick.tick() => {
                let now = now_ms();
                let stale: Vec<String> = inner
                    .data_sockets
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .iter()
                    .filter(|(_, entry)| now - entry.last_activity_ms.load(Ordering::SeqCst) > DATA_SOCKET_IDLE_TIMEOUT_MS as i64)
                    .map(|(id, _)| id.clone())
                    .collect();
                for connection_id in stale {
                    tracing::info!("[Relay] reaping idle data socket connectionId={connection_id}");
                    teardown_data_socket(&inner, &connection_id, Some(1001), "client idle timeout");
                }
            }
            _ = stopped.changed() => return,
        }
    }
}
