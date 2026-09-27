//! Port of `opencode/config-mutation-response.js`.
//!
//! 中文说明：移植 `opencode/config-mutation-response.js`。本模块构造
//! "配置已写入、重启被推迟" 的统一 JSON 响应体，供各配置变更路由
//! （agent/command/MCP/provider 等）在返回时复用，保证前端收到一致的
//! `requiresReload` / `requiresRestart` 语义。

use serde_json::{Value, json};

/// `buildDeferredRestartResponse(message)`.
/// 中文：构造延迟重启响应体。`message` 为面向用户的提示文案；
/// `requiresRestart: true` 且 `restartDeferred: true` 表示引擎重启由
/// 客户端稍后自行触发，本次请求本身不会立即重启进程。
pub(crate) fn deferred_restart_response(message: &str) -> Value {
    json!({
        "success": true,
        "requiresReload": false,
        "requiresRestart": true,
        "restartDeferred": true,
        "message": message,
    })
}
