//! Port of `server/lib/relay/tunnel-host.js` — host side of the tunnel mux
//! (Layer 3): consumes decrypted tunnel frames for ONE relay connection and
//! dispatches them to the local loopback origin.
//!
//! HTTP streams → reqwest against `http://127.0.0.1:<port>` with streamed
//! duplex bodies; WS streams → a tokio-tungstenite client to the loopback
//! WebSocket endpoints. The dispatcher NEVER injects credentials: tunneled
//! requests authenticate exactly like any remote client (bearer `oc_client_*`
//! header, `oc_url_token` query).
//!
//! 中文说明：本模块为隧道多路复用（Layer 3）的主机端实现，移植自
//! `server/lib/relay/tunnel-host.js`：消费单条 relay 连接解密后的隧道帧，
//! 并分发到本地 loopback 源站。HTTP 流经 reqwest 以流式双工 body 请求
//! `http://127.0.0.1:<port>`；WS 流经 tokio-tungstenite 连接 loopback
//! WebSocket 端点。分发器绝不注入凭据：隧道请求的认证方式与任何远程
//! 客户端完全一致（bearer `oc_client_*` 头、`oc_url_token` 查询参数）。

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use axum::body::Bytes;
use futures::{SinkExt, StreamExt};
use serde_json::{Map, Value, json};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

use super::tunnel_codec::{
    MAX_TUNNEL_PAYLOAD_BYTES, decode_json_payload, decode_tunnel_frame, encode_fragmented_message,
    encode_json_payload, encode_tunnel_frame, frame_type,
};

// Path allowlists (defense in depth; same families realtime-proxy.js allows).
/// 判断 HTTP 请求路径是否允许经 relay 转发（纵深防御，与
/// realtime-proxy.js 允许的路径族一致）：仅放行 `/health`、`/api`（含
/// `/api/` 前缀）与 `/auth`（含 `/auth/` 前缀）。
fn is_allowed_http_path(pathname: &str) -> bool {
    pathname == "/health"
        || pathname == "/api"
        || pathname.starts_with("/api/")
        || pathname == "/auth"
        || pathname.starts_with("/auth/")
}

/// 允许经 relay 隧道转发的 WebSocket 端点白名单：全局事件、会话事件、
/// 终端与听写四类。
pub const ALLOWED_WS_PATHS: [&str; 4] = [
    "/api/global/event/ws",
    "/api/event/ws",
    "/api/terminal/ws",
    "/api/dictation/ws",
];

// Hop-by-hop headers stripped from tunneled requests; `host` is set by the
// loopback client. content-length is dropped too because the body is
// re-chunked through the tunnel and the HTTP client computes framing itself.
/// 从隧道请求中剥离的逐跳（hop-by-hop）头；`host` 由 loopback 客户端
/// 自行设置。`content-length` 同样被丢弃：body 经隧道重新分块传输，
/// 帧长由 HTTP 客户端自行计算。
const STRIPPED_REQUEST_HEADERS: [&str; 6] = [
    "connection",
    "keep-alive",
    "transfer-encoding",
    "upgrade",
    "host",
    "content-length",
];

// Response framing headers that no longer apply once the body crosses the
// tunnel as HttpBody chunks (loopback fetch already decoded content-encoding).
/// 响应 body 以 HttpBody 块穿越隧道后不再适用的响应帧头（loopback
/// fetch 已完成 content-encoding 解码）。
const STRIPPED_RESPONSE_HEADERS: [&str; 5] = [
    "connection",
    "keep-alive",
    "transfer-encoding",
    "content-length",
    "content-encoding",
];

// v1 backpressure rule: pause reading the loopback source while the outbound
// relay socket has more than this buffered.
/// v1 背压规则：出站 relay socket 缓冲超过该字节数时暂停读取 loopback 源。
const BACKPRESSURE_LIMIT_BYTES: u64 = 4 * 1024 * 1024;
/// 背压轮询间隔：等待出站缓冲回落时的 sleep 粒度。
const BACKPRESSURE_POLL_MS: u64 = 20;

// Bodies smaller than this are fully buffered before the loopback request is
// sent, so a tunneled body that lost frames (relay reconnect, dropped HttpBody
// frames) can never reach the loopback server as an empty/truncated chunked
// body. Larger bodies stream live as before.
/// 小于该阈值的请求 body 在发送 loopback 请求前先完整缓冲，确保因
/// relay 重连或 HttpBody 帧丢失而损坏的隧道 body 绝不会以空或截断的
/// chunked body 到达 loopback 服务器；超过阈值的 body 仍按流式实时转发。
const BODY_BUFFER_MAX_BYTES: usize = 512 * 1024;
// While the body is still being buffered, abort the stream if it never
// completes, so a stalled tunnel converts into an ambiguous transport failure
// (which the client already retries) instead of a hung loopback request.
/// body 仍处于缓冲阶段时的投递超时：超时即中止流，把停滞的隧道转化为
/// 客户端本就会重试的模糊传输失败，而非挂死的 loopback 请求。
pub const BODY_DELIVERY_TIMEOUT_MS: u64 = 15_000;
/// 流式（live）body 通道容量：写满后生产端在 send 上等待，形成天然背压。
const LIVE_BODY_CHANNEL_CAPACITY: usize = 32;
/// 发起 WebSocket 关闭握手后等待对端回应 close 帧的最长时间；超时即视为
/// 连接死亡（对应 JS ws 客户端仍会触发 close 事件的语义）。
const WS_CLOSE_HANDSHAKE_TIMEOUT_MS: u64 = 2_000;

/// 传输层发送回调类型：把一个明文隧道帧交给 batcher 或直接加密发送。
type SendFrameFn = Arc<dyn Fn(Vec<u8>) + Send + Sync>;
/// 查询本地 loopback HTTP 服务端口的回调类型（每次转发时实时读取）。
type GetLocalPortFn = Arc<dyn Fn() -> u16 + Send + Sync>;
/// 查询传输层出站缓冲字节数的回调类型，用于背压判断。
type BufferedAmountFn = Arc<dyn Fn() -> u64 + Send + Sync>;

/// 构造 [`TunnelHost`] 所需的外部依赖集：连接标识、端口查询、帧发送、
/// 缓冲查询与 body 投递超时，均由 relay 连接层注入。
pub struct TunnelHostDeps {
    /// relay 连接标识，写入 loopback 请求的 `x-ompchamber-relay-connection` 头。
    pub connection_id: String,
    /// 每次转发时查询当前本地 loopback 端口。
    pub get_local_port: GetLocalPortFn,
    /// Hands one plaintext tunnel frame to the transport (batcher or direct
    /// encrypted send). Synchronous enqueue; the transport owns ordering.
    /// 中文：把单个明文隧道帧同步交给传输层（batcher 或直接加密发送）；
    /// 帧顺序由传输层负责保证。
    pub send_frame: SendFrameFn,
    /// 查询出站传输缓冲字节数，驱动背压暂停。
    pub get_buffered_amount: BufferedAmountFn,
    /// 请求 body 缓冲阶段的投递超时时长（默认取 `BODY_DELIVERY_TIMEOUT_MS`）。
    pub body_delivery_timeout: Duration,
}

/// 请求 body 泵向转发任务推送的事件流：分块数据、正常结束或失败。
enum BodyEvent {
    /// 一段请求 body 字节。
    Chunk(Vec<u8>),
    /// 客户端已半关闭请求 body（收到 StreamEnd）。
    End,
    /// body 传输失败，携带原因字符串。
    Fail(String),
}

/// 待写入本地 WebSocket 的出站消息：文本、二进制或关闭帧。
enum WsOut {
    /// 文本消息。
    Text(String),
    /// 二进制消息。
    Binary(Vec<u8>),
    /// 关闭帧，携带状态码与原因。
    Close(u16, String),
}

/// 一条 HTTP 隧道流的主机端状态：代际标记、请求 body 通道与两个可中止任务。
struct HttpStream {
    /// 创建时分配的代际号，用于识别流是否已被替换或中止。
    generation: u64,
    /// 请求 body 事件发送端；None 表示 body 已转发完毕、无需再接收。
    body_tx: Option<mpsc::UnboundedSender<BodyEvent>>,
    /// body 泵任务（`run_http_stream`）句柄，中止流时用于取消。
    run_task: Option<JoinHandle<()>>,
    /// loopback 转发任务（`forward_request`）句柄，中止流时用于取消。
    forward_task: Option<JoinHandle<()>>,
    /// 该请求无 body（GET/HEAD 等），迟到的 HttpBody 帧应被丢弃。
    no_body: bool,
}

/// 一条 WebSocket 隧道流的主机端状态：出站写通道、握手完成标记与任务句柄。
struct WsStream {
    /// 指向本地 WS 连接的出站消息通道；发送 Close 即触发关闭握手。
    writer_tx: mpsc::UnboundedSender<WsOut>,
    /// 升级握手是否已完成；完成前收到的隧道消息将被丢弃。
    opened: Arc<AtomicBool>,
    /// `run_ws_stream` 任务句柄；中止等价 JS 的 `socket.terminate()`。
    task: JoinHandle<()>,
}

/// 按 stream_id 索引的活跃流条目：HTTP 或 WebSocket 二选一。
enum StreamEntry {
    /// HTTP 请求/响应流。
    Http(Box<HttpStream>),
    /// WebSocket 流。
    Ws(Box<WsStream>),
}

/// `StreamEntry` 的辅助方法。
impl StreamEntry {
    /// 返回流类型名（"http" 或 "ws"），用于诊断信息。
    fn kind(&self) -> &'static str {
        match self {
            StreamEntry::Http(_) => "http",
            StreamEntry::Ws(_) => "ws",
        }
    }
}

/// 单条 relay 连接内全部隧道流共享的状态：依赖注入、流表、WS 分片重组器、
/// 关闭标记、代际计数与复用的 loopback HTTP 客户端。
struct Shared {
    /// 外部依赖（连接标识、端口查询、帧发送、缓冲查询、超时）。
    deps: TunnelHostDeps,
    /// stream_id 到活跃流条目的映射。
    streams: std::sync::Mutex<HashMap<u32, StreamEntry>>,
    /// WS 消息分片重组器，跨帧拼装完整的 WS_TEXT/WS_BINARY 消息。
    assembler: std::sync::Mutex<super::tunnel_codec::FragmentAssembler>,
    /// 主机已关闭标记；置位后所有发送变为 no-op。
    closed: AtomicBool,
    /// 单调递增代际计数，区分同 stream_id 的先后实例。
    generation: AtomicU64,
    /// 复用的 reqwest 客户端，所有 loopback HTTP 转发共用。
    http: reqwest::Client,
}

/// Per-connection tunnel dispatcher (JS `createTunnelHost`).
/// 中文：单条 relay 连接的隧道分发器（对应 JS `createTunnelHost`），
/// 通过 `Arc<Shared>` 与各流任务共享状态。
pub struct TunnelHost {
    /// 与各流任务共享的状态（流表、重组器、依赖与 reqwest 客户端）。
    shared: Arc<Shared>,
}

/// 校验解出的 JSON 是否形如 HTTP 请求载荷：method/path/query 均为字符串
/// 且 headers 为对象。
fn is_http_request_payload(parsed: &Value) -> bool {
    parsed.get("method").is_some_and(Value::is_string)
        && parsed.get("path").is_some_and(Value::is_string)
        && parsed.get("query").is_some_and(Value::is_string)
        && parsed.get("headers").is_some_and(Value::is_object)
}

/// 校验解出的 JSON 是否形如 WS 打开载荷：path/query 为字符串，protocols
/// 为数组（缺失视为合法）。
fn is_ws_open_payload(parsed: &Value) -> bool {
    parsed.get("path").is_some_and(Value::is_string)
        && parsed.get("query").is_some_and(Value::is_string)
        && parsed.get("protocols").map(Value::is_array).unwrap_or(true)
}

/// 校验解出的 JSON 是否形如 WS 关闭载荷：任意 JSON 对象即可，字段可选。
fn is_ws_close_payload(parsed: &Value) -> bool {
    parsed.is_object()
}

/// `TunnelHost` 对外 API：解密帧入口、整体关闭与流计数。
impl TunnelHost {
    /// 创建隧道主机：初始化流表、分片重组器、代际计数与 reqwest 客户端，
    /// 返回 `Arc` 以便跨任务共享。
    pub fn new(deps: TunnelHostDeps) -> Arc<Self> {
        Arc::new(Self {
            shared: Arc::new(Shared {
                http: reqwest::Client::builder().build().unwrap_or_default(),
                deps,
                streams: std::sync::Mutex::new(HashMap::new()),
                assembler: std::sync::Mutex::new(super::tunnel_codec::FragmentAssembler::default()),
                closed: AtomicBool::new(false),
                generation: AtomicU64::new(0),
            }),
        })
    }

    /// One decrypted tunnel frame (or one frame from a decoded batch).
    /// 中文：处理单个解密后的隧道帧（或解码批次中的一帧）。WS 消息帧先
    /// 经分片重组器拼装，其余帧按类型分发到对应 handler；PONG 及仅主机
    /// 使用的帧类型被忽略而非拆掉隧道。
    pub fn handle_frame(
        &self,
        plaintext_frame: &[u8],
    ) -> Result<(), super::tunnel_codec::TunnelCodecError> {
        if self.shared.closed.load(Ordering::SeqCst) {
            return Ok(());
        }
        let frame = decode_tunnel_frame(plaintext_frame)?;

        // WS message frames can be fragmented; everything else arrives whole.
        if frame.frame_type == frame_type::WS_TEXT || frame.frame_type == frame_type::WS_BINARY {
            let (stream_id, frame_type_value) = (frame.stream_id, frame.frame_type);
            let message = self
                .shared
                .assembler
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(frame)?;
            if let Some(message) = message {
                self.shared
                    .handle_ws_message(stream_id, frame_type_value, &message);
            }
            return Ok(());
        }

        match frame.frame_type {
            frame_type::HTTP_REQUEST => self
                .shared
                .clone()
                .handle_http_request(frame.stream_id, &frame.payload),
            frame_type::HTTP_BODY => self
                .shared
                .handle_http_body(frame.stream_id, &frame.payload),
            frame_type::STREAM_END => self.shared.handle_stream_end(frame.stream_id),
            frame_type::STREAM_ABORT => self
                .shared
                .abort_local_stream(frame.stream_id, "aborted by client"),
            frame_type::WS_OPEN => self
                .shared
                .clone()
                .handle_ws_open(frame.stream_id, &frame.payload),
            frame_type::WS_CLOSE => self.shared.handle_ws_close(frame.stream_id, &frame.payload),
            frame_type::PING => {
                let shared = self.shared.clone();
                let frame = encode_tunnel_frame(frame_type::PONG, frame.stream_id, &[], false)?;
                shared.send(&frame);
            }
            // Pong and host-only frame types (HttpResponse, WsOpened) are
            // ignored rather than tearing the tunnel down.
            _ => {}
        }
        Ok(())
    }

    /// Tear down every local stream (JS `close`).
    /// 中文：关闭主机（对应 JS `close`）：幂等；中止并清空所有本地流，
    /// 之后到达的帧静默丢弃。
    pub fn close(&self) {
        if self.shared.closed.swap(true, Ordering::SeqCst) {
            return;
        }
        let mut streams = self
            .shared
            .streams
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        for (stream_id, entry) in streams.iter_mut() {
            self.shared
                .abort_entry(*stream_id, entry, "connection closed");
        }
        streams.clear();
    }

    /// 当前活跃流数量（HTTP 与 WS 之和），用于观测与测试。
    pub fn stream_count(&self) -> usize {
        self.shared
            .streams
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .len()
    }
}

/// `Shared` 的内部实现：帧发送辅助、流表与重组器维护、HTTP/WS 帧处理。
impl Shared {
    /// 发送单个已编码隧道帧；主机已关闭时静默丢弃。
    fn send(&self, frame: &[u8]) {
        if self.closed.load(Ordering::SeqCst) {
            return;
        }
        (self.deps.send_frame)(frame.to_vec());
    }

    /// 将 JSON 载荷编码为指定类型的隧道帧并发送；编码失败静默忽略。
    fn send_json(&self, frame_type: u8, stream_id: u32, payload: &Value) {
        if let Ok(frame) =
            encode_tunnel_frame(frame_type, stream_id, &encode_json_payload(payload), false)
        {
            self.send(&frame);
        }
    }

    /// 向客户端发送 STREAM_ABORT 帧并携带原因文本。
    fn send_abort(&self, stream_id: u32, reason: &str) {
        self.send_json(
            frame_type::STREAM_ABORT,
            stream_id,
            &json!({ "reason": reason }),
        );
    }

    /// 从流表与分片重组器中移除指定流；不负责中止任务，由调用方处理。
    fn drop_stream(&self, stream_id: u32) {
        self.streams
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&stream_id);
        self.assembler
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drop_stream(stream_id);
    }

    /// 判断 stream_id 当前是否仍指向给定代际的 HTTP 流；异步任务收尾时
    /// 用它防止向已被替换或中止的流实例发送帧。
    fn still_ours(&self, stream_id: u32, generation: u64) -> bool {
        match self
            .streams
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&stream_id)
        {
            Some(StreamEntry::Http(stream)) => stream.generation == generation,
            _ => false,
        }
    }

    /// 就地中止一个流条目：HTTP 先向 body 泵发送 Fail 再中止 run/forward
    /// 两个任务（均为尽力而为）；WS 直接中止任务（等价 JS
    /// `socket.terminate()`）。
    fn abort_entry(&self, _stream_id: u32, entry: &mut StreamEntry, reason: &str) {
        match entry {
            StreamEntry::Http(stream) => {
                // Body error first (JS `stream.body?.error(...)`), then abort
                // the fetch signal; both are best-effort.
                if let Some(body_tx) = stream.body_tx.take() {
                    let _ = body_tx.send(BodyEvent::Fail(reason.to_string()));
                }
                if let Some(task) = stream.run_task.take() {
                    task.abort();
                }
                if let Some(task) = stream.forward_task.take() {
                    task.abort();
                }
            }
            StreamEntry::Ws(stream) => {
                // JS `socket.terminate()`.
                stream.task.abort();
            }
        }
    }

    /// 移除并中止指定流（含重组器清理）；流不存在时为 no-op。
    fn abort_local_stream(&self, stream_id: u32, reason: &str) {
        let mut streams = self.streams.lock().unwrap_or_else(|e| e.into_inner());
        let Some(mut entry) = streams.remove(&stream_id) else {
            return;
        };
        self.assembler
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drop_stream(stream_id);
        self.abort_entry(stream_id, &mut entry, reason);
    }

    /// 轮询等待出站缓冲降回背压阈值以下；主机已关闭时立即返回。
    async fn wait_for_backpressure(&self) {
        while !self.closed.load(Ordering::SeqCst)
            && (self.deps.get_buffered_amount)() > BACKPRESSURE_LIMIT_BYTES
        {
            tokio::time::sleep(Duration::from_millis(BACKPRESSURE_POLL_MS)).await;
        }
    }

    // -----------------------------------------------------------------------
    // HTTP
    // -----------------------------------------------------------------------

    /// 把隧道请求头转换为 loopback 请求头：名字小写化、剥离逐跳头与
    /// content-length、丢弃含 CR/LF 的注入头、同名头后者覆盖；最后强制
    /// 写入 `x-ompchamber-relay-connection` 与 loopback `origin`。
    fn build_request_headers(
        &self,
        raw_headers: &Value,
        loopback_origin: &str,
    ) -> Vec<(String, String)> {
        let mut headers: Vec<(String, String)> = Vec::new();
        let Some(object) = raw_headers.as_object() else {
            headers.push((
                "x-ompchamber-relay-connection".to_string(),
                self.deps.connection_id.clone(),
            ));
            headers.push(("origin".to_string(), loopback_origin.to_string()));
            return headers;
        };
        for (name, value) in object {
            let Value::String(value) = value else {
                continue;
            };
            let lower = name.to_lowercase();
            if STRIPPED_REQUEST_HEADERS.contains(&lower.as_str()) {
                continue;
            }
            if name.contains(['\r', '\n']) || value.contains(['\r', '\n']) {
                continue;
            }
            match headers.iter_mut().find(|(existing, _)| *existing == lower) {
                Some(slot) => slot.1 = value.clone(),
                None => headers.push((lower, value.clone())),
            }
        }
        match headers
            .iter_mut()
            .find(|(existing, _)| existing == "x-ompchamber-relay-connection")
        {
            Some(slot) => slot.1 = self.deps.connection_id.clone(),
            None => headers.push((
                "x-ompchamber-relay-connection".to_string(),
                self.deps.connection_id.clone(),
            )),
        }
        // Browser-generated Origin is not visible to the tunnel client. Present
        // the loopback origin being dialed and overwrite any client-supplied
        // value.
        match headers
            .iter_mut()
            .find(|(existing, _)| existing == "origin")
        {
            Some(slot) => slot.1 = loopback_origin.to_string(),
            None => headers.push(("origin".to_string(), loopback_origin.to_string())),
        }
        headers
    }

    // Synthetic responses never ship an empty body: `reason` states explicitly
    // that the relay host (not the upstream server) produced this response.
    /// 发送主机侧合成错误响应（HttpResponse + 单个 HttpBody + StreamEnd）；
    /// body 永不为空，且 `reason` 明确标识响应来自 relay 主机而非上游服务器。
    fn synthetic_response(&self, stream_id: u32, status: u16, message: &str) {
        self.send_json(
            frame_type::HTTP_RESPONSE,
            stream_id,
            &json!({ "status": status, "headers": { "content-type": "application/json" } }),
        );
        let body = encode_json_payload(&json!({
            "error": message,
            "reason": message,
            "source": "relay-tunnel-host",
        }));
        if let Ok(frame) = encode_tunnel_frame(frame_type::HTTP_BODY, stream_id, &body, false) {
            self.send(&frame);
        }
        if let Ok(frame) = encode_tunnel_frame(frame_type::STREAM_END, stream_id, &[], false) {
            self.send(&frame);
        }
    }

    /// 处理 HTTP_REQUEST 帧：拒绝重复 stream_id、校验请求载荷、登记
    /// HttpStream 并启动 body 泵任务 `run_http_stream`。
    fn handle_http_request(self: Arc<Self>, stream_id: u32, payload: &[u8]) {
        if self
            .streams
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(&stream_id)
        {
            self.abort_local_stream(stream_id, "duplicate stream id");
            self.send_abort(stream_id, "duplicate stream id");
            return;
        }
        let request = match decode_json_payload(payload, is_http_request_payload) {
            Ok(request) => request,
            Err(error) => {
                self.send_abort(stream_id, &error.0);
                return;
            }
        };
        let (body_tx, body_rx) = mpsc::unbounded_channel();
        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        self.streams
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(
                stream_id,
                StreamEntry::Http(Box::new(HttpStream {
                    generation,
                    body_tx: Some(body_tx),
                    run_task: None,
                    forward_task: None,
                    no_body: false,
                })),
            );
        let shared = self.clone();
        let task = tokio::spawn(run_http_stream(
            shared, stream_id, generation, request, body_rx,
        ));
        if let Some(StreamEntry::Http(stream)) = self
            .streams
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_mut(&stream_id)
        {
            stream.run_task = Some(task);
        }
    }

    /// 处理 HTTP_BODY 帧：向对应流的 body 通道投递分块；无 body 的流或
    /// body 已完成的流直接丢弃迟到字节。
    fn handle_http_body(&self, stream_id: u32, payload: &[u8]) {
        let mut streams = self.streams.lock().unwrap_or_else(|e| e.into_inner());
        let Some(StreamEntry::Http(stream)) = streams.get_mut(&stream_id) else {
            return;
        };
        if stream.no_body {
            return;
        }
        // The run task installs a body sink before any HttpBody frame can
        // arrive; drop stray bytes for request bodies already completed/aborted.
        if let Some(body_tx) = &stream.body_tx {
            let _ = body_tx.send(BodyEvent::Chunk(payload.to_vec()));
        }
    }

    /// 处理 STREAM_END 帧：向 body 通道发送 End（请求 body 半关闭）；
    /// 响应侧继续运行不受影响。
    fn handle_stream_end(&self, stream_id: u32) {
        let mut streams = self.streams.lock().unwrap_or_else(|e| e.into_inner());
        let Some(StreamEntry::Http(stream)) = streams.get_mut(&stream_id) else {
            return;
        };
        if let Some(body_tx) = &stream.body_tx {
            let _ = body_tx.send(BodyEvent::End);
        }
        // Response side keeps running; only the request body is half-closed.
    }

    // -----------------------------------------------------------------------
    // WebSocket
    // -----------------------------------------------------------------------

    /// 处理 WS_OPEN 帧：拒绝重复 stream_id、校验载荷与路径白名单、拼装
    /// loopback WS URL（含 query 与子协议）并启动 `run_ws_stream` 任务。
    fn handle_ws_open(self: Arc<Self>, stream_id: u32, payload: &[u8]) {
        if self
            .streams
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(&stream_id)
        {
            self.abort_local_stream(stream_id, "duplicate stream id");
            self.send_abort(stream_id, "duplicate stream id");
            return;
        }
        let open = match decode_json_payload(payload, is_ws_open_payload) {
            Ok(open) => open,
            Err(error) => {
                self.send_abort(stream_id, &error.0);
                return;
            }
        };
        let path = open.get("path").and_then(Value::as_str).unwrap_or_default();
        if !ALLOWED_WS_PATHS.contains(&path) {
            self.send_abort(stream_id, "Path is not allowed through the relay");
            return;
        }

        let port = (self.deps.get_local_port)();
        let query = open
            .get("query")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let url = format!(
            "ws://127.0.0.1:{port}{path}{}",
            if query.is_empty() {
                String::new()
            } else {
                format!("?{query}")
            }
        );
        // Present the loopback origin we're actually dialing. The server
        // derives this as a trusted same-origin candidate from the Host header
        // (127.0.0.1:<port>), so the WS origin check passes reliably for every
        // client platform. The request itself is still authenticated by the
        // tunneled oc_url_token, not by this origin.
        let protocols: Vec<String> = open
            .get("protocols")
            .and_then(Value::as_array)
            .map(|values| {
                values
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        let shared = self.clone();
        let (writer_tx, writer_rx) = mpsc::unbounded_channel();
        let opened = Arc::new(AtomicBool::new(false));
        let task = tokio::spawn(run_ws_stream(
            shared,
            stream_id,
            url,
            protocols,
            writer_rx,
            opened.clone(),
        ));
        self.streams
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(
                stream_id,
                StreamEntry::Ws(Box::new(WsStream {
                    writer_tx,
                    opened,
                    task,
                })),
            );
    }

    /// 处理重组完成的 WS 消息帧：仅在升级握手完成后转发到本地 WS 写通道，
    /// 其余静默丢弃。
    fn handle_ws_message(&self, stream_id: u32, frame_type: u8, message: &[u8]) {
        let streams = self.streams.lock().unwrap_or_else(|e| e.into_inner());
        let Some(StreamEntry::Ws(stream)) = streams.get(&stream_id) else {
            return;
        };
        if !stream.opened.load(Ordering::SeqCst) {
            return;
        }
        let out = if frame_type == frame_type::WS_TEXT {
            WsOut::Text(String::from_utf8_lossy(message).into_owned())
        } else {
            WsOut::Binary(message.to_vec())
        };
        let _ = stream.writer_tx.send(out);
    }

    /// 处理 WS_CLOSE 帧：移除流并向本地 WS 发送关闭帧（code 限定
    /// 1000..=4999，非法或缺失取 1000）；写通道已断时直接强杀任务（对应
    /// JS 关闭抛错后回退 terminate() 的行为）。
    fn handle_ws_close(&self, stream_id: u32, payload: &[u8]) {
        let mut streams = self.streams.lock().unwrap_or_else(|e| e.into_inner());
        let Some(StreamEntry::Ws(stream)) = streams.remove(&stream_id) else {
            return;
        };
        self.assembler
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drop_stream(stream_id);
        let mut close = Map::new();
        if let Ok(parsed) = decode_json_payload(payload, is_ws_close_payload) {
            close = parsed.as_object().cloned().unwrap_or_default();
        }
        let code = close
            .get("code")
            .and_then(Value::as_u64)
            .filter(|code| (1000..=4999).contains(code))
            .unwrap_or(1000) as u16;
        let reason = close
            .get("reason")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if stream.writer_tx.send(WsOut::Close(code, reason)).is_err() {
            // JS falls back to `socket.terminate()` when close throws.
            stream.task.abort();
        }
    }
}

/// 请求 body 泵的终态：正常结束、失败（带原因）或投递超时。
enum BodyOutcome {
    /// 收到 StreamEnd（或通道关闭），body 完整。
    Ended,
    /// body 通道收到 Fail 事件，携带原因。
    Failed(String),
    /// 超过 body 投递超时仍未收齐。
    TimedOut,
}

/// HTTP 流的 body 泵与调度核心：先校验路径白名单（否则发 403 合成响应并
/// 丢弃流），再拼装 loopback URL；无 body 请求直接转发，有 body 请求先
/// 缓冲、超过 `BODY_BUFFER_MAX_BYTES` 后切换为流式转发；对超时、失败与
/// 丢帧（声明有 body 却一个分块未到）分别走中止路径。
async fn run_http_stream(
    shared: Arc<Shared>,
    stream_id: u32,
    generation: u64,
    request: Value,
    mut body_rx: mpsc::UnboundedReceiver<BodyEvent>,
) {
    let method = request
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_uppercase();
    let path = request
        .get("path")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    if !is_allowed_http_path(&path) {
        shared.drop_stream(stream_id);
        shared.synthetic_response(stream_id, 403, "Path is not allowed through the relay");
        return;
    }

    let has_body = method != "GET" && method != "HEAD";
    let port = (shared.deps.get_local_port)();
    let loopback_origin = format!("http://127.0.0.1:{port}");
    let query = request
        .get("query")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let url = format!(
        "{loopback_origin}{path}{}",
        if query.is_empty() {
            String::new()
        } else {
            format!("?{query}")
        }
    );

    if !has_body {
        if let Some(StreamEntry::Http(stream)) = shared
            .streams
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_mut(&stream_id)
        {
            stream.no_body = true;
        }
        spawn_forward_request(
            shared.clone(),
            stream_id,
            generation,
            url,
            method,
            request,
            ForwardBody::None,
            loopback_origin,
        );
        return;
    }

    // Body-carrying request. Buffer the tunneled body frames and forward the
    // COMPLETE body only once StreamEnd arrives. Forwarding a body that lost
    // frames through the tunnel reaches the loopback server as an
    // empty/truncated chunked body, which it rejects with a bare 400 — the
    // "Failed to send message (400)" seen from the mobile APK. Bodies above
    // BODY_BUFFER_MAX_BYTES stream live so large uploads are not buffered.
    let mut buffered: Vec<Vec<u8>> = Vec::new();
    let mut buffered_bytes = 0usize;
    let mut body_frame_count = 0usize;
    let mut live_tx: Option<mpsc::Sender<Vec<u8>>> = None;
    let timeout = shared.deps.body_delivery_timeout;
    let deadline = tokio::time::Instant::now() + timeout;

    let outcome = loop {
        let timer = tokio::time::sleep_until(deadline);
        tokio::select! {
            event = body_rx.recv() => {
                match event {
                    Some(BodyEvent::Chunk(payload)) => {
                        body_frame_count += 1;
                        if let Some(live_tx) = &live_tx {
                            // The live stream applies backpressure instead of
                            // dropping chunks (JS enqueues into a ReadableStream
                            // and swallows errors only once closed).
                            let _ = live_tx.send(payload).await;
                        } else {
                            buffered_bytes += payload.len();
                            buffered.push(payload);
                            if buffered_bytes > BODY_BUFFER_MAX_BYTES {
                                let (tx, rx) = mpsc::channel(LIVE_BODY_CHANNEL_CAPACITY);
                                for chunk in buffered.drain(..) {
                                    if tx.send(chunk).await.is_err() {
                                        break;
                                    }
                                }
                                live_tx = Some(tx);
                                spawn_forward_request(
                                    shared.clone(),
                                    stream_id,
                                    generation,
                                    url.clone(),
                                    method.clone(),
                                    request.clone(),
                                    ForwardBody::Live(rx),
                                    loopback_origin.clone(),
                                );
                                // The loopback request is now streaming live;
                                // this pump only forwards chunks from here.
                            }
                        }
                    }
                    Some(BodyEvent::Fail(reason)) => break BodyOutcome::Failed(reason),
                    Some(BodyEvent::End) => break BodyOutcome::Ended,
                    None => break BodyOutcome::Ended,
                }
            }
            _ = timer, if live_tx.is_none() => {
                break BodyOutcome::TimedOut;
            }
        }
    };

    match outcome {
        BodyOutcome::TimedOut => {
            shared.drop_stream(stream_id);
            shared.send_abort(stream_id, "tunnel request body was not delivered in time");
            // Settle the buffered chunks; the dropped stream means any late
            // frame no-ops and no second abort is sent.
            return;
        }
        BodyOutcome::Failed(reason) => {
            if !shared.still_ours(stream_id, generation) {
                return;
            }
            shared.drop_stream(stream_id);
            shared.send_abort(stream_id, &reason);
            return;
        }
        BodyOutcome::Ended => {}
    }

    if !shared.still_ours(stream_id, generation) {
        // Aborted or dropped meanwhile.
        return;
    }
    if live_tx.is_some() {
        // Already forwarded via the streaming path; half-close the live body.
        drop(live_tx);
        return;
    }

    // The client signaled it had a body but no HttpBody frame arrived before
    // StreamEnd — the body frames were lost through the tunnel. Abort instead
    // so the client treats it as an ambiguous transport failure and can
    // safely retry.
    if request.get("hasBody") == Some(&Value::Bool(true)) && body_frame_count == 0 {
        shared.drop_stream(stream_id);
        shared.send_abort(stream_id, "tunnel request body frames were lost");
        return;
    }

    // Buffered path: forward the complete body as a single buffer so the HTTP
    // client frames it with content-length — never a chunked body that could
    // be truncated. Reset the buffered handler so late frames cannot enqueue.
    let mut body = Vec::with_capacity(buffered_bytes);
    for chunk in buffered {
        body.extend_from_slice(&chunk);
    }
    if let Some(StreamEntry::Http(stream)) = shared
        .streams
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get_mut(&stream_id)
    {
        stream.body_tx = None;
    }
    spawn_forward_request(
        shared,
        stream_id,
        generation,
        url,
        method,
        request,
        ForwardBody::Buffered(body),
        loopback_origin,
    );
}

/// Request body crossing the loopback fetch: none, one complete buffered
/// buffer (content-length framed), or a live stream (chunked).
/// 中文：请求 body 穿越 loopback fetch 的三种形态：无 body、完整缓冲的
/// 单块（以 content-length 帧）或实时流（以 chunked 方式）。
enum ForwardBody {
    /// 无请求 body。
    None,
    /// 完整缓冲的 body，HTTP 客户端以 content-length 帧发送。
    Buffered(Vec<u8>),
    /// 流式 body 通道，以 chunked 方式实时转发。
    Live(mpsc::Receiver<Vec<u8>>),
}

/// 在独立任务中启动 `forward_request` 并把句柄记入流条目的 forward_task，
/// 供中止路径取消。
fn spawn_forward_request(
    shared: Arc<Shared>,
    stream_id: u32,
    generation: u64,
    url: String,
    method: String,
    request: Value,
    body: ForwardBody,
    loopback_origin: String,
) {
    let task = tokio::spawn(forward_request(
        shared.clone(),
        stream_id,
        generation,
        url,
        method,
        request,
        body,
        loopback_origin,
    ));
    if let Some(StreamEntry::Http(stream)) = shared
        .streams
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get_mut(&stream_id)
    {
        stream.forward_task = Some(task);
    }
}

/// 执行 loopback HTTP 转发：构建请求头、按 `ForwardBody` 设置请求体并
/// 发送；发送或读取失败时（且流仍属于本代际）中止流。成功后回传
/// HttpResponse 头（剥离帧头），将响应字节流按 `MAX_TUNNEL_PAYLOAD_BYTES`
/// 分块、经背压控制以 HttpBody 帧回传，最后发送 STREAM_END 并清理流。
async fn forward_request(
    shared: Arc<Shared>,
    stream_id: u32,
    generation: u64,
    url: String,
    method: String,
    request: Value,
    body: ForwardBody,
    loopback_origin: String,
) {
    let headers = shared.build_request_headers(
        request.get("headers").unwrap_or(&Value::Null),
        &loopback_origin,
    );
    let mut builder = shared.http.request(
        reqwest::Method::from_bytes(method.as_bytes()).unwrap_or(reqwest::Method::GET),
        &url,
    );
    for (name, value) in headers {
        builder = builder.header(&name, &value);
    }
    match body {
        ForwardBody::None => {}
        ForwardBody::Buffered(bytes) => {
            builder = builder.body(bytes);
        }
        ForwardBody::Live(rx) => {
            let stream = tokio_stream::wrappers::ReceiverStream::new(rx)
                .map(|chunk| Ok::<_, std::io::Error>(Bytes::from(chunk)));
            builder = builder.body(reqwest::Body::wrap_stream(stream));
        }
    }
    let response = match builder.send().await {
        Ok(response) => response,
        Err(error) => {
            if shared.still_ours(stream_id, generation) {
                shared.drop_stream(stream_id);
                shared.send_abort(stream_id, &error.to_string());
            }
            return;
        }
    };

    let mut response_headers = Map::new();
    for (name, value) in response.headers() {
        let name = name.as_str();
        if STRIPPED_RESPONSE_HEADERS.contains(&name) {
            continue;
        }
        if let Ok(value) = value.to_str() {
            response_headers.insert(name.to_string(), Value::String(value.to_string()));
        }
    }
    shared.send_json(
        frame_type::HTTP_RESPONSE,
        stream_id,
        &json!({ "status": response.status().as_u16(), "headers": response_headers }),
    );

    let mut stream = response.bytes_stream();
    loop {
        let chunk = match stream.next().await {
            Some(Ok(chunk)) => chunk,
            Some(Err(error)) => {
                if shared.still_ours(stream_id, generation) {
                    shared.drop_stream(stream_id);
                    shared.send_abort(stream_id, &error.to_string());
                }
                return;
            }
            None => break,
        };
        if shared.closed.load(Ordering::SeqCst) {
            return;
        }
        let pieces = super::tunnel_codec::chunk_payload(&chunk, MAX_TUNNEL_PAYLOAD_BYTES)
            .unwrap_or_default();
        for piece in pieces {
            shared.wait_for_backpressure().await;
            if shared.closed.load(Ordering::SeqCst) {
                return;
            }
            if let Ok(frame) = encode_tunnel_frame(frame_type::HTTP_BODY, stream_id, &piece, false)
            {
                shared.send(&frame);
            }
        }
    }
    if shared.still_ours(stream_id, generation) {
        shared.drop_stream(stream_id);
        if let Ok(frame) = encode_tunnel_frame(frame_type::STREAM_END, stream_id, &[], false) {
            shared.send(&frame);
        }
    }
}

/// WS 流任务：构建升级请求（补 origin、连接标识与子协议头）、连接
/// loopback WS 并回传 WsOpened（含服务器选中的子协议）；随后双工泵送：
/// 出站写本地 WS、入站拆分回隧道帧。处理关闭握手（超时即视为死亡）；
/// 对端关闭/错误/EOF 时按是否已 opened 分别回 WS_CLOSE（1006）或
/// STREAM_ABORT。
async fn run_ws_stream(
    shared: Arc<Shared>,
    stream_id: u32,
    url: String,
    protocols: Vec<String>,
    mut writer_rx: mpsc::UnboundedReceiver<WsOut>,
    opened: Arc<AtomicBool>,
) {
    let port = (shared.deps.get_local_port)();
    // Build the handshake request from the URI (fills in the required
    // Upgrade/Connection/Sec-WebSocket-* headers), then add ours.
    let mut request = match url.as_str().into_client_request() {
        Ok(request) => request,
        Err(error) => {
            shared.send_abort(stream_id, &error.to_string());
            return;
        }
    };
    if let Ok(origin) = format!("http://127.0.0.1:{port}").parse() {
        request.headers_mut().insert("origin", origin);
    }
    if let Ok(connection) = shared.deps.connection_id.parse() {
        request
            .headers_mut()
            .insert("x-ompchamber-relay-connection", connection);
    }
    if !protocols.is_empty() {
        if let Ok(protocol) = protocols.join(", ").parse() {
            request
                .headers_mut()
                .insert("sec-websocket-protocol", protocol);
        }
    }
    let (ws, upgrade_response) = match connect_async(request).await {
        Ok(pair) => pair,
        Err(error) => {
            shared.send_abort(stream_id, &error.to_string());
            return;
        }
    };
    opened.store(true, Ordering::SeqCst);
    let selected_protocol = upgrade_response
        .headers()
        .get("sec-websocket-protocol")
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let opened_payload = match &selected_protocol {
        Some(protocol) if !protocol.is_empty() => json!({ "protocol": protocol }),
        _ => json!({}),
    };
    shared.send_json(frame_type::WS_OPENED, stream_id, &opened_payload);

    let (mut sink, mut inbound) = ws.split();
    // After the closing handshake is initiated, only inbound is polled so
    // the peer's close frame (or reset) is observed and relayed.
    let mut writer_done = false;
    let mut close_deadline: Option<tokio::time::Instant> = None;
    let mut close_code_sent: u16 = 1000;
    loop {
        let outbound = async {
            if writer_done {
                std::future::pending::<Option<WsOut>>().await;
            }
            writer_rx.recv().await
        };
        let close_timer = async {
            match close_deadline {
                Some(at) => tokio::time::sleep_until(at).await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            out = outbound => {
                match out {
                    Some(WsOut::Text(text)) => {
                        if sink.send(Message::Text(text.into())).await.is_err() {
                            break;
                        }
                    }
                    Some(WsOut::Binary(bytes)) => {
                        if sink.send(Message::Binary(bytes.into())).await.is_err() {
                            break;
                        }
                    }
                    Some(WsOut::Close(code, reason)) => {
                        // Initiate the closing handshake (JS `socket.close`),
                        // then keep draining inbound until the peer answers —
                        // the tunnel close notification carries the final code.
                        writer_done = true;
                        close_code_sent = code;
                        close_deadline = Some(
                            tokio::time::Instant::now()
                                + Duration::from_millis(WS_CLOSE_HANDSHAKE_TIMEOUT_MS),
                        );
                        if sink.send(Message::Close(Some(
                            tokio_tungstenite::tungstenite::protocol::CloseFrame {
                                code: code.into(),
                                reason: reason.into(),
                            },
                        ))).await.is_err() {
                            break;
                        }
                    }
                    None if !writer_done => break,
                    None => {}
                }
            }
            _ = close_timer => {
                // The peer never completed the closing handshake; the JS `ws`
                // client still fires 'close' on socket death.
                shared.drop_stream(stream_id);
                shared.send_json(
                    frame_type::WS_CLOSE,
                    stream_id,
                    &json!({ "code": close_code_sent, "reason": "" }),
                );
                break;
            }
            message = inbound.next() => {
                match message {
                    Some(Ok(Message::Text(text))) => {
                        send_ws_frames(&shared, stream_id, frame_type::WS_TEXT, text.as_bytes()).await;
                    }
                    Some(Ok(Message::Binary(bytes))) => {
                        send_ws_frames(&shared, stream_id, frame_type::WS_BINARY, &bytes).await;
                    }
                    Some(Ok(Message::Ping(payload))) => {
                        let _ = sink.send(Message::Pong(payload)).await;
                    }
                    Some(Ok(Message::Close(frame))) => {
                        shared.drop_stream(stream_id);
                        let (code, reason) = frame
                            .map(|frame| (u16::from(frame.code), frame.reason.to_string()))
                            .unwrap_or((1006, String::new()));
                        if opened.load(Ordering::SeqCst) {
                            shared.send_json(
                                frame_type::WS_CLOSE,
                                stream_id,
                                &json!({ "code": if code == 0 { 1000 } else { code }, "reason": reason }),
                            );
                        } else {
                            let detail = if reason.is_empty() {
                                format!("upstream ws closed ({code})")
                            } else {
                                reason
                            };
                            shared.send_abort(stream_id, &detail);
                        }
                        break;
                    }
                    Some(Ok(_)) => {}
                    Some(Err(error)) => {
                        if !opened.load(Ordering::SeqCst) {
                            shared.drop_stream(stream_id);
                            shared.send_abort(stream_id, &error.to_string());
                            break;
                        }
                        // A failed stream never yields a close frame (e.g.
                        // "Connection reset without closing handshake"); the
                        // JS `ws` client fires 'close' 1006 after 'error'.
                        shared.drop_stream(stream_id);
                        shared.send_json(
                            frame_type::WS_CLOSE,
                            stream_id,
                            &json!({ "code": 1006, "reason": "" }),
                        );
                        break;
                    }
                    None => {
                        // TCP EOF without a close frame: abnormal closure.
                        shared.drop_stream(stream_id);
                        if opened.load(Ordering::SeqCst) {
                            shared.send_json(
                                frame_type::WS_CLOSE,
                                stream_id,
                                &json!({ "code": 1006, "reason": "" }),
                            );
                        } else {
                            shared.send_abort(stream_id, "upstream ws closed (1006)");
                        }
                        break;
                    }
                }
            }
        }
    }
}

/// 把一条完整 WS 消息编码为（可能分片的）隧道帧序列，逐帧应用背压后
/// 发送；主机已关闭时立即返回。
async fn send_ws_frames(shared: &Arc<Shared>, stream_id: u32, frame_type: u8, bytes: &[u8]) {
    if shared.closed.load(Ordering::SeqCst) {
        return;
    }
    for frame in encode_fragmented_message(frame_type, stream_id, bytes) {
        shared.wait_for_backpressure().await;
        if shared.closed.load(Ordering::SeqCst) {
            return;
        }
        shared.send(&frame);
    }
}
