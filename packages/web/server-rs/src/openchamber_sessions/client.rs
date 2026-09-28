//! Port of the `server/lib/opencode/local-engine-client.js` subset the
//! OMPChamber session routes consume, plus the raw-`fetch` calls the routes
//! make directly (`createSession`, `runPromptAsync`, `fetchJson`).
//!
//! The JS client wraps every SDK call in the `{ data, error, response }`
//! convention and **never rejects for HTTP outcomes** (transport failures are
//! captured into `error`); [`EngineReply`] mirrors that shape and the
//! [`EngineClient`] trait is the DI seam the JS tests replace with mocks.
//! The fetch-based helpers keep their throwing semantics (`Result`).
//!
//! 中文说明：本地 engine 客户端的 Rust 移植——JS `local-engine-client.js`
//! 中被会话路由消费的子集（`{ data, error, response }` 约定，HTTP 结果从不
//! reject），加上路由直接发起的 raw fetch 调用（`createSession`、
//! `runPromptAsync`、`fetchJson`，保持抛错/`Result` 语义）。
//! `EngineClient` trait 是 JS 测试用 mock 替换的依赖注入接缝。

use std::pin::Pin;
use std::sync::Arc;

use serde_json::Value;

use crate::engine::EngineState;

/// 盒装 future 类型别名（`Pin<Box<dyn Future>>`），让注入闭包与 trait 方法
/// 能以对象安全的形式返回异步结果。
pub type BoxFut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// `error` half of the `{ data, error, response }` convention.
///
/// routes.js never reads this half (only `data.id`/`data` arrays gate
/// behavior), so the fields carry the convention shape for the future
/// shared local-engine-client module rather than route logic.
///
/// 中文说明：错误信封的 `name`/`message` 两字段；路由逻辑从不读取这一半，
/// 保留是为了对齐 wire 约定、供未来共享的 local-engine-client 模块使用。
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct EngineCallError {
    /// 错误类型名（缺省 `UnknownError`）。
    pub name: String,
    /// 人类可读的错误消息。
    pub message: String,
}

/// The SDK result convention: `data` is the parsed body of a 2xx response,
/// `error` carries the engine's error envelope otherwise, `status` is the
/// HTTP status when a response arrived. As in the JS client, only `data`
/// drives route behavior; the other halves document the wire convention.
///
/// 中文说明：引擎调用的统一返回体——2xx 时 `data` 为解析后的 body；非 2xx
/// 时 `error` 携带错误信封；`status` 记录到达响应时的 HTTP 状态码，传输
/// 失败（请求未送达）为 `None`。与 JS 客户端一致，仅 `data` 驱动路由行为。
#[derive(Debug, Clone)]
pub struct EngineReply {
    /// 2xx 响应解析出的 body；非 2xx 或传输失败时为 `None`。
    pub data: Option<Value>,
    #[allow(dead_code)]
    /// 非 2xx 时的错误信封（约定中的 error 半边）。
    pub error: Option<EngineCallError>,
    #[allow(dead_code)]
    /// HTTP 状态码；传输失败时为 `None`。
    pub status: Option<u16>,
}

/// 快捷构造：成功应答与传输失败应答。
impl EngineReply {
    /// A 2xx reply carrying the parsed body.
    ///
    /// 中文说明：构造携带解析 body 的成功应答（状态记为 200，主要供 mock 用）。
    pub fn ok(data: Value) -> Self {
        Self {
            data: Some(data),
            error: None,
            status: Some(200),
        }
    }

    /// 构造传输失败应答：无 data、无状态码，错误名固定 `UnknownError`。
    fn transport_error(message: String) -> Self {
        Self {
            data: None,
            error: Some(EngineCallError {
                name: "UnknownError".to_string(),
                message,
            }),
            status: None,
        }
    }
}

/// Parameters of `client.session.command` minus `sessionID` (the SDK sends
/// the rest as the request body).
///
/// 中文说明：`client.session.command` 的请求体参数（`sessionID` 走 URL
/// 路径，其余字段序列化为 body）。
#[derive(Debug, Clone)]
pub struct SessionCommandParams {
    /// 会话所在目录。
    pub directory: String,
    /// slash 命令名（不含 `/`）。
    pub command: String,
    /// 命令参数原文。
    pub arguments: String,
    /// 可选 agent 名。
    pub agent: Option<String>,
    /// `"providerID/modelID"`.
    /// `"providerID/modelID"` 斜杠串（`session.command` 的 wire 形式）。
    pub model: String,
    /// 可选模型 variant。
    pub variant: Option<String>,
}

/// The engine seam used by the session routes: the local-engine-client
/// subset plus the raw fetch helpers. `Err` from the SDK-style methods
/// mirrors a rejecting mock (the HTTP implementation never rejects, exactly
/// like the JS client's try/catch).
///
/// 中文说明：会话路由依赖的 engine 接缝。SDK 风格方法返回 `EngineReply` 且
/// HTTP 实现从不返回 `Err`（对齐 JS 客户端的 try/catch；`Err` 只出现在
/// 拒绝式 mock 中）；raw fetch 帮助函数保持 JS 的抛错/回退语义。
pub trait EngineClient: Send + Sync {
    /// `POST /session/:id/fork`：复制会话，可选从某条消息分叉。
    fn session_fork(
        &self,
        session_id: &str,
        directory: &str,
        message_id: Option<&str>,
    ) -> BoxFut<'_, Result<EngineReply, String>>;
    /// `GET /session/:id/message`：拉取会话最近的消息列表。
    fn session_messages(
        &self,
        session_id: &str,
        directory: &str,
        limit: u32,
    ) -> BoxFut<'_, Result<EngineReply, String>>;
    /// `POST /session/:id/command`：以 slash 命令形式派发。
    fn session_command(
        &self,
        session_id: &str,
        params: &SessionCommandParams,
    ) -> BoxFut<'_, Result<EngineReply, String>>;
    /// `GET /command`：列出目录可用的命令（用于命令名校验）。
    fn command_list(&self, directory: &str) -> BoxFut<'_, Result<EngineReply, String>>;
    /// `fetchJson(url, authHeaders, fallback, directory)` — resolves to the
    /// fallback on any non-2xx or unparseable response.
    ///
    /// 中文说明：对应 JS `fetchJson`——任何非 2xx 或不可解析的响应都回退到
    /// fallback 值，永不失败。
    fn fetch_json(&self, path: &str, directory: &str, fallback: Value) -> BoxFut<'_, Value>;
    /// `createSession` — throws `session create failed (status): body` /
    /// `failed to create session`.
    ///
    /// 中文说明：对应 JS `createSession`——失败抛
    /// `session create failed (status): body` / `failed to create session`。
    fn create_session(
        &self,
        directory: &str,
        title: Option<&str>,
    ) -> BoxFut<'_, Result<String, String>>;
    /// `runPromptAsync` — throws `prompt_async failed (status): body`.
    ///
    /// 中文说明：对应 JS `runPromptAsync`——失败抛
    /// `prompt_async failed (status): body`。
    fn prompt_async(
        &self,
        session_id: &str,
        directory: &str,
        payload: &Value,
    ) -> BoxFut<'_, Result<(), String>>;
}

/// `encodeURIComponent` (RFC 3986 unreserved + JS literal extras).
///
/// 中文说明：复刻 JS `encodeURIComponent`——保留字母数字与 `!'()*-._`，
/// 其余字节（含非 ASCII 的 UTF-8 逐字节）转为大写 `%XX` 转义。
pub fn encode_uri_component(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for &byte in value.as_bytes() {
        let keep = byte.is_ascii_alphanumeric()
            || matches!(
                byte,
                b'-' | b'_' | b'.' | b'!' | b'~' | b'*' | b'\'' | b'(' | b')'
            );
        if keep {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// `URLSearchParams.set('directory', …).toString()` — form-urlencoding
/// (spaces become `+`, slashes `%2F`).
///
/// 中文说明：复刻 `URLSearchParams.set('directory', …).toString()`——
/// form-urlencoding（空格变 `+`、斜杠变 `%2F`）。
fn directory_query(directory: &str) -> String {
    url::form_urlencoded::Serializer::new(String::new())
        .append_pair("directory", directory)
        .finish()
}

/// HTTP-backed client over the managed engine (`createLocalEngineClient({
/// baseUrl, headers: authHeaders })` + the routes' raw fetch calls). The
/// client is constructed without a `directory`, so SDK calls carry the
/// directory in the body/query and no `x-opencode-directory` header; the
/// raw fetch helpers set it percent-encoded like the official SDK.
///
/// 中文说明：基于受管 engine 的 HTTP 实现——SDK 风格调用把 directory 放在
/// body/query（不设 `x-opencode-directory` 头），raw fetch 帮助函数则像官方
/// SDK 一样设置 percent-encoded 的 directory 头。
pub struct HttpEngineClient {
    /// 受管 engine 共享状态（base URL、auth 头、HTTP 客户端、就绪等待）。
    engine: Arc<EngineState>,
}

/// HTTP 实现的私有辅助：base URL 解析、auth 头，以及 SDK/raw 两种请求构造。
impl HttpEngineClient {
    /// 以受管 engine 状态构造客户端。
    pub fn new(engine: Arc<EngineState>) -> Self {
        Self { engine }
    }

    /// 去掉尾部 `/` 的 base URL；engine 无端口时返回 "engine unavailable"。
    fn base(&self) -> Result<String, String> {
        let base = self.engine.base_url().unwrap_or_default();
        let trimmed = base.strip_suffix('/').unwrap_or(&base).to_string();
        if trimmed.is_empty() {
            return Err("engine unavailable".to_string());
        }
        Ok(trimmed)
    }

    /// engine 的 Authorization 头（bearer token）；未配置时为 `None`。
    fn auth(&self) -> Option<String> {
        self.engine.auth_header()
    }

    /// One SDK-convention request: parse the text body, map non-2xx into the
    /// error envelope, capture transport failures.
    ///
    /// 中文说明：SDK 约定的单次请求——附 auth 与 JSON body，读文本响应；
    /// 2xx 装入 `data`，非 2xx 映射为错误信封，传输失败捕获为 transport
    /// error；任何情况都不返回 `Err`。
    async fn sdk_request(
        &self,
        method: reqwest::Method,
        url: String,
        pathname: &str,
        body: Option<Value>,
    ) -> Result<EngineReply, String> {
        let mut request = self.engine.http().request(method, url);
        if let Some(auth) = self.auth() {
            request = request.header("authorization", auth);
        }
        if let Some(payload) = body {
            request = request
                .header("content-type", "application/json")
                .json(&payload);
        }
        let response = match request.send().await {
            Ok(response) => response,
            Err(error) => return Ok(EngineReply::transport_error(error.to_string())),
        };
        let status = response.status().as_u16();
        let text = response.text().await.unwrap_or_default();
        let payload: Option<Value> = if text.is_empty() {
            None
        } else {
            match serde_json::from_str(&text) {
                Ok(value) => Some(value),
                Err(_) => Some(serde_json::json!({ "message": text })),
            }
        };
        if (200..300).contains(&status) {
            return Ok(EngineReply {
                data: payload,
                error: None,
                status: Some(status),
            });
        }
        let message = payload
            .as_ref()
            .and_then(|p| p.get("data").and_then(|d| d.get("message")))
            .or_else(|| payload.as_ref().and_then(|p| p.get("message")))
            .and_then(Value::as_str)
            .map(String::from)
            .unwrap_or_else(|| format!("{pathname} failed with {status}"));
        let name = payload
            .as_ref()
            .and_then(|p| p.get("name"))
            .and_then(Value::as_str)
            .unwrap_or("UnknownError")
            .to_string();
        Ok(EngineReply {
            data: None,
            error: Some(EngineCallError { name, message }),
            status: Some(status),
        })
    }

    /// The raw-fetch header set: auth + percent-encoded directory + accept
    /// (+ content-type when a body is sent).
    ///
    /// 中文说明：raw fetch 的公共请求头——auth + percent-encoded 的
    /// `x-opencode-directory` + accept；带 body 时再加 content-type。
    fn raw_fetch_request(
        &self,
        method: reqwest::Method,
        url: &str,
        directory: &str,
        body: Option<&Value>,
    ) -> reqwest::RequestBuilder {
        let mut request = self
            .engine
            .http()
            .request(method, url)
            .header("accept", "application/json")
            .header("x-opencode-directory", encode_uri_component(directory));
        if let Some(auth) = self.auth() {
            request = request.header("authorization", auth);
        }
        if let Some(payload) = body {
            request = request
                .header("content-type", "application/json")
                .json(&payload);
        }
        request
    }
}

/// trait 的 HTTP 实现：SDK 方法经 `sdk_request` 走 `{ data, error }` 约定；
/// raw fetch 方法保持抛错/回退语义。
impl EngineClient for HttpEngineClient {
    /// fork 会话；engine 不可用时返回 transport error 而非 `Err`。
    fn session_fork(
        &self,
        session_id: &str,
        directory: &str,
        message_id: Option<&str>,
    ) -> BoxFut<'_, Result<EngineReply, String>> {
        let url = match self.base() {
            Ok(base) => format!("{base}/session/{}/fork", encode_uri_component(session_id)),
            Err(error) => return Box::pin(async move { Ok(EngineReply::transport_error(error)) }),
        };
        let mut body = serde_json::Map::new();
        body.insert(
            "directory".to_string(),
            Value::String(directory.to_string()),
        );
        if let Some(message_id) = message_id {
            body.insert(
                "messageID".to_string(),
                Value::String(message_id.to_string()),
            );
        }
        let body = Value::Object(body);
        Box::pin(self.sdk_request(reqwest::Method::POST, url, "/session/fork", Some(body)))
    }

    /// 拉取会话消息；directory 走查询串，limit 拼接在 URL 上。
    fn session_messages(
        &self,
        session_id: &str,
        directory: &str,
        limit: u32,
    ) -> BoxFut<'_, Result<EngineReply, String>> {
        let url = match self.base() {
            Ok(base) => format!(
                "{base}/session/{}/message?{}&limit={limit}",
                encode_uri_component(session_id),
                directory_query(directory),
            ),
            Err(error) => return Box::pin(async move { Ok(EngineReply::transport_error(error)) }),
        };
        Box::pin(self.sdk_request(reqwest::Method::GET, url, "/session/message", None))
    }

    /// 以命令派发；参数序列化为 JSON body（agent/variant 可省略）。
    fn session_command(
        &self,
        session_id: &str,
        params: &SessionCommandParams,
    ) -> BoxFut<'_, Result<EngineReply, String>> {
        let url = match self.base() {
            Ok(base) => format!(
                "{base}/session/{}/command",
                encode_uri_component(session_id)
            ),
            Err(error) => return Box::pin(async move { Ok(EngineReply::transport_error(error)) }),
        };
        let mut body = serde_json::Map::new();
        body.insert(
            "directory".to_string(),
            Value::String(params.directory.clone()),
        );
        body.insert("command".to_string(), Value::String(params.command.clone()));
        body.insert(
            "arguments".to_string(),
            Value::String(params.arguments.clone()),
        );
        if let Some(agent) = &params.agent {
            body.insert("agent".to_string(), Value::String(agent.clone()));
        }
        body.insert("model".to_string(), Value::String(params.model.clone()));
        if let Some(variant) = &params.variant {
            body.insert("variant".to_string(), Value::String(variant.clone()));
        }
        let body = Value::Object(body);
        Box::pin(self.sdk_request(reqwest::Method::POST, url, "/session/command", Some(body)))
    }

    /// 列出目录命令；directory 走查询串。
    fn command_list(&self, directory: &str) -> BoxFut<'_, Result<EngineReply, String>> {
        let url = match self.base() {
            Ok(base) => format!("{base}/command?{}", directory_query(directory)),
            Err(error) => return Box::pin(async move { Ok(EngineReply::transport_error(error)) }),
        };
        Box::pin(self.sdk_request(reqwest::Method::GET, url, "/command", None))
    }

    /// raw GET + 回退语义：engine 不可用、请求失败、非 2xx、解析失败一律
    /// 返回 fallback。
    fn fetch_json(&self, path: &str, directory: &str, fallback: Value) -> BoxFut<'_, Value> {
        let directory = directory.to_string();
        let url = match self.base() {
            Ok(base) => format!("{base}{path}?{}", directory_query(&directory)),
            Err(_) => return Box::pin(async move { fallback }),
        };
        Box::pin(async move {
            let request = self.raw_fetch_request(reqwest::Method::GET, &url, &directory, None);
            let Ok(response) = request.send().await else {
                return fallback;
            };
            if !response.status().is_success() {
                return fallback;
            }
            response.json::<Value>().await.unwrap_or(fallback)
        })
    }

    /// raw POST /session：非 2xx 或响应缺 id 时返回 `Err`，错误消息与 JS
    /// 抛错文本一致。
    fn create_session(
        &self,
        directory: &str,
        title: Option<&str>,
    ) -> BoxFut<'_, Result<String, String>> {
        let directory = directory.to_string();
        let title = title.map(String::from);
        let url = match self.base() {
            Ok(base) => format!("{base}/session?{}", directory_query(&directory)),
            Err(error) => return Box::pin(async move { Err(error) }),
        };
        let mut body = serde_json::Map::new();
        body.insert("directory".to_string(), Value::String(directory.clone()));
        if let Some(title) = &title {
            body.insert("title".to_string(), Value::String(title.clone()));
        }
        let body = Value::Object(body);
        Box::pin(async move {
            let request =
                self.raw_fetch_request(reqwest::Method::POST, &url, &directory, Some(&body));
            let response = request
                .send()
                .await
                .map_err(|_| "session create failed".to_string())?;
            let status = response.status().as_u16();
            let text = response.text().await.unwrap_or_default();
            if !(200..300).contains(&status) {
                let detail = if text.is_empty() {
                    String::new()
                } else {
                    format!(": {text}")
                };
                return Err(format!("session create failed ({status}){detail}"));
            }
            let payload: Option<Value> = serde_json::from_str(&text).ok();
            let id = payload
                .as_ref()
                .and_then(|p| p.get("id"))
                .or_else(|| {
                    payload
                        .as_ref()
                        .and_then(|p| p.get("data").and_then(|d| d.get("id")))
                })
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
                .map(String::from);
            id.ok_or_else(|| "failed to create session".to_string())
        })
    }

    /// raw POST /session/:id/prompt_async：非 2xx 时返回 `Err`，成功不解析
    /// body。
    fn prompt_async(
        &self,
        session_id: &str,
        directory: &str,
        payload: &Value,
    ) -> BoxFut<'_, Result<(), String>> {
        let directory = directory.to_string();
        let url = match self.base() {
            Ok(base) => format!(
                "{base}/session/{}/prompt_async?{}",
                encode_uri_component(session_id),
                directory_query(&directory),
            ),
            Err(error) => return Box::pin(async move { Err(error) }),
        };
        let payload = payload.clone();
        Box::pin(async move {
            let request =
                self.raw_fetch_request(reqwest::Method::POST, &url, &directory, Some(&payload));
            let response = request
                .send()
                .await
                .map_err(|_| "prompt_async failed".to_string())?;
            let status = response.status().as_u16();
            let text = response.text().await.unwrap_or_default();
            if !(200..300).contains(&status) {
                let detail = if text.is_empty() {
                    String::new()
                } else {
                    format!(": {text}")
                };
                return Err(format!("prompt_async failed ({status}){detail}"));
            }
            Ok(())
        })
    }
}

/// URL 转义与 JS `encodeURIComponent` 行为一致性的单元测试。
#[cfg(test)]
mod tests {
    use super::*;

    /// 验证转义结果与 JS `encodeURIComponent` 逐例一致：路径分隔符、非 ASCII
    /// （UTF-8 逐字节转义）、空格，以及安全字符原样保留。
    #[test]
    fn encodes_like_js_encode_uri_component() {
        assert_eq!(encode_uri_component("/repo/app"), "%2Frepo%2Fapp");
        assert_eq!(
            encode_uri_component("/home/user/Masaüstü/projeler"),
            "%2Fhome%2Fuser%2FMasa%C3%BCst%C3%BC%2Fprojeler"
        );
        assert_eq!(encode_uri_component("a b"), "a%20b");
        assert_eq!(encode_uri_component("ses_123"), "ses_123");
    }
}
