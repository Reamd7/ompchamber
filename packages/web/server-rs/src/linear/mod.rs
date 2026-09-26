//! Port of `packages/web/server/lib/linear/` — Linear OAuth (PKCE + public
//! callback broker), token storage, GraphQL issue access, team-to-project
//! mapping, session status comments, and the `/linear` + `/api/linear/*`
//! routes. See `DOCUMENTATION.md` in the JS module for the product contract.

use std::collections::HashMap;
use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::{Arc, LazyLock, Mutex, Weak};

use crate::context::RouterContext;

pub mod auth;
pub mod client;
pub mod http;
pub mod issues;
pub mod mapping;
pub mod oauth;
pub mod parse;
pub mod routes;
pub mod status;
pub mod status_runtime;
pub mod teams;

#[cfg(test)]
mod tests;

/// Shared per-data-dir state (JS module-level singletons). One instance per
/// resolved Linear data dir, process-wide, so the router and the hub
/// consumer see the same pending authorizations and in-flight dedupe maps.
pub struct LinearState {
    data_dir: std::path::PathBuf,
    /// Test-only env overrides (production reads the process environment on
    /// every call, exactly like the JS `readEnv`).
    env: Option<HashMap<String, String>>,
    transport: Arc<dyn http::HttpTransport>,
    /// JS `pendingByState`.
    pending: Mutex<HashMap<String, oauth::PendingAuthorization>>,
    /// JS `brokerPollsByState`.
    broker_polls: Mutex<
        HashMap<String, SharedFuture<Result<Option<oauth::AuthorizationResult>, LinearError>>>,
    >,
    /// JS `inFlightRefreshByWorkspace`.
    refresh_inflight: Mutex<HashMap<String, SharedFuture<Result<Option<String>, LinearError>>>>,
    /// JS `inflight` in status.js.
    status_inflight: Mutex<HashMap<String, SharedFuture<Result<serde_json::Value, LinearError>>>>,
}

/// A cloneable shared future (the JS pattern of handing out the same
/// in-flight promise to concurrent callers).
pub type SharedFuture<T> = futures::future::Shared<Pin<Box<dyn Future<Output = T> + Send>>>;

pub fn shared<T: Clone>(future: Pin<Box<dyn Future<Output = T> + Send>>) -> SharedFuture<T> {
    futures::FutureExt::shared(future)
}

impl LinearState {
    pub fn new(
        data_dir: std::path::PathBuf,
        env: Option<HashMap<String, String>>,
        transport: Arc<dyn http::HttpTransport>,
    ) -> Self {
        Self {
            data_dir,
            env,
            transport,
            pending: Mutex::new(HashMap::new()),
            broker_polls: Mutex::new(HashMap::new()),
            refresh_inflight: Mutex::new(HashMap::new()),
            status_inflight: Mutex::new(HashMap::new()),
        }
    }

    /// JS `readEnv` honoring the test overrides.
    fn env_value(&self, name: &str) -> String {
        match &self.env {
            Some(overrides) => overrides
                .get(name)
                .map(|v| v.trim().to_string())
                .unwrap_or_default(),
            None => env_value_raw(name),
        }
    }
}

/// JS `readEnv` against the process environment.
pub fn env_value_raw(name: &str) -> String {
    std::env::var(name)
        .map(|v| v.trim().to_string())
        .unwrap_or_default()
}

/// JS `Date.now()` in milliseconds.
pub fn now_ms() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as f64)
        .unwrap_or(0.0)
}

/// One shared state per resolved data dir (the JS module state is global).
pub fn shared_state(ctx: &RouterContext) -> Arc<LinearState> {
    static REGISTRY: LazyLock<Mutex<HashMap<std::path::PathBuf, Weak<LinearState>>>> =
        LazyLock::new(|| Mutex::new(HashMap::new()));
    let data_dir = auth::resolve_data_dir();
    let mut registry = REGISTRY.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(existing) = registry.get(&data_dir).and_then(Weak::upgrade) {
        return existing;
    }
    let state = Arc::new(LinearState::new(
        data_dir.clone(),
        None,
        Arc::new(http::ReqwestTransport::new(ctx.engine.http().clone())),
    ));
    registry.insert(data_dir, Arc::downgrade(&state));
    state
}

/// The linear module router. `/linear/*` stays public (Linear's redirect);
/// `/api/linear/*` sits behind the shared `/api` UI-auth gate applied in
/// `main.rs`, exactly like the JS route registration order.
pub fn router(ctx: RouterContext) -> axum::Router {
    routes::router(shared_state(&ctx))
}

/// Hub consumer entry (JS `createLinearSessionRuntime` in `index.js`).
/// Wiring into the global message stream lands with the index.rs port.
pub fn session_status_runtime(ctx: RouterContext) -> status_runtime::LinearSessionStatusRuntime {
    status_runtime::LinearSessionStatusRuntime::new(shared_state(&ctx))
}

/// The single error type for the linear module: carries the JS error's
/// `code`, numeric `status`, `userError` flag, and OAuth `origin`.
#[derive(Debug, Clone, PartialEq)]
pub struct LinearError {
    pub message: String,
    pub code: Option<String>,
    pub status: Option<u16>,
    pub user_error: bool,
    pub origin: Option<oauth::AuthOrigin>,
}

impl LinearError {
    /// A plain JS `Error` (no code, no status).
    pub fn plain(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            code: None,
            status: None,
            user_error: false,
            origin: None,
        }
    }

    /// A `LinearOAuthError` with its default `LINEAR_OAUTH_FAILED` code.
    pub fn oauth(message: impl Into<String>, code: &str) -> Self {
        Self {
            message: message.into(),
            code: Some(code.to_string()),
            status: None,
            user_error: false,
            origin: None,
        }
    }

    pub fn oauth_with_status(message: impl Into<String>, code: &str, status: Option<u16>) -> Self {
        Self {
            status,
            ..Self::oauth(message, code)
        }
    }

    pub fn oauth_with_origin(
        message: impl Into<String>,
        code: &str,
        origin: Option<oauth::AuthOrigin>,
    ) -> Self {
        Self {
            origin,
            ..Self::oauth(message, code)
        }
    }

    /// A `LinearApiError` with its HTTP status.
    pub fn api(message: impl Into<String>, status: u16) -> Self {
        Self {
            message: message.into(),
            code: None,
            status: Some(status),
            user_error: false,
            origin: None,
        }
    }

    pub fn api_with_user_flag(message: impl Into<String>, status: u16, user_error: bool) -> Self {
        Self {
            user_error,
            ..Self::api(message, status)
        }
    }

    /// `error.code = 'INVALID'` (user-facing validation failure).
    pub fn invalid(message: impl Into<String>) -> Self {
        Self {
            code: Some("INVALID".to_string()),
            ..Self::plain(message)
        }
    }

    /// `LinearMappingError`/`LinearSessionStatusError` with `MALFORMED`.
    pub fn mapping(message: impl Into<String>) -> Self {
        Self {
            code: Some("MALFORMED".to_string()),
            ..Self::plain(message)
        }
    }

    pub fn session_malformed(message: impl Into<String>) -> Self {
        Self::mapping(message)
    }

    /// Numeric HTTP status like JS `error.status`.
    pub fn http_status(&self) -> Option<u16> {
        self.status
    }
}

impl std::fmt::Display for LinearError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for LinearError {}

/// JS `writeJsonFile`: atomic tmp-file write with 0600 permissions.
pub fn write_file_atomic_600(path: &Path, body: &str) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp_file = path.with_file_name(format!(
        "{}.{}.{}.tmp",
        path.file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default(),
        std::process::id(),
        now_ms() as u64
    ));
    std::fs::write(&tmp_file, body)?;
    let _ = std::fs::set_permissions(&tmp_file, std::fs::Permissions::from_mode(0o600));
    std::fs::rename(&tmp_file, path)?;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    Ok(())
}

/// Extract the top-level key order of a JSON object document the way JS
/// `Object.keys` reports insertion order. Used by the session-status dedupe
/// file, whose "keep the newest 500" pruning depends on it.
pub fn top_level_key_order(text: &str) -> Vec<String> {
    let mut keys = Vec::new();
    let bytes = text.as_bytes();
    let mut position = 0usize;
    let mut depth = 0usize;
    let mut expect_key = false;
    while position < bytes.len() {
        match bytes[position] {
            b'{' => {
                depth += 1;
                expect_key = depth == 1;
                position += 1;
            }
            b'[' => {
                depth += 1;
                expect_key = false;
                position += 1;
            }
            b'}' | b']' => {
                depth = depth.saturating_sub(1);
                expect_key = false;
                position += 1;
            }
            b',' if depth == 1 => {
                expect_key = true;
                position += 1;
            }
            b'"' if depth == 1 && expect_key => {
                let (key, next) = read_json_string(bytes, position);
                let mut scan = next;
                while scan < bytes.len() && (bytes[scan] as char).is_ascii_whitespace() {
                    scan += 1;
                }
                if scan < bytes.len() && bytes[scan] == b':' {
                    keys.push(key);
                }
                position = scan;
                expect_key = false;
            }
            b'"' => {
                let (_, next) = read_json_string(bytes, position);
                position = next;
            }
            _ => position += 1,
        }
    }
    keys
}

/// Read a JSON string starting at `position` (the opening quote); returns the
/// decoded contents and the index just past the closing quote.
fn read_json_string(bytes: &[u8], position: usize) -> (String, usize) {
    let mut out = String::new();
    let mut index = position + 1;
    while index < bytes.len() {
        match bytes[index] {
            b'\\' if index + 1 < bytes.len() => {
                match bytes[index + 1] {
                    b'n' => out.push('\n'),
                    b't' => out.push('\t'),
                    b'r' => out.push('\r'),
                    b'b' => out.push('\u{8}'),
                    b'f' => out.push('\u{c}'),
                    b'u' => {
                        let hex = bytes
                            .get(index + 2..index + 6)
                            .and_then(|slice| std::str::from_utf8(slice).ok())
                            .and_then(|slice| u32::from_str_radix(slice, 16).ok());
                        if let Some(code) = hex
                            && let Some(character) = char::from_u32(code)
                        {
                            out.push(character);
                        }
                        index += 4;
                    }
                    other => out.push(other as char),
                }
                index += 2;
            }
            b'"' => return (out, index + 1),
            _ => {
                let start = index;
                while index < bytes.len() && bytes[index] != b'"' && bytes[index] != b'\\' {
                    index += 1;
                }
                out.push_str(&String::from_utf8_lossy(&bytes[start..index]));
            }
        }
    }
    (out, index)
}
