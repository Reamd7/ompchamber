//! Direct GitHub REST client replacing `@octokit/rest` (`octokit.js`).
//!
//! JS precedent (`server/lib/github/octokit.js` + the installed octokit
//! packages): every REST call goes to `https://api.github.com` with
//! `accept: application/vnd.github.v3+json`, `authorization: token <t>`
//! (`bearer` for JWT-shaped tokens) and an 8s per-request timeout; GETs go
//! through a conditional-request ETag cache (304 replayed as 200, cache keyed
//! by `token\nurl`, max 300 entries, LRU touch) so unchanged polls do not
//! count against the REST rate limit.
//!
//! Error semantics mirror `@octokit/request`'s `fetch-wrapper.js` /
//! `request-error`: status >= 400 raises an error whose message is built by
//! `toErrorMessage(data)` (`message: errors…` for validation payloads), with
//! `status`, response headers and parsed body attached. 304 without a cached
//! entry surfaces as "Not modified".
//!
//! The transport is a seam (`Transport`) so tests drive routes against a fake
//! instead of api.github.com.
//!
//! 中文概述：替代 `@octokit/rest` 的 GitHub REST 客户端。所有请求走可替换的
//! `Transport` 接缝，生产实现为 reqwest（8 秒超时 + ETag 条件请求缓存），
//! 测试注入 fake 应答。URL 编码、错误消息拼接等细节逐一复刻 octokit，
//! 保证与旧 JS server 的对外行为一致。

use std::sync::{Arc, LazyLock, Mutex};

use futures::future::BoxFuture;
use serde_json::Value;

/// `octokit.js` `OCTOKIT_REQUEST_TIMEOUT_MS`.
const REQUEST_TIMEOUT_MS: u64 = 8_000;
/// `octokit.js` `ETAG_CACHE_MAX_ENTRIES`.
const ETAG_CACHE_MAX_ENTRIES: usize = 300;

/// GitHub REST API 根地址。
pub const GITHUB_API_BASE: &str = "https://api.github.com";
/// 默认 `accept` 头：与 octokit 一致的 v3 JSON 媒体类型。
const DEFAULT_ACCEPT: &str = "application/vnd.github.v3+json";

/// A single GitHub API request as octokit would issue it.
/// 发往 GitHub 的一次 HTTP 请求（octokit 等价形状）。
#[derive(Debug, Clone)]
pub struct GithubRequest {
/// HTTP 方法（大写字符串）。
    pub method: String,
    /// Absolute URL (base + encoded path + query).
/// 完整 URL = base + 已编码路径 + query。
    pub url: String,
/// 附加请求头（name/value 对）。
    pub headers: Vec<(String, String)>,
/// 请求体（JSON 序列化后的字节）；无 body 时为 `None`。
    pub body: Option<Vec<u8>>,
}

/// 一次 GitHub 响应：状态码、响应头与未解码的字节体。
#[derive(Debug, Clone)]
pub struct GithubResponse {
/// HTTP 状态码。
    pub status: u16,
/// 响应头（name/value 对）。
    pub headers: Vec<(String, String)>,
/// 未解码的响应体字节。
    pub body: Vec<u8>,
}

/// Mirror of octokit's `RequestError`: `status`, `message`,
/// `response.headers`, `response.data`.
/// GitHub 请求错误：镜像 octokit 的 `RequestError`（status/message/headers/data）。
#[derive(Debug, Clone)]
pub struct GithubError {
/// HTTP 状态码；传输层失败等无状态场景为 `None`（本实现恒有值）。
    pub status: Option<u16>,
/// 按 octokit 规则拼出的可读错误消息。
    pub message: String,
/// 出错响应的响应头。
    pub headers: Vec<(String, String)>,
    /// Parsed response body (`response.data`) when it was JSON/text.
/// 已解析的响应体（octokit 的 response.data），JSON/文本时存在。
    pub data: Option<Value>,
}

/// 错误对象上的响应头便捷查询。
impl GithubError {
    /// Case-insensitive response header lookup.
/// 大小写不敏感地取错误响应头。
    pub fn header(&self, name: &str) -> Option<&str> {
        header_value(&self.headers, name)
    }
}

/// 在 `(name, value)` 列表中按名字（忽略大小写）查找首个匹配值。
pub fn header_value<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

/// 响应体的解析与头查询。
impl GithubResponse {
    /// Case-insensitive header lookup.
/// 大小写不敏感地取响应头。
    pub fn header(&self, name: &str) -> Option<&str> {
        header_value(&self.headers, name)
    }

    /// `getResponseData` from octokit's fetch-wrapper: `application/json`
    /// bodies parse as JSON (falling back to the raw string), `text/*` or
    /// `charset=utf-8` bodies decode as text, anything else stays opaque.
/// 按 octokit getResponseData 的规则解析响应体：`application/json` 解析为
/// JSON（失败回退原字符串）；`text/*` 或 `charset=utf-8` 解析为字符串；
/// 其余二进制类型返回 `None`。
    pub fn octokit_data(&self) -> Option<Value> {
        let content_type = self.header("content-type").unwrap_or("");
        let mimetype = content_type
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        let charset_utf8 = content_type
            .split(';')
            .skip(1)
            .any(|p| p.trim().to_ascii_lowercase().starts_with("charset=utf-8"));
        let text = String::from_utf8_lossy(&self.body).to_string();
        if mimetype == "application/json" || mimetype == "application/scim+json" {
            return Some(serde_json::from_str(&text).unwrap_or(Value::String(text)));
        }
        if mimetype.starts_with("text/") || (charset_utf8 && mimetype != "application/octet-stream")
        {
            return Some(Value::String(text));
        }
        None
    }
}

/// `toErrorMessage` from octokit's fetch-wrapper.
/// 复刻 octokit 的错误消息拼接：对象取 `message`，附 `errors` 数组
/// （JSON 化后逗号连接）与 `documentation_url` 后缀；
/// 非对象/缺失 message 时为 "Unknown error: …"。
fn to_error_message(data: Option<&Value>) -> String {
    match data {
        None | Some(Value::Null) => "Unknown error".to_string(),
        Some(Value::String(s)) => s.clone(),
        Some(v @ Value::Object(obj)) => {
            let message = obj.get("message").and_then(Value::as_str);
            match message {
                Some(message) => {
                    let suffix = obj
                        .get("documentation_url")
                        .and_then(Value::as_str)
                        .map(|url| format!(" - {url}"))
                        .unwrap_or_default();
                    match obj.get("errors").and_then(Value::as_array) {
                        Some(errors) => {
                            let joined = errors
                                .iter()
                                .map(|e| serde_json::to_string(e).unwrap_or_default())
                                .collect::<Vec<_>>()
                                .join(", ");
                            format!("{message}: {joined}{suffix}")
                        }
                        None => format!("{message}{suffix}"),
                    }
                }
                None => format!("Unknown error: {v}"),
            }
        }
        Some(other) => format!("Unknown error: {other}"),
    }
}

/// Build a `GithubError` the way octokit does for status >= 400.
/// 由 >= 400 的响应构造 `GithubError`（带状态、解析体与响应头）。
fn error_from_response(response: &GithubResponse) -> GithubError {
    let data = response.octokit_data();
    GithubError {
        status: Some(response.status),
        message: to_error_message(data.as_ref()),
        headers: response.headers.clone(),
        data,
    }
}

/// HTTP transport seam. Production: reqwest with the octokit timeout + ETag
/// conditional cache. Tests: canned responses.
/// 传输层抽象：吃请求、返回 future 的共享函数；测试注入 fake 应答。
pub type Transport = Arc<
    dyn Fn(GithubRequest) -> BoxFuture<'static, Result<GithubResponse, GithubError>> + Send + Sync,
>;

/// ETag 缓存条目：上次 200 的 etag、响应体与响应头。
struct EtagEntry {
/// 上次响应的 ETag（作为 If-None-Match 回传）。
    etag: String,
/// 缓存的响应体字节（304 时回放为 200）。
    body: Vec<u8>,
/// 缓存的响应头（304 回放时一并返回）。
    headers: Vec<(String, String)>,
}

/// Process-global ETag cache (`octokit.js` `etagCache`), keyed
/// `token\nurl`, insertion-ordered with LRU touch, capped at 300 entries.
/// 进程级全局缓存；Vec 保插入序，读写时 touch 实现 LRU，容量 300。
static ETAG_CACHE: LazyLock<Mutex<Vec<(String, EtagEntry)>>> =
    LazyLock::new(|| Mutex::new(Vec::new()));

/// 按 `token\nurl` key 读取缓存条目（返回克隆）。
fn etag_cache_get(key: &str) -> Option<EtagEntry> {
    ETAG_CACHE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, e)| EtagEntry {
            etag: e.etag.clone(),
            body: e.body.clone(),
            headers: e.headers.clone(),
        })
}

/// 写入/刷新缓存条目：先移除同 key 再压入尾部，超容量淘汰队首（最旧）。
fn etag_cache_remember(key: String, entry: EtagEntry) {
    let mut cache = ETAG_CACHE.lock().unwrap_or_else(|e| e.into_inner());
    cache.retain(|(k, _)| k != &key);
    cache.push((key, entry));
    if cache.len() > ETAG_CACHE_MAX_ENTRIES {
        cache.remove(0);
    }
}

/// AbortSignal.timeout semantics: octokit rethrows the abort as a 500 error.
/// 超时中断错误：octokit 把 AbortSignal.timeout 的中止重抛为 500。
fn abort_error() -> GithubError {
    GithubError {
        status: Some(500),
        message: "The operation was aborted".to_string(),
        headers: Vec::new(),
        data: None,
    }
}

/// 传输层错误（URL 解析/连接/读体失败等）统一包装为 500 的 `GithubError`。
fn transport_error(message: String) -> GithubError {
    GithubError {
        status: Some(500),
        message,
        headers: Vec::new(),
        data: None,
    }
}

/// 把 reqwest 响应头收集为 `(name, value)` 列表；非法 UTF-8 值降级为空串。
fn collect_headers(response: &reqwest::Response) -> Vec<(String, String)> {
    response
        .headers()
        .iter()
        .map(|(name, value)| {
            (
                name.as_str().to_string(),
                value.to_str().unwrap_or_default().to_string(),
            )
        })
        .collect()
}

/// Production transport: reqwest + 8s timeout + ETag conditional requests
/// (`octokit.js` `createConditionalFetch(token)`).
/// 生产传输实现：GET 命中缓存则附 If-None-Match；304 回放缓存为 200
/// （无缓存时报 "Not modified"）；4xx/5xx 转为 `GithubError`；
/// 成功的 GET 按条件写回 ETag 缓存。整体受 8 秒超时约束，超时映射为 abort 错误。
pub fn default_transport(token: &str) -> Transport {
    let token = token.to_string();
    Arc::new(move |request: GithubRequest| {
        let token = token.clone();
        Box::pin(async move {
            let is_get = request.method.eq_ignore_ascii_case("GET");
            let cache_key = format!("{token}\n{}", request.url);
            let cached = if is_get {
                etag_cache_get(&cache_key)
            } else {
                None
            };

            let mut headers = request.headers.clone();
            if let Some(cached) = cached.as_ref() {
                headers.push(("if-none-match".to_string(), cached.etag.clone()));
            }

            let method = match reqwest::Method::from_bytes(request.method.as_bytes()) {
                Ok(m) => m,
                Err(e) => return Err(transport_error(e.to_string())),
            };
            let url = match reqwest::Url::parse(&request.url) {
                Ok(u) => u,
                Err(e) => return Err(transport_error(e.to_string())),
            };
            let mut req = token_client().request(method, url);
            for (name, value) in &headers {
                req = req.header(name, value);
            }
            if let Some(body) = request.body.clone() {
                req = req.body(body);
            }

            let send = async {
                match req.send().await {
                    Ok(resp) => {
                        let status = resp.status().as_u16();
                        let headers = collect_headers(&resp);
                        let body = resp
                            .bytes()
                            .await
                            .map_err(|e| transport_error(e.to_string()))?;
                        Ok(GithubResponse {
                            status,
                            headers,
                            body: body.to_vec(),
                        })
                    }
                    Err(e) => Err(transport_error(e.to_string())),
                }
            };
            let response = match tokio::time::timeout(
                std::time::Duration::from_millis(REQUEST_TIMEOUT_MS),
                send,
            )
            .await
            {
                Ok(result) => result?,
                Err(_) => return Err(abort_error()),
            };

            if response.status == 304 {
                if let Some(cached) = cached {
                    let EtagEntry {
                        etag,
                        body,
                        headers,
                    } = cached;
                    etag_cache_remember(
                        cache_key,
                        EtagEntry {
                            etag: etag.clone(),
                            body: body.clone(),
                            headers: headers.clone(),
                        },
                    );
                    return Ok(GithubResponse {
                        status: 200,
                        headers,
                        body,
                    });
                }
                let data = response.octokit_data();
                return Err(GithubError {
                    status: Some(304),
                    message: "Not modified".to_string(),
                    headers: response.headers,
                    data,
                });
            }

            if response.status == 204 || response.status == 205 {
                return Ok(GithubResponse {
                    status: response.status,
                    headers: response.headers,
                    body: Vec::new(),
                });
            }

            if (400..600).contains(&response.status) {
                return Err(error_from_response(&response));
            }

            if is_get
                && response.status < 400
                && let Some(etag) = response.header("etag").map(str::to_string)
            {
                etag_cache_remember(
                    cache_key,
                    EtagEntry {
                        etag,
                        body: response.body.clone(),
                        headers: response.headers.clone(),
                    },
                );
            }

            Ok(response)
        })
    })
}

/// 进程级共享的 reqwest Client（连接池复用）。
fn token_client() -> &'static reqwest::Client {
        // 进程生命周期内共享的连接池实例。
    static CLIENT: LazyLock<reqwest::Client> = LazyLock::new(reqwest::Client::new);
    &CLIENT
}

/// `withAuthorizationPrefix`: JWTs use `bearer`, everything else `token`.
/// JWT（三段式点号分隔）用 `bearer` 前缀，其余 token 用 `token` 前缀。
pub fn authorization_header(token: &str) -> String {
    if token.split('.').count() == 3 {
        format!("bearer {token}")
    } else {
        format!("token {token}")
    }
}

/// URL-template `{var}` expansion (`encodeUnreserved`): encodeURIComponent
/// plus escaping `!'()*`.
/// URL 模板路径段编码：仅保留 `A-Za-z0-9-_.~`，其余字节按 `%XX` 转义。
pub fn encode_path_segment(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// JS `encodeURIComponent`: leaves `A-Za-z0-9-_.!~*'()` untouched.
/// 与 JS encodeURIComponent 一致：放行 `A-Za-z0-9-_.!~*'()`，其余转义 `%XX`。
pub fn encode_query_component(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'-'
            | b'_'
            | b'.'
            | b'!'
            | b'~'
            | b'*'
            | b'\''
            | b'('
            | b')' => out.push(byte as char),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// `addQueryParameters`' `q` special case: split on `+`, encode each part,
/// rejoin with `+` (so `c++` survives and spaces become `%20`).
/// Search 的 `q` 参数专用编码：按 `+` 切段分别编码后以 `+` 重连，
/// 使 `c++` 原样保留而空格转为 `%20`。
pub fn encode_search_query(q: &str) -> String {
    q.split('+')
        .map(encode_query_component)
        .collect::<Vec<_>>()
        .join("+")
}

/// The GitHub API client (`createOctokit(token)` equivalent).
/// GitHub API 客户端：持有传输层与预计算的 authorization 头。
#[derive(Clone)]
pub struct GithubClient {
/// 实际执行 HTTP 的传输层。
    transport: Transport,
    /// `authorization` header value octokit's auth-token plugin would set.
/// octokit auth-token 插件会设置的 authorization 头（含 token/bearer 前缀）。
    authorization: String,
}

/// 各 REST 端点封装与请求构造。
impl GithubClient {
/// 用真实 token 构造走生产 reqwest 传输的客户端。
    pub fn new(token: &str) -> Self {
        Self {
            transport: default_transport(token),
            authorization: authorization_header(token),
        }
    }

/// 注入自定义传输（测试 fake）；不带鉴权头。
    pub fn with_transport(transport: Transport) -> Self {
        // Fake transports do not authenticate; mirror the empty-token shape.
        Self {
            transport,
            authorization: String::new(),
        }
    }

/// 组装 `GithubRequest`：拼接 base+path 与编码后的 query，附
/// accept/user-agent/authorization 头；有 body 时序列化为 JSON 并补 content-type。
    fn build_request(
        &self,
        method: &str,
        path: &str,
        query: Vec<(String, String)>,
        body: Option<Value>,
        accept: &str,
    ) -> GithubRequest {
        let mut url = format!("{GITHUB_API_BASE}{path}");
        if !query.is_empty() {
            let qs = query
                .iter()
                .map(|(k, v)| format!("{k}={}", encode_query_component(v)))
                .collect::<Vec<_>>()
                .join("&");
            url.push('?');
            url.push_str(&qs);
        }
        let mut request = GithubRequest {
            method: method.to_string(),
            url,
            headers: vec![
                ("accept".to_string(), accept.to_string()),
                (
                    "user-agent".to_string(),
                    "octokit-rest.js/rust-port".to_string(),
                ),
                ("authorization".to_string(), self.authorization.clone()),
            ],
            body: None,
        };
        if let Some(body) = body {
            request.body = Some(serde_json::to_vec(&body).unwrap_or_default());
            request.headers.push((
                "content-type".to_string(),
                "application/json; charset=utf-8".to_string(),
            ));
        }
        request
    }

    /// GET with default accept, returning parsed data.
/// GET 并把响应体解析为 JSON（无法解析时为 `Null`）。
    async fn get_json(
        &self,
        path: &str,
        query: Vec<(String, String)>,
    ) -> Result<Value, GithubError> {
        let req = self.build_request("GET", path, query, None, DEFAULT_ACCEPT);
        let resp = (self.transport)(req).await?;
        Ok(resp.octokit_data().unwrap_or(Value::Null))
    }

    /// GET returning parsed data plus response headers (`link` drives
    /// has-more checks in routes).
/// GET 并返回解析体与响应头（`link` 头供路由判断分页 has-more）。
    async fn get_json_with_headers(
        &self,
        path: &str,
        query: Vec<(String, String)>,
    ) -> Result<(Value, Vec<(String, String)>), GithubError> {
        let req = self.build_request("GET", path, query, None, DEFAULT_ACCEPT);
        let resp = (self.transport)(req).await?;
        Ok((resp.octokit_data().unwrap_or(Value::Null), resp.headers))
    }

/// POST/PATCH/PUT 等带 JSON body 的请求，返回解析后的响应体。
    async fn send_body(
        &self,
        method: &str,
        path: &str,
        body: Option<Value>,
    ) -> Result<Value, GithubError> {
        let req = self.build_request(method, path, Vec::new(), body, DEFAULT_ACCEPT);
        let resp = (self.transport)(req).await?;
        Ok(resp.octokit_data().unwrap_or(Value::Null))
    }

    // ---- users ----

/// `GET /user`：当前鉴权用户。
    pub async fn users_get_authenticated(&self) -> Result<Value, GithubError> {
        self.get_json("/user", Vec::new()).await
    }

/// `GET /user/emails`：当前用户邮箱列表（每页 100）。
    pub async fn users_list_emails(&self) -> Result<Value, GithubError> {
        self.get_json(
            "/user/emails",
            vec![("per_page".to_string(), "100".to_string())],
        )
        .await
    }

    // ---- repos ----

/// `GET /repos/{owner}/{repo}`：仓库元数据。
    pub async fn repos_get(&self, owner: &str, repo: &str) -> Result<Value, GithubError> {
        let path = format!(
            "/repos/{}/{}",
            encode_path_segment(owner),
            encode_path_segment(repo)
        );
        self.get_json(&path, Vec::new()).await
    }

/// `GET /repos/{o}/{r}/branches/{branch}`：分支信息。
    pub async fn repos_get_branch(
        &self,
        owner: &str,
        repo: &str,
        branch: &str,
    ) -> Result<Value, GithubError> {
        let path = format!(
            "/repos/{}/{}/branches/{}",
            encode_path_segment(owner),
            encode_path_segment(repo),
            encode_path_segment(branch)
        );
        self.get_json(&path, Vec::new()).await
    }

/// `GET /repos/{o}/{r}/branches`：分页列出分支（每页 100）。
    pub async fn repos_list_branches(
        &self,
        owner: &str,
        repo: &str,
        page: u32,
    ) -> Result<Value, GithubError> {
        let path = format!(
            "/repos/{}/{}/branches",
            encode_path_segment(owner),
            encode_path_segment(repo)
        );
        self.get_json(
            &path,
            vec![
                ("per_page".to_string(), "100".to_string()),
                ("page".to_string(), page.to_string()),
            ],
        )
        .await
    }

/// `GET /repos/{o}/{r}/commits/{ref}/status`：commit 的聚合 CI 状态。
    pub async fn repos_combined_status(
        &self,
        owner: &str,
        repo: &str,
        git_ref: &str,
    ) -> Result<Value, GithubError> {
        let path = format!(
            "/repos/{}/{}/commits/{}/status",
            encode_path_segment(owner),
            encode_path_segment(repo),
            encode_path_segment(git_ref)
        );
        self.get_json(&path, Vec::new()).await
    }

/// `GET /repos/{o}/{r}/collaborators/{user}/permission`：协作者权限级别。
    pub async fn repos_collaborator_permission(
        &self,
        owner: &str,
        repo: &str,
        username: &str,
    ) -> Result<Value, GithubError> {
        let path = format!(
            "/repos/{}/{}/collaborators/{}/permission",
            encode_path_segment(owner),
            encode_path_segment(repo),
            encode_path_segment(username)
        );
        self.get_json(&path, Vec::new()).await
    }

/// `GET /repos/{o}/{r}/git/ref/{ref}`：解析 ref 到具体对象。
    pub async fn git_get_ref(
        &self,
        owner: &str,
        repo: &str,
        git_ref: &str,
    ) -> Result<Value, GithubError> {
        let path = format!(
            "/repos/{}/{}/git/ref/{}",
            encode_path_segment(owner),
            encode_path_segment(repo),
            encode_path_segment(git_ref)
        );
        self.get_json(&path, Vec::new()).await
    }

    // ---- pulls ----

    /// `pulls.list`. Returns parsed data plus headers.
/// `GET /repos/{o}/{r}/pulls`：可按 state/head 过滤；返回数据与响应头。
    pub async fn pulls_list(
        &self,
        owner: &str,
        repo: &str,
        state: &str,
        head: Option<&str>,
        per_page: u32,
        page: Option<u32>,
    ) -> Result<(Value, Vec<(String, String)>), GithubError> {
        let path = format!(
            "/repos/{}/{}/pulls",
            encode_path_segment(owner),
            encode_path_segment(repo)
        );
        let mut query = vec![
            ("state".to_string(), state.to_string()),
            ("per_page".to_string(), per_page.to_string()),
        ];
        if let Some(head) = head {
            query.push(("head".to_string(), head.to_string()));
        }
        if let Some(page) = page {
            query.push(("page".to_string(), page.to_string()));
        }
        self.get_json_with_headers(&path, query).await
    }

/// `GET /repos/{o}/{r}/pulls/{n}`：单个 PR 详情。
    pub async fn pulls_get(
        &self,
        owner: &str,
        repo: &str,
        number: &str,
    ) -> Result<Value, GithubError> {
        let path = format!(
            "/repos/{}/{}/pulls/{}",
            encode_path_segment(owner),
            encode_path_segment(repo),
            encode_path_segment(number)
        );
        self.get_json(&path, Vec::new()).await
    }

/// `POST /repos/{o}/{r}/pulls`：创建 PR。
    pub async fn pulls_create(
        &self,
        owner: &str,
        repo: &str,
        payload: &Value,
    ) -> Result<Value, GithubError> {
        let path = format!(
            "/repos/{}/{}/pulls",
            encode_path_segment(owner),
            encode_path_segment(repo)
        );
        self.send_body("POST", &path, Some(payload.clone())).await
    }

/// `PATCH /repos/{o}/{r}/pulls/{n}`：更新 PR（标题/base 等字段）。
    pub async fn pulls_update(
        &self,
        owner: &str,
        repo: &str,
        number: &str,
        payload: &Value,
    ) -> Result<Value, GithubError> {
        let path = format!(
            "/repos/{}/{}/pulls/{}",
            encode_path_segment(owner),
            encode_path_segment(repo),
            encode_path_segment(number)
        );
        self.send_body("PATCH", &path, Some(payload.clone())).await
    }

/// `PUT /repos/{o}/{r}/pulls/{n}/merge`：按指定 merge_method 合并 PR。
    pub async fn pulls_merge(
        &self,
        owner: &str,
        repo: &str,
        number: &str,
        merge_method: &str,
    ) -> Result<Value, GithubError> {
        let path = format!(
            "/repos/{}/{}/pulls/{}/merge",
            encode_path_segment(owner),
            encode_path_segment(repo),
            encode_path_segment(number)
        );
        self.send_body(
            "PUT",
            &path,
            Some(serde_json::json!({ "merge_method": merge_method })),
        )
        .await
    }

/// `GET /repos/{o}/{r}/pulls/{n}/files`：PR 变更文件（每页 100）。
    pub async fn pulls_list_files(
        &self,
        owner: &str,
        repo: &str,
        number: &str,
    ) -> Result<Value, GithubError> {
        let path = format!(
            "/repos/{}/{}/pulls/{}/files",
            encode_path_segment(owner),
            encode_path_segment(repo),
            encode_path_segment(number)
        );
        self.get_json(&path, vec![("per_page".to_string(), "100".to_string())])
            .await
    }

/// `GET /repos/{o}/{r}/pulls/{n}/comments`：PR review 行内评论（每页 100）。
    pub async fn pulls_list_review_comments(
        &self,
        owner: &str,
        repo: &str,
        number: &str,
    ) -> Result<Value, GithubError> {
        let path = format!(
            "/repos/{}/{}/pulls/{}/comments",
            encode_path_segment(owner),
            encode_path_segment(repo),
            encode_path_segment(number)
        );
        self.get_json(&path, vec![("per_page".to_string(), "100".to_string())])
            .await
    }

    /// `GET /repos/{owner}/{repo}/pulls/{n}` with
    /// `accept: application/vnd.github.v3.diff` — returns the diff text.
/// 以 diff 媒体类型请求 PR 详情，返回 diff 文本；>= 400 转为错误。
    pub async fn pulls_get_diff(
        &self,
        owner: &str,
        repo: &str,
        number: &str,
    ) -> Result<Option<String>, GithubError> {
        let path = format!(
            "/repos/{}/{}/pulls/{}",
            encode_path_segment(owner),
            encode_path_segment(repo),
            encode_path_segment(number)
        );
        let req = self.build_request(
            "GET",
            &path,
            Vec::new(),
            None,
            "application/vnd.github.v3.diff",
        );
        let resp = (self.transport)(req).await?;
        if resp.status >= 400 {
            return Err(error_from_response(&resp));
        }
        Ok(Some(String::from_utf8_lossy(&resp.body).to_string()))
    }

    // ---- checks / actions ----

/// `GET /repos/{o}/{r}/commits/{ref}/check-runs`：commit 的 check runs。
    pub async fn checks_list_for_ref(
        &self,
        owner: &str,
        repo: &str,
        git_ref: &str,
    ) -> Result<Value, GithubError> {
        let path = format!(
            "/repos/{}/{}/commits/{}/check-runs",
            encode_path_segment(owner),
            encode_path_segment(repo),
            encode_path_segment(git_ref)
        );
        self.get_json(&path, vec![("per_page".to_string(), "100".to_string())])
            .await
    }

/// `GET /repos/{o}/{r}/check-runs/{id}/annotations`：单条 check 的注解，分页获取。
    pub async fn checks_list_annotations(
        &self,
        owner: &str,
        repo: &str,
        check_run_id: i64,
        page: u32,
    ) -> Result<Value, GithubError> {
        let path = format!(
            "/repos/{}/{}/check-runs/{check_run_id}/annotations",
            encode_path_segment(owner),
            encode_path_segment(repo)
        );
        self.get_json(
            &path,
            vec![
                ("per_page".to_string(), "50".to_string()),
                ("page".to_string(), page.to_string()),
            ],
        )
        .await
    }

/// `GET /repos/{o}/{r}/actions/runs/{id}/jobs`：workflow run 的 job 列表。
    pub async fn actions_list_jobs(
        &self,
        owner: &str,
        repo: &str,
        run_id: i64,
    ) -> Result<Value, GithubError> {
        let path = format!(
            "/repos/{}/{}/actions/runs/{run_id}/jobs",
            encode_path_segment(owner),
            encode_path_segment(repo)
        );
        self.get_json(&path, vec![("per_page".to_string(), "100".to_string())])
            .await
    }

    // ---- issues ----

/// `GET /repos/{o}/{r}/issues`：按 state 分页列出 issue；返回数据与响应头。
    pub async fn issues_list_for_repo(
        &self,
        owner: &str,
        repo: &str,
        state: &str,
        per_page: u32,
        page: u32,
    ) -> Result<(Value, Vec<(String, String)>), GithubError> {
        let path = format!(
            "/repos/{}/{}/issues",
            encode_path_segment(owner),
            encode_path_segment(repo)
        );
        self.get_json_with_headers(
            &path,
            vec![
                ("state".to_string(), state.to_string()),
                ("per_page".to_string(), per_page.to_string()),
                ("page".to_string(), page.to_string()),
            ],
        )
        .await
    }

/// `GET /repos/{o}/{r}/issues/{n}`：单个 issue（PR 也复用该端点）。
    pub async fn issues_get(
        &self,
        owner: &str,
        repo: &str,
        number: &str,
    ) -> Result<Value, GithubError> {
        let path = format!(
            "/repos/{}/{}/issues/{}",
            encode_path_segment(owner),
            encode_path_segment(repo),
            encode_path_segment(number)
        );
        self.get_json(&path, Vec::new()).await
    }

/// `GET /repos/{o}/{r}/issues/{n}/comments`：issue/PR 顶层评论（每页 100）。
    pub async fn issues_list_comments(
        &self,
        owner: &str,
        repo: &str,
        number: &str,
    ) -> Result<Value, GithubError> {
        let path = format!(
            "/repos/{}/{}/issues/{}/comments",
            encode_path_segment(owner),
            encode_path_segment(repo),
            encode_path_segment(number)
        );
        self.get_json(&path, vec![("per_page".to_string(), "100".to_string())])
            .await
    }

    // ---- search ----

/// `GET /search/issues`：issue/PR 搜索；`q` 使用加号切分的专用编码。
    pub async fn search_issues(
        &self,
        q: &str,
        per_page: u32,
        page: u32,
    ) -> Result<Value, GithubError> {
        // `q` gets the split-on-`+` encoding; other params are plain encoded.
        let url = format!(
            "{GITHUB_API_BASE}/search/issues?q={}&per_page={per_page}&page={page}",
            encode_search_query(q)
        );
        let req = GithubRequest {
            method: "GET".to_string(),
            url,
            headers: vec![
                ("accept".to_string(), DEFAULT_ACCEPT.to_string()),
                (
                    "user-agent".to_string(),
                    "octokit-rest.js/rust-port".to_string(),
                ),
                ("authorization".to_string(), self.authorization.clone()),
            ],
            body: None,
        };
        let resp = (self.transport)(req).await?;
        Ok(resp.octokit_data().unwrap_or(Value::Null))
    }

    // ---- graphql ----

/// `POST /graphql`：执行 GraphQL 查询/变更；响应 body 中的 `errors` 数组
/// 转为携带 HTTP 状态的 `GithubError`（对齐 octokit graphql 的抛错行为）。
    pub async fn graphql(&self, query: &str, variables: Value) -> Result<Value, GithubError> {
        let payload = serde_json::json!({ "query": query, "variables": variables });
        let req = self.build_request(
            "POST",
            "/graphql",
            Vec::new(),
            Some(payload),
            DEFAULT_ACCEPT,
        );
        let resp = (self.transport)(req).await?;
        if resp.status >= 400 {
            return Err(error_from_response(&resp));
        }
        let data = resp.octokit_data().unwrap_or(Value::Null);
        // octokit graphql throws on body-level errors; carry the HTTP status so
        // the 403 branch in pr/ready behaves like octokit's RequestError.
        if let Some(errors) = data.get("errors").and_then(Value::as_array) {
            let message = errors
                .first()
                .and_then(|e| e.get("message"))
                .and_then(Value::as_str)
                .unwrap_or("Graphql error")
                .to_string();
            return Err(GithubError {
                status: Some(resp.status),
                message,
                headers: resp.headers,
                data: Some(data),
            });
        }
        Ok(data)
    }
}

/// 客户端单元测试：错误消息形状、编码规则与 URL 构造均对齐 octokit。
#[cfg(test)]
mod tests {
    use super::*;

/// 构造 JSON content-type 的响应。
    fn json_response(status: u16, body: Value) -> GithubResponse {
        GithubResponse {
            status,
            headers: vec![(
                "content-type".to_string(),
                "application/json; charset=utf-8".to_string(),
            )],
            body: serde_json::to_vec(&body).unwrap(),
        }
    }

/// 用同步应答函数构造 fake 传输的客户端。
    fn fake(
        responder: impl Fn(&GithubRequest) -> Result<GithubResponse, GithubError>
        + Send
        + Sync
        + 'static,
    ) -> GithubClient {
        GithubClient::with_transport(Arc::new(move |req: GithubRequest| {
            let result = responder(&req);
            Box::pin(async move { result })
        }))
    }

/// 验证：422 校验失败体的错误消息拼出 message 与 errors 字段子串。
    #[tokio::test]
    async fn error_message_mirrors_octokit_validation_failed_shape() {
        let data = serde_json::json!({
            "message": "Validation Failed",
            "errors": [{ "resource": "PullRequest", "code": "invalid", "field": "head" }]
        });
        let response = json_response(422, data);
        let error = error_from_response(&response);
        assert_eq!(error.status, Some(422));
        // routes.js checks these exact substrings on error.message.
        assert!(error.message.contains("Validation Failed"));
        assert!(error.message.contains("\"field\":\"head\""));
        assert!(error.message.contains("\"code\":\"invalid\""));
    }

/// 验证：documentation_url 以 " - <url>" 后缀拼进错误消息。
    #[tokio::test]
    async fn error_message_uses_documentation_url_suffix() {
        let data = serde_json::json!({
            "message": "Not Found",
            "documentation_url": "https://docs.github.com"
        });
        let response = json_response(404, data);
        let error = error_from_response(&response);
        assert_eq!(error.message, "Not Found - https://docs.github.com");
    }

/// 验证：普通 token 与 JWT 分别得到 `token`/`bearer` 前缀。
    #[test]
    fn authorization_prefix_matches_octokit() {
        assert_eq!(authorization_header("abc"), "token abc");
        assert_eq!(authorization_header("a.b.c"), "bearer a.b.c");
    }

/// 验证：路径段、query 组件与 search q 的编码与 octokit 逐字节一致。
    #[test]
    fn path_and_query_encoding_match_octokit() {
        assert_eq!(encode_path_segment("feat/x"), "feat%2Fx");
        assert_eq!(encode_path_segment("a b"), "a%20b");
        assert_eq!(encode_query_component("a b"), "a%20b");
        // encodeURIComponent escapes literal "+"; only the q-splitting preserves it
        assert_eq!(encode_query_component("c++"), "c%2B%2B");
        // q: spaces become %20, literal + survives
        assert_eq!(
            encode_search_query("repo:o/r fix type:issue"),
            "repo%3Ao%2Fr%20fix%20type%3Aissue"
        );
        assert_eq!(encode_search_query("c++ lang"), "c++%20lang");
    }

/// 验证：pulls.list 生成的 URL 与请求头形状和 octokit 相同。
    #[tokio::test]
    async fn pulls_list_builds_octokit_url_shape() {
        let client = fake(|req| {
            assert_eq!(req.method, "GET");
            assert_eq!(
                req.url,
                "https://api.github.com/repos/acme/app/pulls?state=open&per_page=100&head=acme%3Afeature"
            );
            assert_eq!(
                req.headers
                    .iter()
                    .find(|(k, _)| k == "accept")
                    .map(|(_, v)| v.as_str()),
                Some("application/vnd.github.v3+json")
            );
            Ok(json_response(200, serde_json::json!([])))
        });
        let (data, _) = client
            .pulls_list("acme", "app", "open", Some("acme:feature"), 100, None)
            .await
            .unwrap();
        assert_eq!(data, serde_json::json!([]));
    }

/// 验证：search 的 q 参数按 `+` 切分编码且空格转 `%20`。
    #[tokio::test]
    async fn search_issues_encodes_q_with_plus_splitting() {
        let client = fake(|req| {
            assert_eq!(
                req.url,
                "https://api.github.com/search/issues?q=repo%3Aacme%2Fapp%20fix%20type%3Aissue&per_page=50&page=2"
            );
            Ok(json_response(
                200,
                serde_json::json!({ "total_count": 0, "items": [] }),
            ))
        });
        let data = client
            .search_issues("repo:acme/app fix type:issue", 50, 2)
            .await
            .unwrap();
        assert_eq!(data["total_count"], 0);
    }

/// 验证：graphql POST 的 body 携带 query 与 variables。
    #[tokio::test]
    async fn graphql_posts_query_and_variables() {
        let client = fake(|req| {
            assert_eq!(req.method, "POST");
            assert_eq!(req.url, "https://api.github.com/graphql");
            let body: Value = serde_json::from_slice(req.body.as_deref().unwrap()).unwrap();
            assert!(
                body["query"]
                    .as_str()
                    .unwrap()
                    .contains("markPullRequestReadyForReview")
            );
            assert_eq!(body["variables"]["pullRequestId"], "node-1");
            Ok(json_response(200, serde_json::json!({ "data": {} })))
        });
        client
            .graphql(
                "mutation { markPullRequestReadyForReview }",
                serde_json::json!({ "pullRequestId": "node-1" }),
            )
            .await
            .unwrap();
    }

/// 验证：diff 等 text 媒体类型的成功响应按文本返回。
    #[tokio::test]
    async fn non_json_success_body_parses_as_text() {
        let client = fake(|_| {
            Ok(GithubResponse {
                status: 200,
                headers: vec![(
                    "content-type".to_string(),
                    "application/vnd.github.v3.diff; charset=utf-8".to_string(),
                )],
                body: b"diff --git a/x".to_vec(),
            })
        });
        let diff = client.pulls_get_diff("o", "r", "1").await.unwrap();
        assert_eq!(diff.as_deref(), Some("diff --git a/x"));
    }
}
