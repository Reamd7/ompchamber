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
//!
//! 中文说明：本模块定义所有 quota provider 共用的依赖注入接口 [`QuotaDeps`]。
//! JS 版 provider 直接闭包引用全局状态（`fetch`、`Date.now`、`process.env`、
//! `os.homedir`、OpenCode `auth.json`、macOS Keychain，以及 Cursor 导入用的
//! `sqlite3` 二进制）；这里把每一项都做成可注入的 seam，测试用 fake 替换，
//! 与 JS 测试中 stub `fetch` / mock 模块的做法一一对应。

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{Map, Value, json};

use crate::quota::http::{HttpFetch, default_fetch};

/// The credential blob shape `auth.json` holds per provider.
///
/// 中文说明：读取 auth.json 的闭包类型；返回解析后的 JSON（通常是
/// provider 到凭据对象的映射），失败时返回错误消息字符串。
pub type ReadAuth = Arc<dyn Fn() -> Result<Value, String> + Send + Sync>;
/// 写回 auth.json 的闭包类型：输入完整凭据 JSON，落盘成功返回 `Ok(())`。
pub type WriteAuth = Arc<dyn Fn(&Value) -> Result<(), String> + Send + Sync>;

#[derive(Clone)]
/// quota provider 的依赖集合：把 JS 中隐式的全局环境（HTTP、时钟、env、
/// home 目录、auth.json 读写、Keychain、sqlite 查询）收敛为可注入字段；
/// 生产环境用 [`QuotaDeps::real`] 装配，测试注入各字段的 fake 实现。
pub struct QuotaDeps {
    /// 出站 HTTP 请求通道（对应 JS 的全局 `fetch`），provider 用它调用
    /// 厂商配额接口。
    pub http: HttpFetch,
    /// `Date.now()` in epoch milliseconds.
    ///
    /// 中文说明：当前时间的 epoch 毫秒数（对应 `Date.now()`），用于计算
    /// 窗口重置时间、缓存过期等。
    pub now: Arc<dyn Fn() -> u64 + Send + Sync>,
    /// `process.env[name]`, empty values filtered like `asNonEmptyString`.
    ///
    /// 中文说明：读取环境变量（对应 `process.env[name]`），空字符串按
    /// `asNonEmptyString` 语义过滤为 `None`。
    pub env: Arc<dyn Fn(&str) -> Option<String> + Send + Sync>,
    /// `os.homedir()`.
    ///
    /// 中文说明：当前用户 home 目录（对应 `os.homedir()`）；`None` 表示
    /// 无法确定。
    pub home_dir: Arc<dyn Fn() -> Option<PathBuf> + Send + Sync>,
    /// `readAuthFile()` — `Err` mirrors the JS throw
    /// "Failed to read OpenCode auth configuration".
    ///
    /// 中文说明：读取 OpenCode `auth.json`；`Err` 对应 JS 抛出的
    /// "Failed to read OpenCode auth configuration"。
    pub read_auth: ReadAuth,
    /// `writeAuthFile(auth)`.
    ///
    /// 中文说明：整体写回 OpenCode `auth.json`。
    pub write_auth: WriteAuth,
    /// Raw `security find-generic-password -s 'Claude Code-credentials' -w`
    /// stdout on macOS (JSON blob); None elsewhere / when unavailable.
    ///
    /// 中文说明：macOS 上 `security find-generic-password -s 'Claude
    /// Code-credentials' -w` 的原始 stdout（JSON blob）；其他平台或
    /// 不可用时为 `None`。
    pub keychain: Arc<dyn Fn() -> Option<String> + Send + Sync>,
    /// `sqlite3 -json <db> <query>` first-row `value` (Cursor state.vscdb).
    ///
    /// 中文说明：执行 `sqlite3 -json <db> <query>` 并取第一行的 `value`
    /// 字段（用于 Cursor 的 state.vscdb）。
    pub sqlite_value: Arc<dyn Fn(&Path, &str) -> Option<String> + Send + Sync>,
}

/// [`QuotaDeps`] 的构造方法与便捷读取封装。
impl QuotaDeps {
    /// Production wiring over the real environment.
    ///
    /// 中文说明：生产环境装配：真实 HTTP fetch、系统时钟、进程环境变量、
    /// 真实文件系统上的 auth.json 读写、macOS Keychain 与 sqlite3 子进程。
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

    /// 读取注入时钟的当前毫秒时间戳。
    pub fn now_ms(&self) -> u64 {
        (self.now)()
    }

    /// `readAuthFile()` mapped onto the JS provider convention: a read failure
    /// becomes the thrown error message, a missing file becomes `{}`.
    ///
    /// 中文说明：按 JS provider 约定暴露 auth 读取：读失败映射为 JS 抛出的
    /// 错误消息，文件缺失映射为空对象 `{}`。
    pub fn read_auth_value(&self) -> Result<Value, String> {
        (self.read_auth)()
    }
}

// ============== opencode/auth.js ==============

/// `AUTH_FILE` = `~/.local/share/opencode/auth.json` (JS hardcodes the path).
///
/// 中文说明：计算 OpenCode `auth.json` 的真实路径（JS 硬编码为
/// `~/.local/share/opencode/auth.json`）；home 目录未知时返回 `None`。
pub fn real_auth_path(deps: &QuotaDeps) -> Option<PathBuf> {
    (deps.home_dir)().map(|home| {
        home.join(".local")
            .join("share")
            .join("opencode")
            .join("auth.json")
    })
}

/// 生产环境 auth.json 读取：home 目录未知、文件缺失/为空或读取 IO 失败
/// 都返回 `{}`，仅 JSON 解析失败映射为 "Failed to read OpenCode auth
/// configuration"。
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

/// 生产环境 auth.json 写入：home 目录未知或写入失败均返回
/// "Failed to write OpenCode auth configuration"。
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
///
/// 中文说明：真实落盘逻辑：确保 0700 目录；若目标已存在先复制为
/// `auth.json.ompchamber.backup`（0600）；再以 0600 权限写入序列化 JSON
/// （非 Unix 平台退化为普通写入）。
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
use crate::os_compat::PermissionsExt;
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

/// 生产环境 Keychain 读取：仅在 macOS 上执行 `security find-generic-password`
/// 查询 Claude Code 凭据（stdin/stderr 丢弃，10 秒超时）；其他平台直接
/// 返回 `None`。
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

/// 生产环境 sqlite 查询：以 `-json` 模式运行 `sqlite3` 执行查询（10 秒
/// 超时），解析 stdout 取第一行的 `value` 字符串；空值或任何失败均返回
/// `None`。
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
///
/// 中文说明：带超时的同步命令执行（对应 JS 的 `execFileSync`）：轮询子
/// 进程状态，超过 deadline 先 kill 再收尸；只有成功退出的 stdout 才会被
/// 返回，其余情况一律 `None`。
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
///
/// 中文说明：读取 Zhipu provider 参考的 OpenCode 配置层：无工作目录时只
/// 解析用户层（`~/.config/opencode` 下第一个存在的配置文件）与
/// `OPENCODE_CONFIG` 指向的自定义层（相对路径按当前目录补全），两层做深
/// 合并；home 目录未知或用户层不可读时返回 `None`。
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
///
/// 中文说明：读取单个配置文件并按 JSONC 解析（允许注释与尾逗号）；空文件
/// 视为空对象层，解析失败返回 `None`——调用方据此把 provider 视为未配置，
/// 与 JS `getApiKey` 中的 catch 行为一致。
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
///
/// 中文说明：配置深合并：键冲突时双方都是纯对象则递归合并，否则用
/// override 层的值整体替换 base 层的值。
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
