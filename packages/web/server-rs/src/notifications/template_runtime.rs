//! Port of `server/lib/notifications/template-runtime.js`: notification
//! template variable resolution, message-text extraction from OpenCode
//! payloads, session info caching, and the retired-Zen compatibility
//! stubs. Model-backed summarization is delegated to the ported
//! `small_model` summarizer (its no-model path is byte-for-byte the JS
//! post-retirement fallback).
//!
//! 中文说明：本模块是 `server/lib/notifications/template-runtime.js`
//! 的 Rust 移植，负责通知模板变量（`{project_name}` / `{session_name}`
//! 等）的解析、从 OpenCode payload 提取消息正文、会话信息缓存，以及
//! Zen 供应商退役后的兼容桩；模型摘要委托给移植版 `small_model` 模块。

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Map, Value, json};

use crate::engine::EngineState;
use crate::notifications::crypto::now_ms;
use crate::settings::SettingsStore;

/// 通知正文的最大字符数：提取消息文本时按 Unicode 字符（非字节）截断。
pub const NOTIFICATION_BODY_MAX_CHARS: usize = 1000;
/// `/session/{id}` 信息的内存缓存时长（毫秒）：60 秒内重复构建模板
/// 变量不会再次请求 engine。
pub const SESSION_INFO_CACHE_TTL_MS: u64 = 60 * 1000;

/// Engine JSON GET seam: `(url, timeout_ms, with_auth) -> Option<json>`.
/// `None` covers every JS failure path (non-ok status, bad JSON, throw).
///
/// 中文说明：engine JSON GET 的 future 类型——None 覆盖 JS 端所有
/// 失败路径（非 2xx、JSON 解析失败、请求抛错）。
pub type EngineJsonFuture = Pin<Box<dyn Future<Output = Option<Value>> + Send>>;
/// Engine JSON GET 的实现类型：`(url, timeout_ms, with_auth)` 闭包，
/// 生产环境由 [`engine_json_fetch`] 构造，测试注入桩实现。
pub type EngineJsonFetch = Arc<dyn Fn(String, u64, bool) -> EngineJsonFuture + Send + Sync>;

/// Production fetch against the managed/external engine
/// (`buildOpenCodeUrl` + `getOpenCodeAuthHeaders`).
///
/// 中文说明：把相对 url 拼接到 engine base_url 后发起带超时的 GET，
/// 可选附带 bearer token；非 2xx、非 JSON 对象、网络错误一律映射为 None。
pub fn engine_json_fetch(engine: Arc<EngineState>) -> EngineJsonFetch {
    Arc::new(move |url: String, timeout_ms: u64, with_auth: bool| {
        let engine = Arc::clone(&engine);
        Box::pin(async move {
            let base = engine.base_url()?;
            let full = format!("{}{}", base.trim_end_matches('/'), url);
            let mut request = engine
                .http()
                .get(&full)
                .timeout(Duration::from_millis(timeout_ms))
                .header("accept", "application/json");
            if with_auth {
                if let Some(auth) = engine.auth_header() {
                    request = request.header("authorization", auth);
                }
            }
            let response = request.send().await.ok()?;
            if !response.status().is_success() {
                return None;
            }
            let text = response.text().await.ok()?;
            let parsed: Value = serde_json::from_str(&text).ok()?;
            if parsed.is_object() {
                Some(parsed)
            } else {
                None
            }
        })
    })
}

/// JS `encodeURIComponent`: unreserved characters stay literal.
///
/// 中文说明：非保留字符（字母数字与 `-_.!~*'()`）原样保留，其余字节
/// 按大写十六进制 `%XX` 转义，与 JS encodeURIComponent 语义对齐。
pub fn encode_uri_component(value: &str) -> String {
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

/// Split on `[-_\s]+` (formatMode / agent names).
///
/// 中文说明：按 `-`、`_`、空白切分并逐词首字母大写，用于
/// formatMode / agent 名称的展示化（如 `code-review` → "Code Review"）。
fn split_mode_tokens(value: &str) -> Vec<String> {
    value
        .split(|c: char| c == '-' || c == '_' || c.is_whitespace())
        .filter(|token| !token.is_empty())
        .map(|token| title_case_first(token))
        .collect()
}

/// 首字母大写、其余字符保持原样（不做小写化，与 JS 语义一致）。
fn title_case_first(token: &str) -> String {
    let mut characters = token.chars();
    match characters.next() {
        Some(first) => first.to_uppercase().collect::<String>() + characters.as_str(),
        None => String::new(),
    }
}

/// `formatMode` (runtime.js): default "agent", tokens split on `[-_\s]+`.
///
/// 中文说明：空值/空白回落 "agent"，再经分词美化输出
/// （如 `plan-mode` → "Plan Mode"）。
pub fn format_mode(raw: Option<&str>) -> String {
    let value = raw.unwrap_or("").trim();
    let normalized = if value.is_empty() { "agent" } else { value };
    split_mode_tokens(normalized).join(" ")
}

/// `formatModelId` (runtime.js): default "Assistant"; split on `[-_]+`
/// with adjacent digit tokens merged (`claude-3-7` → "Claude 3.7").
///
/// 中文说明：空值回落 "Assistant"；按 `-`/`_` 分词、相邻纯数字词以
/// `.` 相连模拟版本号，再逐词首字母大写。
pub fn format_model_id(raw: Option<&str>) -> String {
    let value = raw.unwrap_or("").trim();
    if value.is_empty() {
        return "Assistant".to_string();
    }
    let tokens: Vec<&str> = value
        .split(|c: char| c == '-' || c == '_')
        .filter(|token| !token.is_empty())
        .collect();
    let mut merged: Vec<String> = Vec::with_capacity(tokens.len());
    let mut index = 0;
    while index < tokens.len() {
        let current = tokens[index];
        if let Some(next) = tokens.get(index + 1) {
            if !current.is_empty()
                && current.chars().all(|c| c.is_ascii_digit())
                && !next.is_empty()
                && next.chars().all(|c| c.is_ascii_digit())
            {
                merged.push(format!("{current}.{next}"));
                index += 2;
                continue;
            }
        }
        merged.push(current.to_string());
        index += 1;
    }
    merged
        .iter()
        .map(|part| title_case_first(part))
        .collect::<Vec<_>>()
        .join(" ")
}

/// `/session/{id}` 缓存条目：engine 响应体 + 写入时刻，用于 TTL 判断。
struct SessionInfoCacheEntry {
    /// engine 返回的会话信息 JSON 对象。
    data: Value,
    /// 缓存写入时刻（毫秒时间戳）。
    at: u64,
}

/// 通知模板运行时：持有 settings 存储、engine JSON 访问缝、git 可执行
/// 文件名，以及会话标题/会话信息两个内存缓存。
pub struct TemplateRuntime {
    /// settings 存储：读取 zenModel、projects、activeProjectId 等。
    store: Arc<SettingsStore>,
    /// engine JSON GET 注入缝（见 [`EngineJsonFetch`]）。
    fetch: EngineJsonFetch,
    /// 查询分支用的 git 可执行文件名（通常取 PATH 中的 "git"）。
    git_binary: String,
    /// Session titles never expire in the JS module-level cache.
    /// 中文说明：会话标题缓存永不失效（与 JS 的模块级缓存一致）。
    session_title_cache: Mutex<HashMap<String, String>>,
    /// 会话信息缓存：条目按 [`SESSION_INFO_CACHE_TTL_MS`] 过期。
    session_info_cache: Mutex<HashMap<String, SessionInfoCacheEntry>>,
}

/// 模板解析、消息文本提取、会话信息缓存与 Zen 兼容桩的实现集合。
impl TemplateRuntime {
    /// 构造模板运行时（返回 `Arc` 供触发器/发射器运行时共享），
    /// 两个缓存初始为空。
    pub fn new(store: Arc<SettingsStore>, fetch: EngineJsonFetch, git_binary: String) -> Arc<Self> {
        Arc::new(Self {
            store,
            fetch,
            git_binary,
            session_title_cache: Mutex::new(HashMap::new()),
            session_info_cache: Mutex::new(HashMap::new()),
        })
    }

    /// `resolveNotificationTemplate`: `{word}` substitution; missing or
    /// null variables contribute the empty string.
    ///
    /// 中文说明：只替换 `{word}` 形态（字母数字+下划线）的 token；缺失或
    /// null 变量替换为空串，其余花括号内容原样保留。
    pub fn resolve_notification_template(
        &self,
        template: &str,
        variables: &Map<String, Value>,
    ) -> String {
        let mut result = String::with_capacity(template.len());
        let mut rest = template;
        while let Some(open) = rest.find('{') {
            result.push_str(&rest[..open]);
            let after = &rest[open + 1..];
            match after.find('}') {
                Some(close) => {
                    let key = &after[..close];
                    if key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') && !key.is_empty()
                    {
                        match variables.get(key) {
                            Some(Value::Null) | None => {}
                            Some(value) => {
                                result.push_str(&value_to_display_string(value));
                            }
                        }
                    } else {
                        // Not a `{word}` token — keep it verbatim.
                        result.push('{');
                        result.push_str(key);
                        result.push('}');
                    }
                    rest = &after[close + 1..];
                }
                None => {
                    result.push('{');
                    rest = after;
                }
            }
        }
        result.push_str(rest);
        result
    }

    /// `shouldApplyResolvedTemplateMessage`.
    ///
    /// 中文说明：解析结果为空串时不应用；模板引用 `{last_message}` 时，
    /// 只有变量里存在非空白正文才应用，避免把空消息当成通知发出。
    pub fn should_apply_resolved_template_message(
        &self,
        template: &str,
        resolved: &str,
        variables: &Map<String, Value>,
    ) -> bool {
        if resolved.is_empty() {
            return false;
        }
        if template.contains("{last_message}") {
            return variables
                .get("last_message")
                .and_then(Value::as_str)
                .is_some_and(|message| !message.trim().is_empty());
        }
        true
    }

    // -----------------------------------------------------------------------
    // Zen compatibility stubs (provider retired)
    // -----------------------------------------------------------------------

    /// `fetchFreeZenModels`: no selectable models after provider retirement.
    ///
    /// 中文说明：Zen 供应商已退役，永远返回空列表。
    pub async fn fetch_free_zen_models(&self) -> Vec<Value> {
        Vec::new()
    }

    /// `resolveZenModel`: preserves a stored value without validation.
    ///
    /// 中文说明：显式 override 优先（去空白），否则原样返回 settings 里的
    /// `zenModel`，不做任何校验——退役后仅保留读写兼容。
    pub async fn resolve_zen_model(&self, r#override: Option<&str>) -> String {
        let r#override = r#override.unwrap_or("").trim();
        if !r#override.is_empty() {
            return r#override.to_string();
        }
        let settings = self.store.read_raw().await;
        settings
            .get("zenModel")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(String::from)
            .unwrap_or_default()
    }

    /// `validateZenModelAtStartup`: compatibility no-op.
    ///
    /// 中文说明：退役后无需校验，保留空实现以兼容旧调用点。
    pub async fn validate_zen_model_at_startup(&self) {}

    /// `summarizeText`: delegates to the ported summarizer; the no-model
    /// path always answers with the local fallback (JS post-retirement).
    ///
    /// 中文说明：空白文本原样返回；否则交给 small_model 摘要器，
    /// 摘要为空时回落原文（与 JS 退役后的本地回退一致）。
    pub async fn summarize_text(&self, text: &str, target_length: u64) -> String {
        if text.trim().is_empty() {
            return text.to_string();
        }
        let result = crate::small_model::summarization::summarize_text(
            crate::small_model::summarization::SummarizeParams {
                text: Some(text),
                threshold: 0,
                max_length: Some(target_length as f64),
                mode: crate::small_model::summarization::SummaryMode::Notification,
            },
            None,
        )
        .await;
        result
            .get("summary")
            .and_then(Value::as_str)
            .filter(|summary| !summary.trim().is_empty())
            .map(String::from)
            .unwrap_or_else(|| text.to_string())
    }

    // -----------------------------------------------------------------------
    // Message text extraction
    // -----------------------------------------------------------------------

    /// `isNotificationTextPart`.
    ///
    /// 中文说明：只有 `type == "text"` 且带字符串 `text` 或 `content` 的
    /// part 才计入通知正文（reasoning 等其它类型一律排除）。
    fn is_notification_text_part(part: &Value) -> bool {
        if !part.is_object() {
            return false;
        }
        if part.get("type").and_then(Value::as_str) != Some("text") {
            return false;
        }
        part.get("text").and_then(Value::as_str).is_some()
            || part.get("content").and_then(Value::as_str).is_some()
    }

    /// 按 Unicode 字符（而非字节）截取前 `max` 个字符。
    fn chars_take(value: &str, max: usize) -> String {
        value.chars().take(max).collect()
    }

    /// `extractTextFromParts`.
    ///
    /// 中文说明：过滤出文本 part，取 `text`/`content` 字段，换行拼接后
    /// trim；`max_length` 非 0 时按字符数截断。
    pub fn extract_text_from_parts(&self, parts: &Value, max_length: usize) -> String {
        let Some(entries) = parts.as_array() else {
            return String::new();
        };
        if entries.is_empty() {
            return String::new();
        }
        let text_parts: Vec<&str> = entries
            .iter()
            .filter(|part| Self::is_notification_text_part(part))
            .filter_map(|part| {
                part.get("text")
                    .and_then(Value::as_str)
                    .or_else(|| part.get("content").and_then(Value::as_str))
            })
            .filter(|text| !text.is_empty())
            .collect();
        let mut text = if text_parts.is_empty() {
            String::new()
        } else {
            text_parts.join("\n").trim().to_string()
        };
        if max_length > 0 && text.chars().count() > max_length {
            text = Self::chars_take(&text, max_length);
        }
        text
    }

    /// `extractLastMessageText`.
    ///
    /// 中文说明：优先取 `properties.info.parts`，其次 `properties.parts`；
    /// 都为空时回落 legacy 的 `info.content` 数组，最终限制在
    /// NOTIFICATION_BODY_MAX_CHARS 字符内。
    pub fn extract_last_message_text(&self, payload: &Value) -> String {
        let Some(info) = payload
            .get("properties")
            .and_then(|props| props.get("info"))
        else {
            return String::new();
        };
        let parts = info
            .get("parts")
            .or_else(|| {
                payload
                    .get("properties")
                    .and_then(|props| props.get("parts"))
            })
            .cloned()
            .unwrap_or(Value::Null);
        let text = self.extract_text_from_parts(&parts, NOTIFICATION_BODY_MAX_CHARS);
        if !text.is_empty() {
            return text;
        }
        // Legacy content arrays.
        if let Some(content) = info.get("content").and_then(Value::as_array) {
            let text_parts: Vec<&str> = content
                .iter()
                .filter(|part| Self::is_notification_text_part(part))
                .filter_map(|part| part.get("text").and_then(Value::as_str))
                .filter(|text| !text.is_empty())
                .collect();
            if !text_parts.is_empty() {
                let mut result = text_parts.join("\n").trim().to_string();
                if result.chars().count() > NOTIFICATION_BODY_MAX_CHARS {
                    result = Self::chars_take(&result, NOTIFICATION_BODY_MAX_CHARS);
                }
                return result;
            }
        }
        String::new()
    }

    /// `fetchLastAssistantMessageText`: `/session/{id}/message?limit=5`,
    /// preferring the given message id, else the latest assistant message
    /// that finished with "stop".
    ///
    /// 中文说明：请求最近 5 条消息，优先找 id 匹配的 assistant 消息，
    /// 否则取最后一条 finish 为 "stop" 的 assistant 消息，再提取其文本；
    /// 任一步失败都返回空串，由调用方改用其它模板变量。
    pub async fn fetch_last_assistant_message_text(
        &self,
        session_id: &str,
        message_id: Option<&str>,
    ) -> String {
        if session_id.is_empty() {
            return String::new();
        }
        let url = format!(
            "/session/{}/message?limit=5",
            encode_uri_component(session_id)
        );
        let Some(messages) = (self.fetch)(url, 3000, true).await else {
            return String::new();
        };
        let Some(entries) = messages.as_array() else {
            return String::new();
        };
        // 取消息的 info 对象（id/role/finish 元数据都在其中）。
        fn info_of(message: &Value) -> Option<&Value> {
            message.get("info")
        }
        let mut target: Option<&Value> = None;
        if let Some(message_id) = message_id.filter(|id| !id.is_empty()) {
            target = entries.iter().find(|message| {
                info_of(message)
                    .and_then(|info| info.get("id"))
                    .and_then(Value::as_str)
                    == Some(message_id)
                    && info_of(message)
                        .and_then(|info| info.get("role"))
                        .and_then(Value::as_str)
                        == Some("assistant")
            });
        }
        if target.is_none() {
            target = entries.iter().rev().find(|message| {
                info_of(message)
                    .and_then(|info| info.get("role"))
                    .and_then(Value::as_str)
                    == Some("assistant")
                    && info_of(message)
                        .and_then(|info| info.get("finish"))
                        .and_then(Value::as_str)
                        == Some("stop")
            });
        }
        let Some(target) = target else {
            return String::new();
        };
        let Some(parts) = target.get("parts").filter(|parts| parts.is_array()) else {
            return String::new();
        };
        self.extract_text_from_parts(parts, NOTIFICATION_BODY_MAX_CHARS)
    }

    // -----------------------------------------------------------------------
    // Session info + variables
    // -----------------------------------------------------------------------

    /// 写入会话标题缓存（session id 或标题为空时跳过）。
    fn cache_session_title(&self, session_id: &str, title: &str) {
        if session_id.is_empty() || title.is_empty() {
            return;
        }
        self.session_title_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(session_id.to_string(), title.to_string());
    }

    /// 读取会话标题缓存（无过期）。
    fn get_cached_session_title(&self, session_id: &str) -> Option<String> {
        self.session_title_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(session_id)
            .cloned()
    }

    /// `maybeCacheSessionInfoFromEvent`.
    ///
    /// 中文说明：只处理 session.updated / session.created 事件，把
    /// `properties.info` 的 id/title 写入标题缓存，后续模板变量直接复用，
    /// 省去一次 engine 往返。
    pub fn maybe_cache_session_info_from_event(&self, payload: &Value) {
        let event_type = payload.get("type").and_then(Value::as_str);
        if !matches!(
            event_type,
            Some("session.updated") | Some("session.created")
        ) {
            return;
        }
        let Some(info) = payload
            .get("properties")
            .and_then(|props| props.get("info"))
            .filter(|info| info.is_object())
        else {
            return;
        };
        let id = info.get("id").and_then(Value::as_str).unwrap_or_default();
        let title = info
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or_default();
        self.cache_session_title(id, title);
    }

    /// Engine JSON GET through the runtime's fetch seam (shared with the
    /// trigger runtime's parent-chain / goal / title lookups).
    ///
    /// 中文说明：对触发器运行时暴露的通用 engine JSON GET 入口——
    /// 父链 / goal / 标题查询共用同一访问缝。
    pub async fn fetch_json(&self, url: String, timeout_ms: u64, with_auth: bool) -> Option<Value> {
        (self.fetch)(url, timeout_ms, with_auth).await
    }

    /// `fetchSessionInfo`: no directory, no auth headers (mirrors JS).
    ///
    /// 中文说明：带 60s TTL 缓存；未命中时以 2s 超时、不带鉴权头请求
    /// `/session/{id}`（与 JS 一致），失败返回 None。
    async fn fetch_session_info(&self, session_id: &str) -> Option<Value> {
        if session_id.is_empty() {
            return None;
        }
        {
            let cache = self
                .session_info_cache
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if let Some(entry) = cache.get(session_id) {
                if now_ms().saturating_sub(entry.at) < SESSION_INFO_CACHE_TTL_MS {
                    return Some(entry.data.clone());
                }
            }
        }
        let url = format!("/session/{}", encode_uri_component(session_id));
        let data = (self.fetch)(url, 2000, false).await?;
        let mut cache = self
            .session_info_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        cache.insert(
            session_id.to_string(),
            SessionInfoCacheEntry {
                data: data.clone(),
                at: now_ms(),
            },
        );
        Some(data)
    }

    /// `buildTemplateVariables`.
    ///
    /// 中文说明：从事件 payload 与 settings 组装模板变量——
    /// session_name（事件 → 标题缓存 → engine 兜底）、agent_name、
    /// model_name、project_name/worktree（按 info.path 匹配 settings 项目，
    /// 否则用活动项目）、branch（git rev-parse）、last_message（留空由
    /// 调用方填充）与 session_id。
    pub async fn build_template_variables(
        &self,
        payload: &Value,
        session_id: &str,
    ) -> Map<String, Value> {
        let properties = payload.get("properties");
        let info = properties
            .and_then(|props| props.get("info"))
            .cloned()
            .unwrap_or_else(|| json!({}));
        let info = info.as_object().cloned().unwrap_or_default();

        let mut session_title = properties
            .and_then(|props| props.get("sessionTitle"))
            .and_then(Value::as_str)
            .map(str::to_string)
            .filter(|title| !title.is_empty())
            .or_else(|| {
                properties
                    .and_then(|props| props.get("session"))
                    .and_then(|session| session.get("title"))
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .or_else(|| {
                info.get("sessionTitle")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .unwrap_or_default();

        if session_title.is_empty() && !session_id.is_empty() {
            if let Some(cached) = self.get_cached_session_title(session_id) {
                session_title = cached;
            }
        }
        if session_title.is_empty() && !session_id.is_empty() {
            if let Some(session_info) = self.fetch_session_info(session_id).await {
                if let Some(title) = session_info.get("title").and_then(Value::as_str) {
                    session_title = title.to_string();
                    self.cache_session_title(session_id, &session_title);
                }
            }
        }

        let agent_name = {
            let mode = info
                .get("agent")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|agent| !agent.is_empty())
                .map(str::to_string)
                .or_else(|| {
                    info.get("mode")
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .map(str::to_string)
                })
                .unwrap_or_default();
            if mode.is_empty() {
                "Agent".to_string()
            } else {
                split_mode_tokens(&mode).join(" ")
            }
        };

        let model_name = {
            let raw = info
                .get("modelID")
                .and_then(Value::as_str)
                .map(str::trim)
                .map(str::to_string)
                .or_else(|| {
                    info.get("model")
                        .and_then(|model| model.get("modelID"))
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .map(str::to_string)
                })
                .unwrap_or_default();
            if raw.is_empty() {
                "Assistant".to_string()
            } else {
                format_model_id(Some(&raw))
            }
        };

        let mut project_name = String::new();
        let mut branch = String::new();
        let mut worktree_dir = String::new();

        let info_path = info.get("path");
        if let Some(root) = info_path
            .and_then(|path| path.get("root"))
            .and_then(Value::as_str)
            .filter(|root| !root.is_empty())
        {
            worktree_dir = root.to_string();
        } else if let Some(cwd) = info_path
            .and_then(|path| path.get("cwd"))
            .and_then(Value::as_str)
            .filter(|cwd| !cwd.is_empty())
        {
            worktree_dir = cwd.to_string();
        }

        let settings = self.store.read_raw().await;
        let projects: Vec<Value> = settings
            .get("projects")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let strip_trailing_slashes = |path: &str| path.trim_end_matches('/').to_string();

        if !worktree_dir.is_empty() {
            let normalized_dir = strip_trailing_slashes(&worktree_dir);
            let matched = projects.iter().find(|project| {
                project
                    .get("path")
                    .and_then(Value::as_str)
                    .map(|path| strip_trailing_slashes(path) == normalized_dir)
                    .unwrap_or(false)
            });
            if let Some(matched) = matched {
                let label = matched
                    .get("label")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|label| !label.is_empty());
                project_name = match label {
                    Some(label) => label.to_string(),
                    None => path_basename(&normalized_dir),
                };
            } else {
                project_name = path_basename(&normalized_dir);
            }
        } else {
            let active_id = settings
                .get("activeProjectId")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let active_project = if !active_id.is_empty() {
                projects
                    .iter()
                    .find(|project| project.get("id").and_then(Value::as_str) == Some(active_id))
            } else {
                projects.first()
            };
            if let Some(active_project) = active_project {
                let label = active_project
                    .get("label")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|label| !label.is_empty());
                let path = active_project
                    .get("path")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                project_name = match label {
                    Some(label) => label.to_string(),
                    None => path
                        .as_ref()
                        .map(|path| path_basename(path))
                        .unwrap_or_default(),
                };
                if let Some(path) = path {
                    worktree_dir = path;
                }
            }
        }

        if !worktree_dir.is_empty() {
            branch = self.git_current_branch(&worktree_dir).await;
        }

        let mut variables = Map::new();
        variables.insert(
            "project_name".into(),
            Value::String(project_name.trim().to_string()),
        );
        variables.insert("worktree".into(), Value::String(worktree_dir));
        variables.insert("branch".into(), Value::String(branch.trim().to_string()));
        variables.insert("session_name".into(), Value::String(session_title));
        variables.insert("agent_name".into(), Value::String(agent_name));
        variables.insert("model_name".into(), Value::String(model_name));
        variables.insert("last_message".into(), Value::String(String::new()));
        variables.insert("session_id".into(), Value::String(session_id.to_string()));
        variables
    }

    /// `simple-git revparse(['--abbrev-ref', 'HEAD'])` with a 3s timeout.
    /// The git-service binary resolver is module-private, so `git` from
    /// PATH is used directly (noted gap).
    ///
    /// 中文说明：在工作目录执行 `git rev-parse --abbrev-ref HEAD`，3 秒
    /// 超时；任何失败（超时/非零退出/非 UTF-8 输出）都返回空串。
    async fn git_current_branch(&self, worktree_dir: &str) -> String {
        let output = tokio::time::timeout(
            Duration::from_millis(3000),
            tokio::process::Command::new(&self.git_binary)
                .arg("rev-parse")
                .arg("--abbrev-ref")
                .arg("HEAD")
                .current_dir(worktree_dir)
                .stdin(std::process::Stdio::null())
                .output(),
        )
        .await;
        match output {
            Ok(Ok(output)) if output.status.success() => {
                String::from_utf8_lossy(&output.stdout).trim().to_string()
            }
            _ => String::new(),
        }
    }
}

/// `path.split('/').filter(Boolean).pop() || ''`.
///
/// 中文说明：取路径最后一个非空 `/` 段，作为项目名的目录名回退。
fn path_basename(path: &str) -> String {
    path.split('/')
        .filter(|segment| !segment.is_empty())
        .last()
        .unwrap_or_default()
        .to_string()
}

/// `String(value)` for template substitution.
///
/// 中文说明：模板变量转字符串——字符串原样、null 为空串、布尔/数字
/// 用字面量，其余 JSON 序列化（对应 JS 的 String(value)）。
fn value_to_display_string(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => String::new(),
        Value::Bool(flag) => flag.to_string(),
        Value::Number(number) => number.to_string(),
        other => other.to_string(),
    }
}

/// 模板运行时单元测试：验证与 JS 契约对齐的变量解析、文本提取、
/// 缓存行为与格式化规则。
#[cfg(test)]
mod tests {
    use super::*;

    /// 创建一次性临时目录存放测试 settings.json。
    fn temp_dir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "notif-template-{}-{}",
            std::process::id(),
            now_ms() * 1000 + rand::random::<u64>() % 100_000
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    /// 构造绑定指定 settings 与 fetch 桩的 `TemplateRuntime`
    /// （连同其临时目录返回）。
    fn runtime_with_fetch(
        settings: Map<String, Value>,
        fetch: EngineJsonFetch,
    ) -> (Arc<TemplateRuntime>, std::path::PathBuf) {
        let dir = temp_dir();
        let store = crate::settings::store_for_path(&dir.join("settings.json"));
        // Persist the settings synchronously before construction.
        if !settings.is_empty() {
            let path = dir.join("settings.json");
            std::fs::write(
                &path,
                serde_json::to_string_pretty(&Value::Object(settings)).unwrap(),
            )
            .expect("settings");
        }
        (TemplateRuntime::new(store, fetch, "git".to_string()), dir)
    }

    /// 永远返回 None 的 fetch 桩：模拟 engine 不可达。
    fn noop_fetch() -> EngineJsonFetch {
        Arc::new(|_url: String, _timeout: u64, _auth: bool| Box::pin(async { None::<Value> }))
    }

    /// 验证 Zen 退役后无可选模型，且无存储时 resolve 返回空串。
    #[tokio::test]
    async fn returns_no_selectable_zen_models_after_provider_retirement() {
        let (runtime, _dir) = runtime_with_fetch(Map::new(), noop_fetch());
        assert_eq!(runtime.fetch_free_zen_models().await, Vec::<Value>::new());
        assert_eq!(runtime.resolve_zen_model(None).await, "");
    }

    /// 验证 settings 中的 zenModel 被原样保留，override 优先且会去空白。
    #[tokio::test]
    async fn preserves_stored_zen_model_value_for_compatibility() {
        let mut settings = Map::new();
        settings.insert("zenModel".to_string(), json!("trinity-large-preview-free"));
        let (runtime, _dir) = runtime_with_fetch(settings, noop_fetch());
        assert_eq!(
            runtime.resolve_zen_model(None).await,
            "trinity-large-preview-free"
        );
        assert_eq!(
            runtime.resolve_zen_model(Some(" override ")).await,
            "override"
        );
    }

    /// 验证 payload 正文提取会排除 reasoning part，只保留 text part。
    #[tokio::test]
    async fn excludes_reasoning_parts_from_payload_message_text() {
        let (runtime, _dir) = runtime_with_fetch(Map::new(), noop_fetch());
        let payload = json!({
            "properties": {
                "info": {
                    "parts": [
                        { "type": "reasoning", "text": "private chain of thought" },
                        { "type": "text", "text": "final answer" },
                    ],
                },
            }
        });
        assert_eq!(runtime.extract_last_message_text(&payload), "final answer");
    }

    /// 验证缺 `type` 的 part 即使带 text/content 字段也不计入正文。
    #[tokio::test]
    async fn ignores_untyped_parts_even_when_they_contain_text() {
        let (runtime, _dir) = runtime_with_fetch(Map::new(), noop_fetch());
        let payload = json!({
            "properties": {
                "info": {
                    "parts": [
                        { "text": "untyped text" },
                        { "content": "untyped content" },
                        { "type": "text", "text": "typed final answer" },
                    ],
                },
            }
        });
        assert_eq!(
            runtime.extract_last_message_text(&payload),
            "typed final answer"
        );
    }

    /// 验证按 id 或 finish=stop 选取 assistant 消息并排除 reasoning 文本。
    #[tokio::test]
    async fn excludes_reasoning_parts_when_fetching_assistant_messages() {
        let fetch: EngineJsonFetch = Arc::new(|_url, _timeout, _auth| {
            Box::pin(async {
                Some(json!([
                    {
                        "info": { "id": "msg-1", "role": "assistant", "finish": "stop" },
                        "parts": [
                            { "type": "reasoning", "text": "private chain of thought" },
                            { "type": "text", "text": "final answer" },
                        ],
                    },
                ]))
            })
        });
        let (runtime, _dir) = runtime_with_fetch(Map::new(), fetch);
        assert_eq!(
            runtime
                .fetch_last_assistant_message_text("session-1", Some("msg-1"))
                .await,
            "final answer"
        );
        // Miss on the id → falls back to the last assistant/stop message.
        assert_eq!(
            runtime
                .fetch_last_assistant_message_text("session-1", Some("other"))
                .await,
            "final answer"
        );
        assert_eq!(
            runtime.fetch_last_assistant_message_text("", None).await,
            ""
        );
    }

    /// 验证缺失/null 变量替换为空串、非 word token 的花括号原样保留。
    #[tokio::test]
    async fn resolves_templates_with_missing_variables_as_empty() {
        let (runtime, _dir) = runtime_with_fetch(Map::new(), noop_fetch());
        let mut variables = Map::new();
        variables.insert("agent_name".to_string(), json!("Build"));
        variables.insert("missing".to_string(), Value::Null);
        assert_eq!(
            runtime.resolve_notification_template("{agent_name} is ready — {missing}!", &variables),
            "Build is ready — !"
        );
        // Non-word keys stay verbatim.
        assert_eq!(
            runtime.resolve_notification_template("{not a token} {user.name}", &variables),
            "{not a token} {user.name}"
        );
    }

    /// 验证 `{last_message}` 模板只在变量含非空白正文时才应用。
    #[tokio::test]
    async fn applies_resolved_message_only_with_content() {
        let (runtime, _dir) = runtime_with_fetch(Map::new(), noop_fetch());
        let mut variables = Map::new();
        variables.insert("last_message".to_string(), json!("   "));
        assert!(!runtime.should_apply_resolved_template_message(
            "{last_message}",
            "   ",
            &variables
        ));
        variables.insert("last_message".to_string(), json!("real text"));
        assert!(runtime.should_apply_resolved_template_message(
            "{last_message}",
            "real text",
            &variables
        ));
        assert!(!runtime.should_apply_resolved_template_message("{last_message}", "", &variables));
        assert!(runtime.should_apply_resolved_template_message("plain", "x", &variables));
    }

    /// 验证 session.updated 事件预热标题缓存，且其它事件类型不污染缓存。
    #[tokio::test]
    async fn caches_session_titles_from_events_and_uses_them_for_variables() {
        let (runtime, _dir) = runtime_with_fetch(Map::new(), noop_fetch());
        runtime.maybe_cache_session_info_from_event(&json!({
            "type": "session.updated",
            "properties": { "info": { "id": "ses_9", "title": "Refactor the parser" } },
        }));
        // Other event types never pollute the cache.
        runtime.maybe_cache_session_info_from_event(&json!({
            "type": "message.updated",
            "properties": { "info": { "id": "ses_10", "title": "nope" } },
        }));
        let payload = json!({ "properties": { "info": { "mode": "build", "modelID": "claude-sonnet-4-5" } } });
        let variables = runtime.build_template_variables(&payload, "ses_9").await;
        assert_eq!(variables["session_name"], json!("Refactor the parser"));
        assert_eq!(variables["agent_name"], json!("Build"));
        assert_eq!(variables["model_name"], json!("Claude Sonnet 4.5"));
        assert_eq!(variables["session_id"], json!("ses_9"));
        assert_eq!(variables["last_message"], json!(""));
        assert_eq!(variables["project_name"], json!(""));
    }

    /// 验证按 info.path 匹配项目、空白 label 回退目录名、git 失败时 branch 为空。
    #[tokio::test]
    async fn resolves_project_and_branch_from_settings() {
        let mut settings = Map::new();
        settings.insert(
            "projects".to_string(),
            json!([
                { "id": "p2", "path": "/tmp/second", "label": "Second" },
                { "id": "p1", "path": "/work/proj/", "label": "  " },
            ]),
        );
        let (runtime, _dir) = runtime_with_fetch(settings, noop_fetch());
        let payload = json!({
            "properties": {
                "info": {
                    "path": { "root": "/work/proj" },
                    "agent": "code-review",
                    "modelID": "gpt-4o-mini",
                },
            }
        });
        let variables = runtime.build_template_variables(&payload, "ses_x").await;
        // Blank label → folder name; trailing slash normalization matched.
        assert_eq!(variables["project_name"], json!("proj"));
        assert_eq!(variables["worktree"], json!("/work/proj"));
        assert_eq!(variables["agent_name"], json!("Code Review"));
        assert_eq!(variables["model_name"], json!("Gpt 4o Mini"));
        // Branch stays empty when git fails (bogus worktree in tests).
        assert_eq!(variables["branch"], json!(""));
    }

    /// 验证 payload 无路径信息时回退到 activeProjectId 指定的项目。
    #[tokio::test]
    async fn falls_back_to_active_project_without_a_path() {
        let mut settings = Map::new();
        settings.insert("activeProjectId".to_string(), json!("p1"));
        settings.insert(
            "projects".to_string(),
            json!([{ "id": "p1", "path": "/work/first", "label": "First" }]),
        );
        let (runtime, _dir) = runtime_with_fetch(settings, noop_fetch());
        let payload = json!({ "properties": { "info": {} } });
        let variables = runtime.build_template_variables(&payload, "ses_y").await;
        assert_eq!(variables["project_name"], json!("First"));
        assert_eq!(variables["worktree"], json!("/work/first"));
    }

    /// 验证 session info 的 60s 缓存（仅触发一次请求）且请求不带鉴权头。
    #[tokio::test]
    async fn session_info_fetch_caches_and_omits_auth() {
        let calls = Arc::new(Mutex::new(Vec::<(String, u64, bool)>::new()));
        let calls_for_fetch = Arc::clone(&calls);
        let fetch: EngineJsonFetch = Arc::new(move |url, timeout, with_auth| {
            let calls = Arc::clone(&calls_for_fetch);
            Box::pin(async move {
                calls
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push((url, timeout, with_auth));
                Some(json!({ "title": "Fetched title" }))
            })
        });
        let (runtime, _dir) = runtime_with_fetch(Map::new(), fetch);
        let payload = json!({ "properties": { "info": {} } });
        let variables = runtime.build_template_variables(&payload, "ses_z").await;
        assert_eq!(variables["session_name"], json!("Fetched title"));
        // Second build hits the 60s cache: still exactly one fetch.
        runtime.build_template_variables(&payload, "ses_z").await;
        let recorded = calls.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].0, "/session/ses_z");
        assert_eq!(recorded[0].1, 2000);
        assert!(!recorded[0].2, "fetchSessionInfo sends no auth header");
    }

    /// 验证 format_mode / format_model_id 的默认值与分词美化规则。
    #[test]
    fn format_mode_and_model_defaults() {
        assert_eq!(format_mode(None), "Agent");
        assert_eq!(format_mode(Some("  ")), "Agent");
        assert_eq!(format_mode(Some("plan-mode")), "Plan Mode");
        assert_eq!(format_model_id(None), "Assistant");
        assert_eq!(
            format_model_id(Some("claude-3-7-sonnet")),
            "Claude 3.7 Sonnet"
        );
        assert_eq!(format_model_id(Some("my_model")), "My Model");
    }

    /// 验证空白文本原样返回、普通短文本走本地回退仍有输出。
    #[tokio::test]
    async fn summarize_text_returns_local_fallback_for_short_text() {
        let (runtime, _dir) = runtime_with_fetch(Map::new(), noop_fetch());
        let summarized = runtime.summarize_text("plain short text", 50).await;
        assert!(!summarized.is_empty());
        // Empty/whitespace text passes through untouched.
        assert_eq!(runtime.summarize_text("", 50).await, "");
        assert_eq!(runtime.summarize_text("   ", 50).await, "   ");
    }
}
