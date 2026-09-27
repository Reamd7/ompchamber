//! Port of `server/lib/dev-tunnel/`: the host-side runtime (`runtime.js`,
//! the `/api/dev-tunnel` WebSocket pipe to loopback dev servers) and the
//! local-end client (`client.js`, binds a loopback listener and pipes each
//! connection through one WebSocket).
//!
//! WS client note: there is no WebSocket-client crate in the allowed set, so
//! the client implements RFC 6455 directly over `TcpStream` (handshake with
//! `Sec-WebSocket-Accept` verification, masked client frames, unmasked
//! server frames, ping/pong). `wss://` (TLS) is not implementable with the
//! allowed crates and fails fast with an explicit error.
//!
//! 中文说明：本模块是 dev-tunnel 的双端实现——宿主侧在 `/api/dev-tunnel`
//! 上把 WebSocket 字节管道接到 loopback dev server；本地侧客户端为每个
//! 目标端口绑定 loopback 监听器，并把每条连接经一条 WebSocket 透传。
//! 由于允许的依赖集合中没有 WebSocket 客户端 crate，客户端基于
//! TcpStream 手写 RFC 6455：握手时校验 Sec-WebSocket-Accept、客户端帧
//! 加掩码、处理 ping/pong；wss://（TLS）无法实现，遇到即快速失败。

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket};
use futures::future::BoxFuture;
use futures::{SinkExt, StreamExt};
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};

/// dev-tunnel WebSocket 升级路径常量（同时也是端口 query 参数的宿主路径）。
pub const DEV_TUNNEL_WS_PATH: &str = "/api/dev-tunnel";
/// One page load opens many sockets; the cap is per host, not per page.
/// 中文补充：上限按宿主进程计，而非按页面计。
const MAX_CONCURRENT_SOCKETS: usize = 64;
/// 连接 loopback dev server 的 TCP 超时（毫秒）。
const CONNECT_TIMEOUT_MS: u64 = 5_000;

/// What one connection may buffer while its WebSocket is still connecting.
/// 中文补充：超限即断开本地连接，防止桌面应用内存无限增长。
const MAX_PENDING_BYTES: usize = 256 * 1024;
/// A handshake that has not completed by now is not going to.
/// 中文补充：防止对端永久挂起的握手僵死。
const HANDSHAKE_TIMEOUT_MS: u64 = 15_000;

/// wss://（TLS）不被本构建支持时的固定错误消息（client.open 直接返回）。
pub const WSS_UNSUPPORTED_MESSAGE: &str =
    "TLS (wss://) dev-tunnel connections are not supported by this build; use an http:// base URL";

// ---------------------------------------------------------------------------
// Host side (runtime.js)
// ---------------------------------------------------------------------------

/// `parseRequestedPort(url)`: the port query param on this exact path.
/// 中文补充：仅接受 1..=65535 的合法端口值，缺失或越界返回 None。
pub fn parse_requested_port(uri: &axum::http::Uri) -> Option<u16> {
    if uri.path() != DEV_TUNNEL_WS_PATH {
        return None;
    }
    let query = uri.query()?;
    for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
        if key == "port" {
            let parsed: i64 = value.parse().ok()?;
            if parsed > 0 && parsed <= 65535 {
                return Some(parsed as u16);
            }
            return None;
        }
    }
    None
}

/// `isDevTunnelPath(url)`: only claims its own upgrade path.
/// 中文补充：把相对 URL 解析到 localhost base 后比对路径，避免误认带
/// 相同前缀的其它 WS 路由。
#[cfg_attr(not(test), allow(dead_code))]
pub fn is_dev_tunnel_path(url: &str) -> bool {
    let Ok(base) = url::Url::parse("http://localhost") else {
        return false;
    };
    url::Url::options()
        .base_url(Some(&base))
        .parse(url)
        .map(|parsed| parsed.path() == DEV_TUNNEL_WS_PATH)
        .unwrap_or(false)
}

/// 升级请求的鉴权通过类型（决定无 Origin 请求的处理策略）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthKind {
    /// 显式 bearer token 的客户端凭据。
    Client,
    /// 浏览器会话（cookie/URL token 等环境凭据）。
    Session,
}

/// 端口发现闭包的返回结果。
#[derive(Debug, Clone)]
pub enum DiscoverOutcome {
    /// 当前探测到的可用 dev server 端口列表。
    Available(Vec<u16>),
    /// 发现阶段不可用（如 dev server 扫描失败），一切端口均拒绝。
    Unavailable,
}

/// 端口发现闭包类型：异步返回当前可用的 dev server 端口集合。
pub type DiscoverFn = Arc<dyn Fn() -> BoxFuture<'static, DiscoverOutcome> + Send + Sync>;
/// 鉴权解析闭包类型：从请求 Parts 同步判定凭据类型；None 表示未认证。
pub type ResolveAuthFn = Arc<dyn Fn(&axum::http::request::Parts) -> Option<AuthKind> + Send + Sync>;

/// dev-tunnel 宿主侧共享状态：并发计数、端口发现、鉴权开关与凭据解析。
#[derive(Clone)]
pub struct DevTunnelState {
    /// 当前打开的隧道 socket 数（AtomicUsize，配合并发上限判断）。
    pub open_sockets: Arc<AtomicUsize>,
    /// 端口发现闭包，每次升级实时调用。
    pub discover: DiscoverFn,
    /// 是否启用 UI 鉴权（配置了非空 ui_password 时为 true）。
    pub auth_enabled: bool,
    /// 凭据解析闭包（同步子集：会话 cookie / URL token）。
    pub resolve_auth: ResolveAuthFn,
}

/// DevTunnelState 的辅助方法。
impl DevTunnelState {
    /// 读取当前打开 socket 数（SeqCst）。
    pub fn open_socket_count(&self) -> usize {
        self.open_sockets.load(Ordering::SeqCst)
    }
}

/// Auth + port preflight (JS `upgradeHandler`): `Ok(port)` proceeds to the
/// WebSocket upgrade, `Err(response)` rejects it with the JS status/message.
/// 中文补充：鉴权失败 401/403、非法端口 400、并发超限 503、未发现端口 403。
pub async fn dev_tunnel_preflight(
    state: &DevTunnelState,
    parts: &axum::http::request::Parts,
) -> Result<u16, axum::response::Response> {
    use crate::ui_auth::{is_request_origin_allowed, reject_websocket_upgrade};

    if state.auth_enabled {
        // JS `resolveAuthContext(req, null, { allowUrlToken: false })`: the
        // URL token is already excluded for this path (it is not an
        // allowlisted WS path), so `guard` covers the resolution.
        let Some(kind) = (state.resolve_auth)(parts) else {
            return Err(reject_websocket_upgrade(401, "UI authentication required"));
        };
        let has_origin = parts
            .headers
            .get(axum::http::header::ORIGIN)
            .and_then(|value| value.to_str().ok())
            .map(|value| !value.trim().is_empty())
            .unwrap_or(false);
        if has_origin {
            if !is_request_origin_allowed(parts) {
                return Err(reject_websocket_upgrade(403, "Invalid origin"));
            }
        } else if !matches!(kind, AuthKind::Client) {
            // Only an explicit bearer may skip the origin check; ambient
            // session credentials are what the origin check protects.
            return Err(reject_websocket_upgrade(
                403,
                "Client authentication required",
            ));
        }
    }

    let Some(port) = parse_requested_port(&parts.uri) else {
        return Err(reject_websocket_upgrade(400, "Invalid port"));
    };
    if state.open_sockets.load(Ordering::SeqCst) >= MAX_CONCURRENT_SOCKETS {
        return Err(reject_websocket_upgrade(503, "Too many tunnel connections"));
    }
    if !is_allowed_port(state, port).await {
        tracing::warn!("[dev-tunnel] refused port {port}: not reported by dev-server discovery");
        return Err(reject_websocket_upgrade(
            403,
            "That port is not an available dev server",
        ));
    }
    Ok(port)
}

/// A port is reachable only while discovery still reports it; re-checked on
/// every upgrade rather than cached.
/// 中文补充：发现不可用（Unavailable）同样视为不允许。
async fn is_allowed_port(state: &DevTunnelState, port: u16) -> bool {
    match (state.discover)().await {
        DiscoverOutcome::Available(ports) => ports.contains(&port),
        DiscoverOutcome::Unavailable => false,
    }
}

/// Post-upgrade pipe: connect TCP to the loopback dev server and pipe bytes
/// both ways; closing either end closes the other.
/// 中文补充：TCP 连接超时则回 Close 帧并释放计数；任一方向出错/关闭即
/// 结束 join，双向随之退出。
pub async fn pipe_dev_tunnel_socket(state: DevTunnelState, port: u16, socket: WebSocket) {
    state.open_sockets.fetch_add(1, Ordering::SeqCst);
    let connect = tokio::net::TcpStream::connect(("127.0.0.1", port));
    let tcp = match tokio::time::timeout(Duration::from_millis(CONNECT_TIMEOUT_MS), connect).await {
        Ok(Ok(stream)) => stream,
        _ => {
            tracing::warn!("[dev-tunnel] timed out connecting to 127.0.0.1:{port}");
            let (mut sender, _receiver) = socket.split();
            let _ = sender.send(Message::Close(None)).await;
            state.open_sockets.fetch_sub(1, Ordering::SeqCst);
            return;
        }
    };
    let _ = tcp.set_nodelay(true);
    let (mut tcp_read, mut tcp_write) = tcp.into_split();
    let (mut ws_sender, mut ws_receiver) = socket.split();

    // WS → TCP.
    let uplink = async {
        while let Some(message) = ws_receiver.next().await {
            match message {
                Ok(Message::Binary(bytes)) => {
                    if tcp_write.write_all(&bytes).await.is_err() {
                        break;
                    }
                }
                Ok(Message::Text(text)) => {
                    if tcp_write.write_all(text.as_bytes()).await.is_err() {
                        break;
                    }
                }
                Ok(Message::Close(_)) => break,
                Ok(_) => {}
                Err(_) => break,
            }
        }
    };

    // TCP → WS. Awaited sends give natural sink backpressure (JS pauses the
    // upstream at 1 MiB of buffered socket data).
    let downlink = async {
        let mut buffer = [0u8; 8192];
        loop {
            match tcp_read.read(&mut buffer).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if ws_sender
                        .send(Message::Binary(buffer[..n].to_vec().into()))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
            }
        }
    };

    tokio::join!(uplink, downlink);
    state.open_sockets.fetch_sub(1, Ordering::SeqCst);
}

// ---------------------------------------------------------------------------
// WS client primitives (RFC 6455 over TcpStream)
// ---------------------------------------------------------------------------

/// RFC 6455 握手固定 GUID，参与 Sec-WebSocket-Accept 计算。
const WS_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

/// SHA-1 (needed only for `Sec-WebSocket-Accept`; sha2 covers SHA-256 but
/// not SHA-1).
/// 中文补充：标准分组消息填充 + 80 轮压缩轮换，输出 20 字节摘要。
pub(crate) fn sha1(data: &[u8]) -> [u8; 20] {
    let mut h: [u32; 5] = [0x67452301, 0xEFCDAB89, 0x98BADCFE, 0x10325476, 0xC3D2E1F0];
    let bit_len = (data.len() as u64) * 8;
    let mut message = data.to_vec();
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&bit_len.to_be_bytes());

    for block in message.chunks(64) {
        let mut w = [0u32; 80];
        for (index, word) in block.chunks(4).enumerate() {
            w[index] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
        }
        for index in 16..80 {
            let value = w[index - 3] ^ w[index - 8] ^ w[index - 14] ^ w[index - 16];
            w[index] = value.rotate_left(1);
        }
        let (mut a, mut b, mut c, mut d, mut e) = (h[0], h[1], h[2], h[3], h[4]);
        for (index, word) in w.iter().enumerate() {
            let (f, k) = match index {
                0..=19 => ((b & c) | ((!b) & d), 0x5A827999u32),
                20..=39 => (b ^ c ^ d, 0x6ED9EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1BBCDC),
                _ => (b ^ c ^ d, 0xCA62C1D6),
            };
            let temp = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(*word);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = temp;
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
    }

    let mut digest = [0u8; 20];
    for (index, word) in h.iter().enumerate() {
        digest[index * 4..index * 4 + 4].copy_from_slice(&word.to_be_bytes());
    }
    digest
}

/// 计算 Sec-WebSocket-Accept：base64(SHA-1(client_key + GUID))。
fn ws_accept_key(client_key: &str) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD
        .encode(sha1(format!("{client_key}{WS_GUID}").as_bytes()))
}

/// `toWebSocketUrl(baseUrl, port)`: resolves `/api/dev-tunnel` against the
/// base, rejects non-http(s) schemes, swaps to ws(s), appends the port.
/// 中文补充：非法 base URL、无法解析的路径或非 http(s) scheme 均返回 Err。
pub fn to_websocket_url(base_url: &str, port: u16) -> Result<String, String> {
    let Ok(base) = url::Url::parse(base_url) else {
        return Err(format!("Invalid remote base URL: {base_url}"));
    };
    let Ok(parsed) = url::Url::options()
        .base_url(Some(&base))
        .parse(DEV_TUNNEL_WS_PATH)
    else {
        return Err(format!("Invalid remote base URL: {base_url}"));
    };
    let scheme = parsed.scheme();
    if scheme != "http" && scheme != "https" {
        return Err(format!(
            "The remote base URL must be http(s); got \"{scheme}\""
        ));
    }
    let ws_scheme = if scheme == "https" { "wss" } else { "ws" };
    let authority = match parsed.port() {
        Some(explicit) => format!("{}:{}", parsed.host_str().unwrap_or_default(), explicit),
        None => parsed.host_str().unwrap_or_default().to_string(),
    };
    Ok(format!(
        "{ws_scheme}://{authority}{DEV_TUNNEL_WS_PATH}?port={port}"
    ))
}

/// 服务端 WS 帧解码后的消息形态。
#[derive(Debug)]
enum ClientFrame {
    /// 二进制数据帧（dev-tunnel 的载荷通道）。
    Binary(Vec<u8>),
    /// 服务端 ping，需要回 pong。
    Ping(Vec<u8>),
    /// 关闭帧或连接结束。
    Close,
}

/// Writes a masked client frame (clients must mask).
/// 中文补充：随机 4 字节掩码，按 RFC 6455 的 7/16/64 位长度编码写头部，
/// 载荷与掩码异或后输出。
fn encode_client_frame(opcode: u8, payload: &[u8]) -> Vec<u8> {
    use rand::RngCore;
    let mut mask = [0u8; 4];
    rand::rng().fill_bytes(&mut mask);
    let mut frame = Vec::with_capacity(payload.len() + 14);
    frame.push(0x80 | opcode);
    let len = payload.len();
    if len < 126 {
        frame.push(0x80 | len as u8);
    } else if len <= u16::MAX as usize {
        frame.push(0x80 | 126);
        frame.extend_from_slice(&(len as u16).to_be_bytes());
    } else {
        frame.push(0x80 | 127);
        frame.extend_from_slice(&(len as u64).to_be_bytes());
    }
    frame.extend_from_slice(&mask);
    for (index, byte) in payload.iter().enumerate() {
        frame.push(byte ^ mask[index % 4]);
    }
    frame
}

/// Reads one complete (defragmented) server message.
/// 中文补充：处理分片（continuation）消息、ping/pong 与控制帧；对流干净
/// 关闭（EOF）返回 Ok(None)，中途协议错误返回 Err。
async fn read_server_frame<R: tokio::io::AsyncRead + Unpin>(
    reader: &mut R,
) -> std::io::Result<Option<ClientFrame>> {
    let mut header = [0u8; 2];
    loop {
        match reader.read_exact(&mut header).await {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => {
                return Ok(None);
            }
            Err(error) => return Err(error),
        }
        let fin = header[0] & 0x80 != 0;
        let opcode = header[0] & 0x0f;
        let masked = header[1] & 0x80 != 0;
        let len = (header[1] & 0x7f) as u64;
        let len = match len {
            126 => {
                let mut extended = [0u8; 2];
                reader.read_exact(&mut extended).await?;
                u16::from_be_bytes(extended) as u64
            }
            127 => {
                let mut extended = [0u8; 8];
                reader.read_exact(&mut extended).await?;
                u64::from_be_bytes(extended)
            }
            other => other,
        };
        let mask = if masked {
            let mut mask = [0u8; 4];
            reader.read_exact(&mut mask).await?;
            Some(mask)
        } else {
            None
        };
        let mut payload = vec![0u8; len as usize];
        if len > 0 {
            reader.read_exact(&mut payload).await?;
        }
        if let Some(mask) = mask {
            for (index, byte) in payload.iter_mut().enumerate() {
                *byte ^= mask[index % 4];
            }
        }

        match opcode {
            0x8 => return Ok(Some(ClientFrame::Close)),
            0x9 => {
                // Ping from the server: handled by the caller (pong reply).
                return Ok(Some(ClientFrame::Ping(payload)));
            }
            0xA => {
                // Pong: keep waiting for data frames.
                if fin {
                    continue;
                }
            }
            0x2 => {
                if fin {
                    return Ok(Some(ClientFrame::Binary(payload)));
                }
                // Fragmented message: accumulate continuations.
                let mut message = payload;
                loop {
                    let mut continuation = [0u8; 2];
                    reader.read_exact(&mut continuation).await?;
                    let continuation_fin = continuation[0] & 0x80 != 0;
                    let continuation_opcode = continuation[0] & 0x0f;
                    if continuation_opcode != 0x0 {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            "unexpected websocket opcode mid-message",
                        ));
                    }
                    let continuation_len = (continuation[1] & 0x7f) as u64;
                    let continuation_len = match continuation_len {
                        126 => {
                            let mut extended = [0u8; 2];
                            reader.read_exact(&mut extended).await?;
                            u16::from_be_bytes(extended) as u64
                        }
                        127 => {
                            let mut extended = [0u8; 8];
                            reader.read_exact(&mut extended).await?;
                            u64::from_be_bytes(extended)
                        }
                        other => other,
                    };
                    let mut chunk = vec![0u8; continuation_len as usize];
                    if continuation_len > 0 {
                        reader.read_exact(&mut chunk).await?;
                    }
                    message.extend_from_slice(&chunk);
                    if continuation_fin {
                        return Ok(Some(ClientFrame::Binary(message)));
                    }
                }
            }
            _ => continue,
        }
    }
}

/// One piped local connection: local TCP ↔ one WebSocket to the host.
/// 中文补充：完成 RFC 6455 客户端握手（校验 101 与 Sec-WebSocket-Accept），
/// 之后本地读、WS 写、WS 读三个任务并发泵送；握手失败或任一方向关闭
/// 即整体退出。等待上行的 pending 字节数超限直接断开。
async fn pipe_client_connection(
    ws_url: &url::Url,
    extra_headers: &[(String, String)],
    local_socket: tokio::net::TcpStream,
    handshake_timeout: Duration,
    max_pending_bytes: usize,
) {
    let host = ws_url.host_str().unwrap_or_default().to_string();
    let port = ws_url.port_or_known_default().unwrap_or(80);
    let upstream = match tokio::net::TcpStream::connect((host.as_str(), port)).await {
        Ok(stream) => stream,
        Err(_) => return,
    };
    let _ = upstream.set_nodelay(true);

    use base64::Engine;
    let key = base64::engine::general_purpose::STANDARD.encode({
        use rand::RngCore;
        let mut bytes = [0u8; 16];
        rand::rng().fill_bytes(&mut bytes);
        bytes
    });

    let target = match ws_url.query() {
        Some(query) => format!("{}?{}", ws_url.path(), query),
        None => ws_url.path().to_string(),
    };
    let mut request = format!(
        "GET {target} HTTP/1.1\r\nHost: {host_header}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n",
        host_header = match ws_url.port() {
            Some(port) => format!("{host}:{port}"),
            None => host.clone(),
        },
    );
    for (name, value) in extra_headers {
        if name.eq_ignore_ascii_case("host")
            || name.eq_ignore_ascii_case("sec-websocket-key")
            || name.eq_ignore_ascii_case("upgrade")
        {
            continue;
        }
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    request.push_str("\r\n");

    let (upstream_read, mut upstream_write) = upstream.into_split();
    let mut reader = BufReader::new(upstream_read);
    let (local_read, local_write) = local_socket.into_split();

    let handshake = async {
        if upstream_write.write_all(request.as_bytes()).await.is_err() {
            return false;
        }
        let _ = upstream_write.flush().await;

        let mut head = Vec::new();
        let mut byte = [0u8; 1];
        loop {
            match reader.read(&mut byte).await {
                Ok(0) => return false,
                Ok(_) => {
                    head.push(byte[0]);
                    if head.ends_with(b"\r\n\r\n") {
                        break;
                    }
                    if head.len() > 16 * 1024 {
                        return false;
                    }
                }
                Err(_) => return false,
            }
        }
        let head = String::from_utf8_lossy(&head).into_owned();
        let status_ok = head
            .lines()
            .next()
            .map(|line| line.contains("101"))
            .unwrap_or(false);
        if !status_ok {
            return false;
        }
        let mut accept_ok = false;
        for line in head.lines().skip(1) {
            let Some((name, value)) = line.split_once(':') else {
                continue;
            };
            if name.trim().eq_ignore_ascii_case("sec-websocket-accept")
                && value.trim() == ws_accept_key(&key)
            {
                accept_ok = true;
            }
        }
        accept_ok
    };

    let handshake_ok: bool = tokio::time::timeout(handshake_timeout, handshake)
        .await
        .unwrap_or_default();
    if !handshake_ok {
        return;
    }

    // Pipe phase: the local socket was held back while there was nowhere to
    // put its bytes; the pending buffer is bounded by `max_pending_bytes`.
    // 本地连接产出的上行消息：数据载荷或对 ping 的 pong 应答。
    enum ToServer {
        // 本地 socket 读到的数据字节。
        Data(Vec<u8>),
        // 对服务端 ping 的 pong 载荷。
        Pong(Vec<u8>),
    }
    let (to_server_tx, mut to_server_rx) = tokio::sync::mpsc::channel::<ToServer>(64);

    let reader_tx = to_server_tx.clone();
    let local_reader = async move {
        let mut local_read = local_read;
        let mut buffer = [0u8; 8192];
        loop {
            match local_read.read(&mut buffer).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if reader_tx
                        .send(ToServer::Data(buffer[..n].to_vec()))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
            }
        }
    };

    let ws_writer = async move {
        let mut writer = upstream_write;
        let mut pending_bytes = 0usize;
        while let Some(message) = to_server_rx.recv().await {
            let (opcode, payload) = match message {
                ToServer::Data(payload) => (0x2u8, payload),
                ToServer::Pong(payload) => (0xAu8, payload),
            };
            pending_bytes += payload.len();
            if pending_bytes > max_pending_bytes {
                // A local process writing into a stalled handshake must not
                // grow the desktop app's memory (JS drops the connection).
                break;
            }
            let frame = encode_client_frame(opcode, &payload);
            if writer.write_all(&frame).await.is_err() || writer.flush().await.is_err() {
                break;
            }
            pending_bytes = 0;
        }
    };

    let ws_reader = async move {
        let mut reader = reader;
        let mut local_write = local_write;
        loop {
            match read_server_frame(&mut reader).await {
                Ok(Some(ClientFrame::Binary(payload))) => {
                    if local_write.write_all(&payload).await.is_err() {
                        break;
                    }
                }
                Ok(Some(ClientFrame::Ping(payload))) => {
                    let _ = to_server_tx.send(ToServer::Pong(payload)).await;
                }
                Ok(Some(ClientFrame::Close)) | Ok(None) | Err(_) => break,
            }
        }
    };

    tokio::join!(local_reader, ws_writer, ws_reader);
}

// ---------------------------------------------------------------------------
// Client (client.js)
// ---------------------------------------------------------------------------

/// list() 输出的隧道摘要（序列化为 camelCase JSON）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ListedTunnel {
    /// 本地侧监听端口（浏览器连接它）。
    #[serde(rename = "localPort")]
    pub local_port: u16,
    /// 远端被代理的 dev server 端口。
    #[serde(rename = "remotePort")]
    pub remote_port: u16,
    /// 远端 base URL（去尾斜杠后的字符串形式）。
    #[serde(rename = "baseUrl")]
    pub base_url: String,
}

/// tunnels 表中的活跃隧道条目。
struct TunnelEntry {
    /// 本地侧监听端口。
    local_port: u16,
    /// 远端被代理的端口。
    remote_port: u16,
    /// 远端 base URL（构造 key 的一部分）。
    base_url: String,
    /// 关停信号：send(true) 让接受循环退出。
    shutdown: tokio::sync::watch::Sender<bool>,
}

/// 本地端 dev-tunnel 客户端：维护 (baseUrl, port) → 本地监听器的隧道表。
pub struct DevTunnelClient {
    /// 活跃隧道表，key 为 `base|port`。
    tunnels: Arc<Mutex<HashMap<String, TunnelEntry>>>,
    /// 单连接握手超时。
    handshake_timeout: Duration,
    /// 握手期间允许缓存的最大 pending 字节数。
    max_pending_bytes: usize,
}

/// open() 的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpenOutcome {
    /// 本次（或复用的）本地监听端口。
    pub local_port: u16,
    /// 是否复用了已存在的隧道。
    pub reused: bool,
}

/// 默认构造：等价 new()。
impl Default for DevTunnelClient {
    /// 委托给 new()（使用默认超时配置）。
    fn default() -> Self {
        Self::new()
    }
}

/// 隧道的打开、关闭与列举。
impl DevTunnelClient {
    /// 以默认握手超时（15s）与 pending 上限（256 KiB）构造客户端。
    pub fn new() -> Self {
        Self {
            tunnels: Arc::new(Mutex::new(HashMap::new())),
            handshake_timeout: Duration::from_millis(HANDSHAKE_TIMEOUT_MS),
            max_pending_bytes: MAX_PENDING_BYTES,
        }
    }

    /// 以自定义握手超时与 pending 上限构造客户端（测试用）。
    pub fn with_timeouts(handshake_timeout: Duration, max_pending_bytes: usize) -> Self {
        Self {
            tunnels: Arc::new(Mutex::new(HashMap::new())),
            handshake_timeout,
            max_pending_bytes,
        }
    }

    /// `open({ baseUrl, port, headers })`: opens (or reuses) a tunnel and
    /// resolves with the local port to browse.
    /// 中文补充：先做端口与 base URL 校验，再按 `base|port` 复用已有隧道；
    /// wss:// 显式拒绝；随后绑定 loopback 随机端口并 spawn 接受循环
    /// （每条本地连接各走一条 WebSocket），登记后返回本地端口。
    pub async fn open(
        &self,
        base_url: &str,
        port: u16,
        headers: Vec<(String, String)>,
    ) -> Result<OpenOutcome, String> {
        // JS validates a parsed Number; a u16 is in range by construction, so
        // only the zero port is invalid here.
        if port == 0 {
            return Err("A valid remote port is required".to_string());
        }
        let base = base_url.trim();
        if base.is_empty() {
            return Err("A remote base URL is required".to_string());
        }

        let key = format!("{base}|{port}");
        {
            let tunnels = self.tunnels.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(existing) = tunnels.get(&key) {
                return Ok(OpenOutcome {
                    local_port: existing.local_port,
                    reused: true,
                });
            }
        }

        let ws_url = to_websocket_url(base, port)?;
        if ws_url.starts_with("wss://") {
            return Err(WSS_UNSUPPORTED_MESSAGE.to_string());
        }
        let ws_url =
            url::Url::parse(&ws_url).map_err(|error| format!("Invalid tunnel URL: {error}"))?;

        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .map_err(|error| error.to_string())?;
        let local_port = listener
            .local_addr()
            .map(|address| address.port())
            .map_err(|_| "Failed to bind a local tunnel port".to_string())?;

        let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);
        let handshake_timeout = self.handshake_timeout;
        let max_pending_bytes = self.max_pending_bytes;
        let headers = Arc::new(headers);
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        match accepted {
                            Ok((socket, _)) => {
                                let ws_url = ws_url.clone();
                                let headers = headers.clone();
                                tokio::spawn(async move {
                                    pipe_client_connection(
                                        &ws_url,
                                        &headers,
                                        socket,
                                        handshake_timeout,
                                        max_pending_bytes,
                                    )
                                    .await;
                                });
                            }
                            Err(_) => break,
                        }
                    }
                    _ = shutdown_rx.changed() => {
                        if *shutdown_rx.borrow() {
                            break;
                        }
                    }
                }
            }
        });

        self.tunnels
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(
                key,
                TunnelEntry {
                    local_port,
                    remote_port: port,
                    base_url: base.to_string(),
                    shutdown: shutdown_tx,
                },
            );
        Ok(OpenOutcome {
            local_port,
            reused: false,
        })
    }

    /// `close({ baseUrl, port })`: returns whether a tunnel was closed.
    /// 中文补充：命中 key 才移除条目并触发关停信号。
    pub fn close(&self, base_url: &str, port: u16) -> bool {
        let key = format!("{}|{port}", base_url.trim());
        self.close_key(&key)
    }

    /// 按 key 移除隧道并发送关停信号；条目不存在返回 false。
    fn close_key(&self, key: &str) -> bool {
        let mut tunnels = self.tunnels.lock().unwrap_or_else(|e| e.into_inner());
        let Some(entry) = tunnels.remove(key) else {
            return false;
        };
        let _ = entry.shutdown.send(true);
        true
    }

    /// `closeAll()`.
    /// 中文补充：逐个 close_key，表最终清空。
    pub fn close_all(&self) {
        let keys: Vec<String> = {
            let tunnels = self.tunnels.lock().unwrap_or_else(|e| e.into_inner());
            tunnels.keys().cloned().collect()
        };
        for key in keys {
            self.close_key(&key);
        }
    }

    /// `list()`.
    /// 中文补充：返回全部活跃隧道的 camelCase 摘要。
    pub fn list(&self) -> Vec<ListedTunnel> {
        let tunnels = self.tunnels.lock().unwrap_or_else(|e| e.into_inner());
        tunnels
            .values()
            .map(|entry| ListedTunnel {
                local_port: entry.local_port,
                remote_port: entry.remote_port,
                base_url: entry.base_url.clone(),
            })
            .collect()
    }
}

/// 模块内单元测试（测试体位于 dev_tunnel_tests.rs）。
#[cfg(test)]
#[path = "dev_tunnel_tests.rs"]
mod dev_tunnel_tests;

/// 测试可见的内部直通（把私有 sha1 以 test_exports::sha1_for_tests 暴露）。
#[cfg(test)]
pub(crate) mod test_exports {
    //! Test-visible shims for the WS client internals.
    pub(crate) use super::sha1 as sha1_for_tests;
}
