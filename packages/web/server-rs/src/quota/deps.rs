//! Dependency seams shared by every quota provider.
//!
//! JS providers close over ambient state: the global `fetch`, `Date.now`,
//! `process.env`, `os.homedir`, the OpenCode `auth.json`, the macOS Keychain,
//! and (Cursor import only) the `sqlite3` binary. [`QuotaDeps`] makes each of
//! those injectable so tests drive providers against fakes exactly where the
//! JS tests stub `fetch` / mock modules.
//!
//! Credential hygiene: nothing in this module logs or persists token values;
//! secrets only ever flow into request headers or the 0600 credential store.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{Map, Value, json};

use crate::quota::http::{HttpFetch, default_fetch};

/// The credential blob shape `auth.json` holds per provider.
pub type ReadAuth = Arc<dyn Fn() -> Result<Value, String> + Send + Sync>;
pub type WriteAuth = Arc<dyn Fn(&Value) -> Result<(), String> + Send + Sync>;

#[derive(Clone)]
pub struct QuotaDeps {
    pub http: HttpFetch,
    /// `Date.now()` in epoch milliseconds.
    pub now: Arc<dyn Fn() -> u64 + Send + Sync>,
    /// `process.env[name]`, empty values filtered like `asNonEmptyString`.
    pub env: Arc<dyn Fn(&str) -> Option<String> + Send + Sync>,
    /// `os.homedir()`.
    pub home_dir: Arc<dyn Fn() -> Option<PathBuf> + Send + Sync>,
    /// `readAuthFile()` — `Err` mirrors the JS throw
    /// "Failed to read OpenCode auth configuration".
    pub read_auth: ReadAuth,
    /// `writeAuthFile(auth)`.
    pub write_auth: WriteAuth,
    /// Raw `security find-generic-password -s 'Claude Code-credentials' -w`
    /// stdout on macOS (JSON blob); None elsewhere / when unavailable.
    pub keychain: Arc<dyn Fn() -> Option<String> + Send + Sync>,
    /// `sqlite3 -json <db> <query>` first-row `value` (Cursor state.vscdb).
    pub sqlite_value: Arc<dyn Fn(&Path, &str) -> Option<String> + Send + Sync>,
}

impl QuotaDeps {
    /// Production wiring over the real environment.
    pub fn real() -> Self {
        Self {
            http: default_fetch(),
            now: Arc::new(|| {
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0)
            }),
            env: Arc::new(|name| std::env::var(name).ok().filter(|v| !v.is_empty())),
            home_dir: Arc::new(crate::config::home_dir),
            read_auth: Arc::new(real_read_auth),
            write_auth: Arc::new(real_write_auth),
            keychain: Arc::new(real_keychain),
            sqlite_value: Arc::new(real_sqlite_value),
        }
    }

    pub fn now_ms(&self) -> u64 {
        (self.now)()
    }

    /// `readAuthFile()` mapped onto the JS provider convention: a read failure
    /// becomes the thrown error message, a missing file becomes `{}`.
    pub fn read_auth_value(&self) -> Result<Value, String> {
        (self.read_auth)()
    }
}

// ============== opencode/auth.js ==============

/// `AUTH_FILE` = `~/.local/share/opencode/auth.json` (JS hardcodes the path).
pub fn real_auth_path(deps: &QuotaDeps) -> Option<PathBuf> {
    (deps.home_dir)().map(|home| {
        home.join(".local")
            .join("share")
            .join("opencode")
            .join("auth.json")
    })
}

fn real_read_auth() -> Result<Value, String> {
    let Some(path) = crate::config::home_dir().map(|home| {
        home.join(".local")
            .join("share")
            .join("opencode")
            .join("auth.json")
    }) else {
        return Ok(json!({}));
    };
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(json!({})),
        Err(_) => return Ok(json!({})),
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(json!({}));
    }
    serde_json::from_str(trimmed)
        .map_err(|_| "Failed to read OpenCode auth configuration".to_string())
}

fn real_write_auth(auth: &Value) -> Result<(), String> {
    let Some(home) = crate::config::home_dir() else {
        return Err("Failed to write OpenCode auth configuration".to_string());
    };
    let dir = home.join(".local").join("share").join("opencode");
    let auth_file = dir.join("auth.json");
    write_auth_file_at(&dir, &auth_file, auth)
        .map_err(|_| "Failed to write OpenCode auth configuration".to_string())
}

/// `writeAuthFile`: 0700 directory, backup copy, then a 0600 write.
pub(crate) fn write_auth_file_at(
    dir: &Path,
    auth_file: &Path,
    auth: &Value,
) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        let mut builder = std::fs::DirBuilder::new();
        builder.mode(0o700).recursive(true);
        builder.create(dir)?;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    #[cfg(not(unix))]
    std::fs::create_dir_all(dir)?;

    if auth_file.exists() {
        let backup = dir.join("auth.json.ompchamber.backup");
        std::fs::copy(auth_file, &backup)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&backup, std::fs::Permissions::from_mode(0o600))?;
        }
    }

    let body = serde_json::to_string(auth).unwrap_or_default();
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(auth_file)?;
        file.write_all(body.as_bytes())?;
        std::fs::set_permissions(auth_file, std::fs::Permissions::from_mode(0o600))?;
    }
    #[cfg(not(unix))]
    std::fs::write(auth_file, body)?;
    Ok(())
}

// ============== claude/auth.js keychain ==============

fn real_keychain() -> Option<String> {
    if !cfg!(target_os = "macos") {
        return None;
    }
    let mut command = Command::new("security");
    command
        .args([
            "find-generic-password",
            "-s",
            "Claude Code-credentials",
            "-w",
        ])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .stdout(Stdio::piped());
    run_command_capture(&mut command, Duration::from_secs(10))
}

fn real_sqlite_value(db: &Path, query: &str) -> Option<String> {
    let mut command = Command::new("sqlite3");
    command
        .args(["-json"])
        .arg(db)
        .arg(query)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .stdout(Stdio::piped());
    let rows = run_command_capture(&mut command, Duration::from_secs(10))?;
    let parsed: Value = serde_json::from_str(rows.trim()).ok()?;
    let value = parsed.as_array()?.first()?.get("value")?.as_str()?;
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// `execFileSync` with a timeout: poll the child, kill it when the deadline
/// passes, and only surface stdout of successful exits.
fn run_command_capture(command: &mut Command, timeout: Duration) -> Option<String> {
    let mut child = command.spawn().ok()?;
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => {
                let mut stdout = child.stdout.take()?;
                let mut output = String::new();
                stdout.read_to_string(&mut output).ok()?;
                return Some(output);
            }
            Ok(Some(_)) => return None,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

/// Read the OpenCode config layers the Zhipu provider consults
/// (`readConfigLayers()` without a working directory resolves only the user
/// and `OPENCODE_CONFIG` layers; the project layer is null without one).
pub fn read_opencode_user_config(deps: &QuotaDeps) -> Option<Map<String, Value>> {
    let config_dir = (deps.home_dir)()?.join(".config").join("opencode");
    let user_path = [
        config_dir.join("config.json"),
        config_dir.join("opencode.json"),
        config_dir.join("opencode.jsonc"),
    ]
    .into_iter()
    .find(|path| path.exists())
    .or_else(|| Some(config_dir.join("config.json")))?;

    let user_layer = read_config_layer(&user_path)?;
    let mut merged = user_layer;

    if let Some(custom) = (deps.env)("OPENCODE_CONFIG") {
        let custom_path = if Path::new(&custom).is_absolute() {
            PathBuf::from(custom)
        } else {
            std::env::current_dir().unwrap_or_default().join(custom)
        };
        if let Some(custom_layer) = read_config_layer(&custom_path) {
            merged = merge_configs(&merged, &custom_layer);
        }
    }
    Some(merged)
}

/// `readConfigFile` — broken JSONC yields no layer at all (the caller treats
/// the provider as unconfigured, matching the JS catch in `getApiKey`).
fn read_config_layer(path: &Path) -> Option<Map<String, Value>> {
    let content = std::fs::read_to_string(path).ok()?;
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return Some(Map::new());
    }
    let options = jsonc_parser::ParseOptions {
        allow_comments: true,
        allow_loose_object_property_names: false,
        allow_trailing_commas: true,
    };
    match jsonc_parser::parse_to_serde_value(trimmed, &options) {
        Ok(Some(Value::Object(map))) => Some(map),
        Ok(_) => Some(Map::new()),
        Err(_) => None,
    }
}

/// `mergeConfigs` — deep merge for plain objects, replace otherwise.
fn merge_configs(
    base: &Map<String, Value>,
    override_map: &Map<String, Value>,
) -> Map<String, Value> {
    let mut result = base.clone();
    for (key, value) in override_map {
        match (result.get(key), value) {
            (Some(Value::Object(base_inner)), Value::Object(override_inner)) => {
                let merged = merge_configs(base_inner, override_inner);
                result.insert(key.clone(), Value::Object(merged));
            }
            _ => {
                result.insert(key.clone(), value.clone());
            }
        }
    }
    result
}
