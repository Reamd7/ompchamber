//! Desktop control channel: a private Unix-domain socket the desktop shell
//! (Electron) uses to integrate with the spawned server process.
//!
//! Replaces the interim channels — stdout line scraping, authenticated HTTP
//! status polling, and static env-only runtime config — with one protocol:
//! newline-delimited JSON. Requests carry an `id`; responses echo it. Events
//! (`notify`) have none.
//!
//! ```text
//! shell → server:
//!   {"id":1,"op":"hello","token":"…"}                     (first line, auth)
//!   {"id":2,"op":"focus","focused":true}
//!   {"id":3,"op":"runtimeConfig","apiBaseUrl":"…","requestHeaders":{…}}
//!   {"id":4,"op":"quitRisk"}
//!   {"id":5,"op":"engineInfo"}
//!   {"id":6,"op":"shutdown"}
//! server → shell:
//!   {"id":2,"ok":true}
//!   {"id":4,"ok":true,"tunnel":{"active":false},"scheduledTasks":{…}}
//!   {"op":"notify","payload":{…}}
//! ```
//!
//! Discovery: boot writes `<data-dir>/run/desktop-control.json`
//! (`{"path","token"}`); the shell reads it and connects. Enabled via
//! `OMPCHAMBER_DESKTOP_CONTROL=true` (set by the desktop spawn).
//!
//! 中文说明：本模块实现桌面控制通道——一个私有 Unix domain socket（Windows
//! 上为 loopback TCP），供 Electron 桌面壳与被拉起的 server 进程集成。
//! 协议为换行分隔的 JSON：请求带 `id`，响应回显同一 `id`；事件（`notify`）
//! 无 id。启动时把发现信息（socket 路径 + 鉴权 token）写入
//! `<data-dir>/run/desktop-control.json`，桌面壳读取后连接。
//! 通过 `OMPCHAMBER_DESKTOP_CONTROL=true` 启用（由桌面 spawn 设置）。

use std::future::Future;
use std::pin::Pin;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

use std::sync::{Mutex, RwLock};
use rand::RngCore;
use serde_json::{Value, json};

use crate::engine::EngineState;

/// Serialized status provider (scheduled-tasks status shape).
/// 中文说明：返回计划任务状态的序列化 JSON，供 `quitRisk` 应答组装。
pub type StatusProvider = Arc<dyn Fn() -> Value + Send + Sync>;

/// 控制通道的进程级全局状态（OnceLock 惰性初始化，整个进程共享一份）。
struct Globals {
    /// Authenticated client queues — desktop notification fan-out.
    /// 中文说明：已认证客户端的发送队列，用于桌面通知扇出。
    clients: Mutex<Vec<tokio::sync::mpsc::Sender<String>>>,
    /// Shell-pushed window focus (notifications suppress when focused).
    /// 中文说明：壳推送的窗口焦点状态（聚焦时抑制通知）。
    focus: AtomicBool,
    /// Graceful shutdown trigger.
    /// 中文说明：优雅停机触发器（`shutdown` op 通过 notify_one 唤醒等待方）。
    shutdown: tokio::sync::Notify,
    /// Late-bound handles set during boot.
    /// 中文说明：启动阶段晚绑定的引擎句柄，供 `engineInfo` 查询。
    engine: RwLock<Option<Arc<EngineState>>>,
    /// 计划任务状态提供者（晚绑定），供 quitRisk 应答组装。
    scheduler_status: RwLock<Option<StatusProvider>>,
}

/// 进程级全局状态的唯一载体；首次访问时惰性初始化。
static GLOBALS: OnceLock<Globals> = OnceLock::new();

/// 获取（必要时初始化）全局状态的单例引用。
fn globals() -> &'static Globals {
    GLOBALS.get_or_init(|| Globals {
        clients: Mutex::new(Vec::new()),
        focus: AtomicBool::new(false),
        shutdown: tokio::sync::Notify::new(),
        engine: RwLock::new(None),
        scheduler_status: RwLock::new(None),
    })
}

/// 启动阶段晚绑定引擎句柄，供 `engineInfo` op 返回 pid/port 等进程信息。
pub fn set_engine(engine: Arc<EngineState>) {
    *globals().engine.write().unwrap_or_else(|e| e.into_inner()) = Some(engine);
}

/// 注册计划任务状态提供者（计划任务模块在组合阶段调用）。
pub fn set_scheduler_status(provider: StatusProvider) {
    *globals()
        .scheduler_status
        .write()
        .unwrap_or_else(|e| e.into_inner()) = Some(provider);
}

/// Whether any shell is connected over the control channel.
/// 中文说明：任一壳经控制通道完成鉴权握手即视为已连接。
pub fn shell_connected() -> bool {
    !globals().clients.lock().unwrap_or_else(|e| e.into_inner()).is_empty()
}

/// Push a desktop notification payload to every connected shell.
/// 中文说明：把通知负载包装成 `{"op":"notify",...}` 行，投递给所有仍存活
/// 的已连接壳队列；已关闭的队列被即时清理。
pub fn emit_notification(payload: &Value) {
    let line = json!({ "op": "notify", "payload": payload }).to_string();
    let mut clients = globals().clients.lock().unwrap_or_else(|e| e.into_inner());
    clients.retain(|sender| !sender.is_closed());
    for sender in clients.iter() {
        let _ = sender.try_send(format!("{line}\n"));
    }
}

/// Current shell-pushed focus state.
/// 中文说明：读取壳最近一次 `focus` op 推送的焦点状态。
pub fn window_focused() -> bool {
    globals().focus.load(Ordering::SeqCst)
}

/// Resolve once the shell asks for a graceful shutdown.
/// 中文说明：等待壳发出优雅停机请求（`shutdown` op）后返回；
/// `Notify::notify_one` 会预存许可，晚到的等待者立即通过。
pub async fn shutdown_requested() {
    globals().shutdown.notified().await;
}

/// 组装退出风险评估：tunnel 是否活跃 + 计划任务状态（提供者未注册时为空对象）。
fn quit_risk_status() -> Value {
    let tunnel_active = crate::tunnels::tunnel_public_url().is_some();
    let scheduled_tasks = {
        let guard = globals().scheduler_status.read().unwrap_or_else(|e| e.into_inner());
        guard.as_ref().map(|provider| provider()).unwrap_or_else(|| json!({}))
    };
    json!({ "tunnel": { "active": tunnel_active }, "scheduledTasks": scheduled_tasks })
}

/// 组装引擎进程信息：受管子进程返回 (managed, pid, port)，
/// 引擎未绑定或外部模式下返回 managed=false 与 null 字段。
fn engine_info() -> Value {
    let guard = globals().engine.read().unwrap_or_else(|e| e.into_inner());
    match guard.as_ref() {
        Some(engine) => {
            let (managed, pid, port) = engine.managed_process_info();
            json!({ "managed": managed, "pid": pid, "port": port })
        }
        None => json!({ "managed": false, "pid": null, "port": null }),
    }
}

/// Bind the control socket and serve connections until the process exits.
/// Writes the discovery file before returning the listener task handle.
/// 中文说明：先建 `<data-dir>/run` 目录，生成随机 token 并绑定 socket，
/// 写入发现文件，注册通知钩子，最后返回监听任务的 JoinHandle。
/// Unix 用 domain socket，Windows 用 127.0.0.1 随机端口 TCP。
pub async fn serve(data_dir: PathBuf) -> anyhow::Result<tokio::task::JoinHandle<()>> {
    let run_dir = data_dir.join("run");
    std::fs::create_dir_all(&run_dir)?;

    let mut token_bytes = [0u8; 24];
    rand::thread_rng().fill_bytes(&mut token_bytes);
    let token = hex::encode(token_bytes);

    // Unix: domain socket; Windows: loopback TCP. The discovery `path`
    // carries `host:port` on Windows so the shell picks the transport.
    #[cfg(unix)]
    let (discovery, accept): (
        Value,
        Pin<Box<dyn Future<Output = ()> + Send>>,
    ) = {
        let socket_path = run_dir.join("desktop-control.sock");
        let _ = std::fs::remove_file(&socket_path);
        let listener = tokio::net::UnixListener::bind(&socket_path)?;
        let discovery = json!({ "path": socket_path, "token": token });
        let accept_token = listener_token(&discovery);
        let accept = Box::pin(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else { break };
                tokio::spawn(handle_connection(UnixOrTcp::Unix(stream), accept_token.clone()));
            }
        });
        (discovery, accept)
    };

    #[cfg(windows)]
    let (discovery, accept): (
        Value,
        Pin<Box<dyn Future<Output = ()> + Send>>,
    ) = {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await?;
        let port = listener.local_addr()?.port();
        let discovery = json!({ "path": format!("127.0.0.1:{port}"), "token": token });
        let accept_token = listener_token(&discovery);
        let accept = Box::pin(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else { break };
                tokio::spawn(handle_connection(UnixOrTcp::Tcp(stream), accept_token.clone()));
            }
        });
        (discovery, accept)
    };

    std::fs::write(
        run_dir.join("desktop-control.json"),
        format!("{discovery}\n"),
    )?;

    // Register the desktop hooks consumed by the notification stack at
    // composition time (see notifications::State::new).
    crate::notifications::set_desktop_hooks(crate::notifications::DesktopHooks {
        on_notification: Arc::new(|payload: &Value| emit_notification(payload)),
        is_focused: Arc::new(|| window_focused()),
    });

    let task = tokio::spawn(accept);
    Ok(task)
}

/// 从发现 JSON 中取出鉴权 token（供 accept 闭包捕获）。
fn listener_token(discovery: &Value) -> String {
    discovery["token"].as_str().unwrap_or_default().to_string()
}

/// 平台无关的连接流封装：Unix domain socket 或 TCP。
enum UnixOrTcp {
    /// Unix 平台的 domain socket 连接。
    #[cfg(unix)]
    Unix(tokio::net::UnixStream),
    /// Windows 平台的 loopback TCP 连接。
    Tcp(tokio::net::TcpStream),
}

/// 单个控制连接的会话循环：按行读取 JSON 请求，首行必须是携带正确 token 的
/// `hello` 握手（失败即断开）；之后分发 focus/runtimeConfig/quitRisk/
/// engineInfo/shutdown 各 op，未知 op 返回错误应答。出站消息经队列由独立
/// 写任务串行写出，避免读写互相阻塞。
async fn handle_connection(stream: UnixOrTcp, token: String) {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let (reader, mut writer) = match stream {
        #[cfg(unix)]
        UnixOrTcp::Unix(stream) => {
            let (read_half, write_half) = stream.into_split();
            (
                Box::pin(read_half) as Pin<Box<dyn tokio::io::AsyncRead + Send>>,
                Box::pin(write_half) as Pin<Box<dyn tokio::io::AsyncWrite + Send>>,
            )
        }
        UnixOrTcp::Tcp(stream) => {
            let (read_half, write_half) = stream.into_split();
            (
                Box::pin(read_half) as Pin<Box<dyn tokio::io::AsyncRead + Send>>,
                Box::pin(write_half) as Pin<Box<dyn tokio::io::AsyncWrite + Send>>,
            )
        }
    };
    let mut lines = BufReader::new(reader).lines();

    // 每连接一个出站队列，由独立写任务排空；读循环只投递不阻塞在写上。
    // Per-connection outbound queue drained by a writer task.
    let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(64);
    let writer_task = tokio::spawn(async move {
        while let Some(line) = rx.recv().await {
            if writer.write_all(line.as_bytes()).await.is_err() {
                break;
            }
        }
    });

    // 构造成功应答行：回显 id，ok=true，并把 body 的字段合并进顶层。
    let respond = |id: Value, body: Value| -> String {
        let mut response = json!({ "id": id, "ok": true });
        if let (Value::Object(target), Value::Object(extra)) = (&mut response, body) {
            for (key, value) in extra {
                target.insert(key, value);
            }
        }
        format!("{}\n", response)
    };

    // 鉴权前拒绝一切非 hello 消息；token 不匹配直接断开连接。
    let mut authenticated = false;
    while let Ok(Some(line)) = lines.next_line().await {
        let Ok(message) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let id = message.get("id").cloned().unwrap_or(Value::Null);
        let op = message.get("op").and_then(|v| v.as_str()).unwrap_or("");

        if !authenticated {
            if op != "hello" || message.get("token").and_then(|v| v.as_str()) != Some(token.as_str()) {
                return; // wrong or missing handshake → drop
            }
            authenticated = true;
            globals().clients.lock().unwrap_or_else(|e| e.into_inner()).push(tx.clone());
            let _ = tx.send(respond(id, json!({}))).await;
            continue;
        }

        // 分发已鉴权的 op。
        match op {
            "focus" => {
                globals().focus.store(
                    message.get("focused").and_then(|v| v.as_bool()).unwrap_or(false),
                    Ordering::SeqCst,
                );
                let _ = tx.send(respond(id, json!({}))).await;
            }
            "runtimeConfig" => {
                let api_base_url = message
                    .get("apiBaseUrl")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let headers = message.get("requestHeaders").cloned().unwrap_or(json!({}));
                crate::realtime_proxy::set_dynamic_config_from_parts(&api_base_url, &headers);
                let _ = tx.send(respond(id, json!({}))).await;
            }
            "quitRisk" => {
                let _ = tx.send(respond(id, quit_risk_status())).await;
            }
            "engineInfo" => {
                let _ = tx.send(respond(id, engine_info())).await;
            }
            "shutdown" => {
                let _ = tx.send(respond(id, json!({}))).await;
                globals().clients.lock().unwrap_or_else(|e| e.into_inner()).clear();
                // notify_one stores a permit when no waiter is registered yet — a
                // late subscriber returns immediately instead of missing the event.
                globals().shutdown.notify_one();
                // No abort: the queued ack must drain before the writer task
                // ends (all senders drop when this handler returns).
                drop(tx);
                writer_task.await.ok();
                return;
            }
            _ => {
                let _ = tx
                    .send(format!(
                        "{}\n",
                        json!({ "id": id, "ok": false, "error": format!("unknown op {op}") })
                    ))
                    .await;
            }
        }
    }
    globals().clients.lock().unwrap_or_else(|e| e.into_inner()).retain(|client| !client.is_closed());
}

/// 控制通道协议的端到端测试：握手鉴权、各 op 语义、通知扇出与停机信号。
#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    /// 验证：完整控制协议端到端可用——错误 token 被断开；hello 握手成功后
    /// focus/runtimeConfig/quitRisk/shutdown 各 op 行为正确；通知可扇出至
    /// 已连接客户端。
    #[tokio::test]
    async fn control_protocol_end_to_end() {
        let dir = crate::skills_catalog::scan::mkd_temp("dc-test-").unwrap();
        let _task = serve(dir.clone()).await.unwrap();

        let discovery: Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("run/desktop-control.json")).unwrap())
                .unwrap();
        let path = discovery["path"].as_str().unwrap().to_string();
        let token = discovery["token"].as_str().unwrap().to_string();

        // Wrong token is dropped.
        let impostor = tokio::net::UnixStream::connect(&path).await.unwrap();
        let (impostor_reader, mut impostor_writer) = impostor.into_split();
        impostor_writer
            .write_all(b"{\"id\":1,\"op\":\"hello\",\"token\":\"wrong\"}\n")
            .await
            .unwrap();
        drop(impostor_writer);
        let mut impostor_lines = BufReader::new(impostor_reader).lines();
        let closed = impostor_lines.next_line().await.unwrap();
        assert!(closed.is_none(), "impostor connection must be dropped");

        // Correct handshake and ops.
        let stream = tokio::net::UnixStream::connect(&path).await.unwrap();
        let (reader, mut writer) = stream.into_split();
        let mut lines = BufReader::new(reader).lines();

        writer
            .write_all(format!("{}\n", json!({ "id": 1, "op": "hello", "token": token })).as_bytes())
            .await
            .unwrap();
        let hello: Value = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        assert_eq!(hello["id"], 1);
        assert_eq!(hello["ok"], true);

        // focus toggles the shared probe
        writer
            .write_all(format!("{}\n", json!({ "id": 2, "op": "focus", "focused": true })).as_bytes())
            .await
            .unwrap();
        let ack: Value = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        assert_eq!(ack["id"], 2);
        assert!(window_focused());

        // runtimeConfig reaches the realtime proxy provider
        writer
            .write_all(
                format!(
                    "{}\n",
                    json!({ "id": 3, "op": "runtimeConfig", "apiBaseUrl": "http://127.0.0.1:9/", "requestHeaders": { "x-api-key": "tok" } })
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        let ack: Value = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        assert_eq!(ack["id"], 3);
        let active = crate::realtime_proxy::active_config().expect("dynamic config");
        assert_eq!(active.api_base_url, "http://127.0.0.1:9");

        // quitRisk composes tunnel + scheduled status
        writer
            .write_all(format!("{}\n", json!({ "id": 4, "op": "quitRisk" })).as_bytes())
            .await
            .unwrap();
        let risk: Value = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        assert_eq!(risk["id"], 4);
        assert!(risk["tunnel"]["active"].is_boolean());
        assert!(risk["scheduledTasks"].is_object());

        // shutdown resolves the shutdown waiter
        let waiter = tokio::spawn(async {
            tokio::time::timeout(Duration::from_secs(2), shutdown_requested()).await
        });
        writer
            .write_all(format!("{}\n", json!({ "id": 5, "op": "shutdown" })).as_bytes())
            .await
            .unwrap();
        let ack: Value = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        assert_eq!(ack["id"], 5);
        assert!(waiter.await.unwrap().is_ok(), "shutdown must be notified");

        // notification fan-out reaches a connected client (new connection)
        let stream2 = tokio::net::UnixStream::connect(&path).await.unwrap();
        let (reader2, mut writer2) = stream2.into_split();
        writer2
            .write_all(format!("{}\n", json!({ "id": 1, "op": "hello", "token": token })).as_bytes())
            .await
            .unwrap();
        let mut lines2 = BufReader::new(reader2).lines();
        let _ = lines2.next_line().await.unwrap().unwrap();
        let payload = json!({ "title": "hi", "body": "done" });
        emit_notification(&payload);
        let notified: Value =
            serde_json::from_str(&tokio::time::timeout(Duration::from_secs(2), lines2.next_line()).await.unwrap().unwrap().unwrap()).unwrap();
        assert_eq!(notified["op"], "notify");
        assert_eq!(notified["payload"]["title"], "hi");

        // Teardown: these are process-global and must not leak into other
        // notification tests in the same test binary (focus suppression).
        globals().focus.store(false, Ordering::SeqCst);
        globals().clients.lock().unwrap_or_else(|e| e.into_inner()).clear();
        crate::realtime_proxy::set_dynamic_config(None);
        crate::notifications::clear_desktop_hooks();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
