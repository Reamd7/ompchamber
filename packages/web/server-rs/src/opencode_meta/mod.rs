//! Port of the OpenChamber meta surfaces from `server/lib/opencode/`:
//!
//! - `openchamber-routes.js` → [`openchamber_routes`] —
//!   `GET /api/ompchamber/update-check`, `POST /api/ompchamber/update-install`,
//!   `GET /api/ompchamber/models-metadata`, `GET /api/zen/models`.
//! - `models-metadata.js` → [`models_metadata`] — the models.dev catalog
//!   cache those routes serve.
//! - `npm-registry.js` → [`npm_registry`] — npm metadata lookups (library;
//!   no routes of its own, like the JS).
//! - `project-icon-routes.js` → [`project_icon_routes`] —
//!   `GET/PUT/DELETE /api/projects/{projectId}/icon` and
//!   `POST /api/projects/{projectId}/icon/discover`.
//!
//! Update flows consume the ported `package_manager` runtime exactly like
//! the JS imports `../package-manager.js`. The install-side side effects
//! that can only run in the Node distribution (spawning a global
//! package-manager install that replaces the running server, then
//! restarting it) return the JS response shape with honest
//! not-available semantics — see `openchamber_routes` module docs.
//!
//! 中文说明：OpenChamber 元信息模块聚合层（对应 JS `server/lib/opencode/`
//! 的 openchamber-routes / models-metadata / npm-registry / project-icon-routes
//! 四个文件）。这里统一装配路由、共享的 Express 请求语义辅助（query 解析、
//! JSON body 解析、字符串清洗）以及模块级状态 [`MetaState`]。

/// HTTP 传输接缝：请求/响应/错误类型与可注入的 fetch 函数。
mod http;
/// models.dev 模型目录缓存（TTL + 磁盘持久化 + proxy 重试）。
mod models_metadata;
/// npm 包元数据查询（1h TTL 缓存 + 在途请求去重）；库模块，无自有路由。
mod npm_registry;
/// OpenChamber 元信息路由：update-check / update-install / models-metadata / zen models。
mod openchamber_routes;
/// 项目图标路由：`GET/PUT/DELETE /api/projects/{id}/icon` 与 icon/discover。
mod project_icon_routes;

/// 路由层集成测试，与 JS 侧 Express 行为对拍。
#[cfg(test)]
mod tests;

use std::path::PathBuf;
use std::sync::Arc;

use axum::Router;
use serde_json::Value;

use crate::context::RouterContext;
use crate::package_manager::PackageManagerRuntime;
use crate::settings::SettingsStore;

use models_metadata::ModelsMetadataCache;

/// `index.js`: `MODELS_METADATA_CACHE_TTL = 5 * 60 * 1000`.
const MODELS_METADATA_CACHE_TTL_MS: u64 = 5 * 60 * 1000;

/// Module-local state (JS module-level closures capture these once).
pub(crate) struct MetaState {
/// 全局设置存储（JS 闭包捕获的 settings 单例等价物）。
    settings: Arc<SettingsStore>,
/// models.dev 目录缓存实例（update-check / models-metadata 路由共享）。
    models: Arc<ModelsMetadataCache>,
    /// `../package-manager.js` import target (detection state is
    /// process-wide inside the runtime, so one instance per router is
    // equivalent to the JS module singleton).
    package_manager: Arc<PackageManagerRuntime>,
    /// JS `ompchamberDataDir`.
    data_dir: PathBuf,
    /// JS `server.address()?.port || 3000` source (the requested bind port).
    port: u16,
}
/// 生产组合入口：创建真实 models.dev 缓存与 package-manager 运行时后装配路由。
pub fn router(ctx: RouterContext) -> Router {
    let models = Arc::new(ModelsMetadataCache::production(
        models_metadata::cache_file_path(&ctx.config.data_dir),
    ));
    router_with(ctx, models, Arc::new(PackageManagerRuntime::new()))
}

/// Composition entry for tests: injected models.dev cache + package-manager
/// runtime seams.
pub(crate) fn router_with(
    ctx: RouterContext,
    models: Arc<ModelsMetadataCache>,
    package_manager: Arc<PackageManagerRuntime>,
) -> Router {
    let state = Arc::new(MetaState {
        settings: crate::settings::store(&ctx),
        models,
        package_manager,
        data_dir: ctx.config.data_dir.clone(),
        port: ctx.config.port,
    });
    project_icon_routes::routes(state.clone()).merge(openchamber_routes::routes(state))
}

// ---------------------------------------------------------------------------
// Express request plumbing shared by the route files
// ---------------------------------------------------------------------------

/// Parsed query string: keys with all their values in order (express/qs
/// semantics — repeated keys become arrays).
pub(crate) struct QueryValues(Vec<(String, Vec<String>)>);

/// 解析原始 query string（express/qs 语义：`+` 解码为空格、重复键聚合为
/// 数组、无值键得到空串值）。
pub(crate) fn parse_query(raw: Option<&str>) -> QueryValues {
    let Some(raw) = raw else {
        return QueryValues(Vec::new());
    };
    let mut out: Vec<(String, Vec<String>)> = Vec::new();
    for piece in raw.split('&') {
        if piece.is_empty() {
            continue;
        }
        let (key, value) = match piece.split_once('=') {
            Some((key, value)) => (key, value),
            None => (piece, ""),
        };
        let key = percent_decode(key);
        let value = percent_decode(value);
        // `+` decodes to a space, like the default express query parser.
        let key = key.replace('+', " ");
        let value = value.replace('+', " ");
        if let Some(entry) = out.iter_mut().find(|(name, _)| name == &key) {
            entry.1.push(value);
        } else {
            out.push((key, vec![value]));
        }
    }
    QueryValues(out)
}

/// Percent-decode (invalid escapes pass through verbatim, like `decodeURIComponent`
/// fallbacks in qs).
fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() + 1 {
            let hex = input
                .get(index + 1..index + 3)
                .and_then(|h| u8::from_str_radix(h, 16).ok());
            if let Some(byte) = hex {
                out.push(byte);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Express 风格取值辅助，对拍 JS 侧对重复键的数组判定。
impl QueryValues {
    /// Express `Array.isArray(req.query.key) ? req.query.key[0] : req.query.key`
    /// — the first value when a key repeats.
    pub(crate) fn first(&self, key: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|(name, _)| name == key)
            .and_then(|(_, values)| values.first())
            .map(String::as_str)
    }

    /// The value only when the key appears exactly once (express renders a
    /// repeated key as an array, which `typeof === 'string'` guards reject).
    pub(crate) fn single(&self, key: &str) -> Option<&str> {
        let entry = self.0.iter().find(|(name, _)| name == key)?;
        if entry.1.len() == 1 {
            entry.1.first().map(String::as_str)
        } else {
            None
        }
    }
}

/// JS `parseString` helper from `openchamber-routes.js`: trimmed non-empty
/// string or `undefined`.
pub(crate) fn parse_string(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|trimmed| !trimmed.is_empty())
        .map(str::to_string)
}

/// Mirror `express.json({ limit: '50mb' })` body semantics: only
/// `application/json` / `application/*+json` bodies are parsed (an empty JSON
/// body parses as `{}`), anything else is left unparsed (`req.body` is
/// `undefined`), malformed JSON is a 400, and an over-limit body is a 413.
/// Returns `Ok(None)` for an unparsed body.
pub(crate) async fn parse_json_body(
    headers: &axum::http::HeaderMap,
    body: axum::body::Bytes,
) -> Result<Option<Value>, axum::response::Response> {
    use axum::http::{StatusCode, header};
    use axum::response::IntoResponse;

// express.json({ limit: '50mb' }) 的请求体大小上限。
    const BODY_LIMIT: usize = 50 * 1024 * 1024;

    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    let mime = content_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_lowercase();
    let is_json =
        mime == "application/json" || (mime.starts_with("application/") && mime.ends_with("+json"));
    if !is_json {
        return Ok(None);
    }
    if body.len() > BODY_LIMIT {
        return Err((StatusCode::PAYLOAD_TOO_LARGE, "request body too large").into_response());
    }
    if body.is_empty() {
        return Ok(Some(Value::Object(serde_json::Map::new())));
    }
    match serde_json::from_slice::<Value>(&body) {
        Ok(value) => Ok(Some(value)),
        Err(error) => Err((StatusCode::BAD_REQUEST, error.to_string()).into_response()),
    }
}

/// 当前毫秒时间戳；委托 models_metadata 的时钟入口以便测试替换。
pub(crate) fn now_ms() -> u64 {
    models_metadata::system_now_ms()
}
