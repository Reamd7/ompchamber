//! Port of `opencode/config-mutation-response.js`.

use serde_json::{Value, json};

/// `buildDeferredRestartResponse(message)`.
pub(crate) fn deferred_restart_response(message: &str) -> Value {
    json!({
        "success": true,
        "requiresReload": false,
        "requiresRestart": true,
        "restartDeferred": true,
        "message": message,
    })
}
