//! Background command execution — port of routes.js `runCommandInDirectory`,
//! the git-read cache (`runCommandWithGitReadCache`), and the exec job store
//! (`execJobs` + `pruneExecJobs` + `runExecJob`).
//!
//! 中文说明：后台命令执行子系统。`run_command_in_directory` 是唯一的
//! 命令执行原语（`shell -c` + 硬超时 SIGKILL）；`GitReadCache` 对
//! 白名单内的 `git rev-parse` 查询做结果缓存与并发去重；`ExecJobStore`
//! 维护 `/api/fs/exec` 的任务表。环境变量解析函数复刻 JS `Number()`
//! 的宽松转换与守卫条件（非法值一律回落默认值，绝不 panic）。

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};

use super::paths::random_uuid;

/// exec 任务在存储中的存活时间（30 分钟）；超时任务由 `prune` 清除。
pub const EXEC_JOB_TTL_MS: u64 = 30 * 60 * 1000;
/// git-read 缓存的最大条目数（超出后按 LRU 逐出最旧条目）。
const GIT_READ_CACHE_MAX_ENTRIES: usize = 500;
/// git-read 缓存的总字节上限（键 + stdout + stderr 合计 1 MiB）。
const GIT_READ_CACHE_MAX_BYTES: usize = 1024 * 1024;
/// 允许缓存的 `git rev-parse` 标志白名单。
const GIT_READ_FLAGS: [&str; 3] = ["--absolute-git-dir", "--git-common-dir", "--show-toplevel"];

/// 当前 Unix 时间戳（毫秒）；时钟早于 epoch 时返回 0 而非 panic。
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// `Number(raw)` with the JS `Number.isFinite && raw > 0` gate.
///
/// 中文说明：按 f64 解析环境变量值，仅当有限且严格大于 0 时采纳
/// （截断为整毫秒），否则返回默认值。
fn env_ms_positive(raw: Option<&str>, default: u64) -> u64 {
    match raw.and_then(|value| value.trim().parse::<f64>().ok()) {
        Some(value) if value.is_finite() && value > 0.0 => value as u64,
        _ => default,
    }
}

/// `Number(raw)` with the JS `Number.isFinite && raw >= 0` gate.
///
/// 中文说明：同 [`env_ms_positive`]，但允许 0（用于"0 表示禁用缓存"
/// 之类的开关语义）。
fn env_ms_non_negative(raw: Option<&str>, default: u64) -> u64 {
    match raw.and_then(|value| value.trim().parse::<f64>().ok()) {
        Some(value) if value.is_finite() && value >= 0.0 => value as u64,
        _ => default,
    }
}

/// 命令执行超时的原始解析（默认 5 分钟）；非法或非正值回落默认。
pub fn command_timeout_ms_from_raw(raw: Option<&str>) -> u64 {
    env_ms_positive(raw, 5 * 60 * 1000)
}

/// git-read 缓存 TTL 的原始解析（默认 30 秒；0 表示禁用缓存）。
pub fn git_read_cache_ttl_ms_from_raw(raw: Option<&str>) -> u64 {
    env_ms_non_negative(raw, 30 * 1000)
}

/// `git check-ignore` 超时的原始解析（默认 2500 毫秒）。
pub fn git_check_ignore_timeout_ms_from_raw(raw: Option<&str>) -> u64 {
    env_ms_non_negative(raw, 2500)
}

/// 上传字节上限的原始解析（默认 100 MiB）；f64 向下取整，非正值回落默认。
pub fn upload_max_bytes_from_raw(raw: Option<&str>) -> u64 {
    match raw.and_then(|value| value.trim().parse::<f64>().ok()) {
        Some(value) if value.is_finite() && value > 0.0 => value.floor() as u64,
        _ => 100 * 1024 * 1024,
    }
}

/// routes.js `createCommandTimeoutMs` (OMPCHAMBER_FS_EXEC_TIMEOUT_MS, default
/// 5 min). Read once per server process, like the JS.
///
/// 中文说明：读取 OMPCHAMBER_FS_EXEC_TIMEOUT_MS；与 JS 一致，仅在进程
/// 启动时求值一次（由 `FsState::new` 调用）。
pub fn command_timeout_ms() -> u64 {
    command_timeout_ms_from_raw(
        std::env::var("OMPCHAMBER_FS_EXEC_TIMEOUT_MS")
            .ok()
            .as_deref(),
    )
}

/// routes.js `createGitReadCacheTtlMs` (OMPCHAMBER_GIT_READ_CACHE_TTL_MS,
/// default 30 s; 0 disables caching).
///
/// 中文说明：读取 OMPCHAMBER_GIT_READ_CACHE_TTL_MS；0 会禁用缓存
/// （`GitReadCache::run` 直接旁路）。
pub fn git_read_cache_ttl_ms() -> u64 {
    git_read_cache_ttl_ms_from_raw(
        std::env::var("OMPCHAMBER_GIT_READ_CACHE_TTL_MS")
            .ok()
            .as_deref(),
    )
}

/// routes.js `createGitCheckIgnoreTimeoutMs` (default 2500 ms).
///
/// 中文说明：读取 OMPCHAMBER_GIT_CHECK_IGNORE_TIMEOUT_MS，约束搜索
/// 运行时的 `git check-ignore` 子进程。
pub fn git_check_ignore_timeout_ms() -> u64 {
    git_check_ignore_timeout_ms_from_raw(
        std::env::var("OMPCHAMBER_GIT_CHECK_IGNORE_TIMEOUT_MS")
            .ok()
            .as_deref(),
    )
}

/// routes.js `createUploadMaxBytes` (OMPCHAMBER_FS_UPLOAD_MAX_BYTES, default
/// 100 MiB). Read per request, like the JS.
///
/// 中文说明：读取 OMPCHAMBER_FS_UPLOAD_MAX_BYTES；与 JS 一致按请求读取
/// （upload 处理器内调用），修改环境变量对新请求立即生效。
pub fn upload_max_bytes() -> u64 {
    upload_max_bytes_from_raw(
        std::env::var("OMPCHAMBER_FS_UPLOAD_MAX_BYTES")
            .ok()
            .as_deref(),
    )
}

/// routes.js `normalizeCommand`.
///
/// 中文说明：按任意空白切分再以单空格连接，去除命令串中的多余空白。
pub fn normalize_command(command: &str) -> String {
    command.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// routes.js `isCacheableGitReadCommand`: only deterministic
/// `git rev-parse` plumbing queries with 1–3 allowlisted flags.
///
/// 中文说明：仅 `git rev-parse` 后跟 1–3 个白名单标志（共 3–5 个
/// token）视为确定性可缓存查询；组合命令或其它子命令一律不缓存。
pub fn is_cacheable_git_read_command(command: &str) -> bool {
    let normalized = normalize_command(command);
    let tokens: Vec<&str> = normalized.split(' ').collect();
    if tokens.len() < 3 || tokens.len() > 5 {
        return false;
    }
    if tokens[0] != "git" || tokens[1] != "rev-parse" {
        return false;
    }
    tokens[2..].iter().all(|flag| GIT_READ_FLAGS.contains(flag))
}

/// One executed command. `to_json` applies the exact JS field presence rules
/// (`exitCode`/`error` omitted when unknown).
///
/// 中文说明：单条命令的执行结果。`success` 仅在退出码为 0 且未超时时
/// 为 true；`to_json` 严格复刻 JS 的字段省略规则。
#[derive(Debug, Clone)]
pub struct CommandOutcome {
    /// 规范化后的命令文本（原样回显给客户端）。
    pub command: String,
    /// 退出码为 0 且未超时才为 true。
    pub success: bool,
    /// 进程退出码；被信号杀死或 spawn 失败时为 None。
    pub exit_code: Option<i32>,
    /// 标准输出（已去除首尾空白）。
    pub stdout: String,
    /// 标准错误（已去除首尾空白）。
    pub stderr: String,
    /// spawn 失败或超时的人工可读描述；正常执行为 None。
    pub error: Option<String>,
}

/// JSON 序列化辅助：对齐 JS `execCommand` 结果对象的字段出现规则。
impl CommandOutcome {
    /// JS pushes `{ command, success: false, error: 'Invalid command' }` for
    /// non-string/blank entries — no stdout/stderr/exitCode fields.
    ///
    /// 中文说明：非法命令（非字符串或空白）的固定 JSON 形状，
    /// 只含 command/success/error 三个字段。
    pub fn invalid_command_json(command: &Value) -> Value {
        json!({
            "command": command,
            "success": false,
            "error": "Invalid command",
        })
    }

    /// 序列化为 JS 形状的对象；stdout/stderr 恒出现，
    /// exitCode/error 仅在已知时出现。
    pub fn to_json(&self) -> Value {
        let mut object = serde_json::Map::new();
        object.insert("command".into(), json!(self.command));
        object.insert("success".into(), json!(self.success));
        if let Some(exit_code) = self.exit_code {
            object.insert("exitCode".into(), json!(exit_code));
        }
        object.insert("stdout".into(), json!(self.stdout));
        object.insert("stderr".into(), json!(self.stderr));
        if let Some(error) = &self.error {
            object.insert("error".into(), json!(error));
        }
        Value::Object(object)
    }
}

/// routes.js `runCommandInDirectory`: `shell -c command` in `resolved_cwd`
/// with a hard SIGKILL deadline. Always resolves — failures travel inside the
/// outcome (`success: false` + `error`), never as exceptions.
///
/// 中文说明：在 `resolved_cwd` 中以 `shell -c command` 运行命令，
/// 超过 `timeout_ms` 即 SIGKILL 并收割退出状态。永不返回 Err：
/// spawn 失败、超时等信息一律装入 `error` 字段。输出首尾空白被裁剪。
pub async fn run_command_in_directory(
    shell: &str,
    shell_flag: &str,
    command: &str,
    resolved_cwd: &Path,
    timeout_ms: u64,
) -> CommandOutcome {
    let spawn_result = tokio::process::Command::new(shell)
        .arg(shell_flag)
        .arg(command)
        .current_dir(resolved_cwd)
        // The JS swaps PATH for buildAugmentedPath() (login-shell PATH
        // merge); the Rust port inherits the parent PATH as-is.
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn();
    let mut child = match spawn_result {
        Ok(child) => child,
        Err(error) => {
            return CommandOutcome {
                command: command.to_string(),
                success: false,
                exit_code: None,
                stdout: String::new(),
                stderr: String::new(),
                error: Some(error.to_string()),
            };
        }
    };

    let mut stdout_pipe = child.stdout.take();
    let mut stderr_pipe = child.stderr.take();

    // Deadline loop keeps the child handle alive so the timeout arm can
    // SIGKILL it and still reap the exit status (JS: kill then 'close').
    let deadline = tokio::time::Instant::now() + Duration::from_millis(timeout_ms);
    let mut timed_out = false;
    let wait_result = match tokio::time::timeout_at(deadline, child.wait()).await {
        Ok(status) => status,
        Err(_elapsed) => {
            timed_out = true;
            let _ = child.start_kill();
            child.wait().await
        }
    };

    /// 读完管道中剩余字节并以 UTF-8 宽松解码；管道缺席时返回空串。
    async fn drain(pipe: &mut Option<impl tokio::io::AsyncRead + Unpin>) -> String {
        let mut bytes = Vec::new();
        if let Some(pipe) = pipe.as_mut() {
            let _ = tokio::io::AsyncReadExt::read_to_end(pipe, &mut bytes).await;
        }
        String::from_utf8_lossy(&bytes).into_owned()
    }
    let (stdout, stderr) = futures::join!(drain(&mut stdout_pipe), drain(&mut stderr_pipe));

    let exit_code = wait_result.as_ref().ok().and_then(|status| status.code());
    let base = CommandOutcome {
        command: command.to_string(),
        success: exit_code == Some(0) && !timed_out,
        exit_code,
        stdout: stdout.trim().to_string(),
        stderr: stderr.trim().to_string(),
        error: None,
    };
    if timed_out {
        let signal = wait_result.ok().as_ref().and_then(unix_signal_name);
        return CommandOutcome {
            error: Some(format!(
                "Command timed out after {timeout_ms}ms{}",
                signal.map(|s| format!(" ({s})")).unwrap_or_default()
            )),
            ..base
        };
    }
    base
}

/// 从 `ExitStatus` 提取终止信号名（SIGKILL/SIGTERM 或 SIG+编号），
/// 正常退出返回 None；超时错误文案用它标注信号。
#[cfg(unix)]
fn unix_signal_name(status: &std::process::ExitStatus) -> Option<String> {
    use crate::os_compat::ExitStatusExt;
    let signal = status.signal()?;
    Some(match signal {
        9 => "SIGKILL".to_string(),
        15 => "SIGTERM".to_string(),
        other => format!("SIG{other}"),
    })
}

/// 非 Unix 平台无信号语义，恒返回 None。
#[cfg(not(unix))]
fn unix_signal_name(_status: &std::process::ExitStatus) -> Option<String> {
    None
}

/// git-read 缓存的单个条目：写入时刻 + 被缓存的命令结果。
struct GitReadEntry {
    /// 条目写入时刻（Unix 毫秒），TTL 判定依据。
    at: u64,
    /// 被缓存的命令结果。
    result: CommandOutcome,
}

/// git 只读命令的结果缓存：条目列表（近似 LRU）+ 在飞（in-flight）去重锁表。
pub struct GitReadCache {
    /// 条目有效期（毫秒）；0 表示整个缓存被禁用。
    ttl_ms: u64,
    /// (规范化命令键, 条目) 列表，队首最旧，命中后移到队尾。
    entries: Mutex<Vec<(String, GitReadEntry)>>,
    /// 相同命令键的并发执行去重锁表（键数达到 1024 时整体清空防泄漏）。
    in_flight: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

/// 查询、逐出与带并发去重的执行入口。
impl GitReadCache {
    /// 以给定 TTL 构造空缓存。
    pub fn new(ttl_ms: u64) -> Self {
        Self {
            ttl_ms,
            entries: Mutex::new(Vec::new()),
            in_flight: Mutex::new(HashMap::new()),
        }
    }

    /// 单条目占用字节数：键长 + stdout + stderr。
    fn entry_bytes(key: &str, result: &CommandOutcome) -> usize {
        key.len() + result.stdout.len() + result.stderr.len()
    }

    /// Insert with LRU (oldest-first) eviction enforcing the JS dual
    /// count+bytes caps.
    ///
    /// 中文说明：写入/覆盖键并施加双重上限：条目数 ≤ 500 且总字节
    /// ≤ 1 MiB，超限从最旧端逐出（字节超限时保底保留 1 条）。
    fn set_entry(&self, key: String, result: CommandOutcome) {
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        entries.retain(|(existing, _)| existing != &key);
        entries.push((
            key,
            GitReadEntry {
                at: now_ms(),
                result,
            },
        ));
        let mut total_bytes: usize = entries
            .iter()
            .map(|(k, e)| Self::entry_bytes(k, &e.result))
            .sum();
        while entries.len() > GIT_READ_CACHE_MAX_ENTRIES
            || (total_bytes > GIT_READ_CACHE_MAX_BYTES && entries.len() > 1)
        {
            let Some((oldest_key, oldest)) = entries.first() else {
                break;
            };
            let oldest_bytes = Self::entry_bytes(oldest_key, &oldest.result);
            entries.remove(0);
            total_bytes = total_bytes.saturating_sub(oldest_bytes);
        }
    }

    /// Fresh hit, refreshed to most-recently-used without altering its age.
    ///
    /// 中文说明：命中未过期条目则克隆返回并把条目移到队尾（LRU
    /// 语义，不刷新 `at` 以保持原龄期）；过期则删除并返回 None。
    fn fresh_entry(&self, key: &str) -> Option<CommandOutcome> {
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        let index = entries.iter().position(|(existing, _)| existing == key)?;
        if now_ms().saturating_sub(entries[index].1.at) >= self.ttl_ms {
            entries.remove(index);
            return None;
        }
        let (key, entry) = entries.remove(index);
        let result = entry.result;
        entries.push((
            key,
            GitReadEntry {
                at: entry.at,
                result: result.clone(),
            },
        ));
        Some(result)
    }

    /// routes.js `pruneGitReadCache`.
    ///
    /// 中文说明：移除所有过期条目；TTL 为 0（禁用）时直接返回。
    pub fn prune(&self) {
        if self.ttl_ms == 0 {
            return;
        }
        let now = now_ms();
        self.entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|(_, entry)| now.saturating_sub(entry.at) < self.ttl_ms);
    }

    /// 当前条目数（测试与诊断用）。
    pub fn len(&self) -> usize {
        self.entries.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    /// routes.js `runCommandWithGitReadCache`: serve/store allowlisted
    /// git-read results and dedupe concurrent identical runs.
    ///
    /// 中文说明：可缓存命令（TTL>0 且命中白名单）先查缓存；未命中
    /// 时经在飞锁去重并发相同命令——后到者等锁释放后直接读缓存。
    /// 仅缓存成功结果（失败可能是瞬时的）；不可缓存命令直通执行。
    pub async fn run(
        self: &Arc<Self>,
        shell: &str,
        shell_flag: &str,
        command: &str,
        resolved_cwd: &Path,
        timeout_ms: u64,
    ) -> CommandOutcome {
        let cacheable = self.ttl_ms > 0 && is_cacheable_git_read_command(command);
        let cache_key = cacheable.then(|| {
            format!(
                "{}{}",
                resolved_cwd.to_string_lossy(),
                normalize_command(command)
            )
        });

        let Some(cache_key) = cache_key else {
            return run_command_in_directory(shell, shell_flag, command, resolved_cwd, timeout_ms)
                .await;
        };

        if let Some(cached) = self.fresh_entry(&cache_key) {
            return CommandOutcome {
                command: command.to_string(),
                ..cached
            };
        }

        // In-flight dedupe: the first caller runs, the rest wait on the same
        // key lock and then read the successful result from the cache.
        let lock = {
            let mut in_flight = self.in_flight.lock().unwrap_or_else(|e| e.into_inner());
            if in_flight.len() > 1024 {
                in_flight.clear();
            }
            Arc::clone(in_flight.entry(cache_key.clone()).or_default())
        };
        let _guard = lock.lock().await;

        if let Some(cached) = self.fresh_entry(&cache_key) {
            return CommandOutcome {
                command: command.to_string(),
                ..cached
            };
        }

        let result =
            run_command_in_directory(shell, shell_flag, command, resolved_cwd, timeout_ms).await;
        // Only cache successful results — failures may be transient.
        if result.success {
            self.set_entry(cache_key, result.clone());
        }
        result
    }
}

/// 一次 `/api/fs/exec` 请求对应的任务记录，供轮询接口读取进度。
#[derive(Debug, Clone)]
pub struct ExecJob {
    /// 任务标识（randomUUID 形状），即轮询 URL 中的 `:jobId`。
    pub job_id: String,
    /// 生命周期状态："running" 或 "done"（与 JS 字符串字面量一致）。
    pub status: &'static str,
    /// 全部命令都成功才为 Some(true)；运行中为 None。
    pub success: Option<bool>,
    /// 命令执行目录（工作区解析后的绝对路径）。
    pub resolved_cwd: std::path::PathBuf,
    /// 每条命令的 JSON 结果数组（逐条更新，支持观察中间进度）。
    pub results: Vec<Value>,
    /// 任务创建时刻（Unix 毫秒）。
    pub started_at: u64,
    /// 任务完成时刻；未完成为 None。
    pub finished_at: Option<u64>,
    /// 最近一次更新时刻，TTL 清扫以此为准。
    pub updated_at: u64,
}

/// exec 任务表：jobId → 共享可变任务（`Arc<Mutex<ExecJob>>`），
/// 供执行协程与 HTTP 轮询并发访问。
#[derive(Default)]
pub struct ExecJobStore {
    /// 任务映射；锁中毒以 `into_inner` 恢复，任务表永不 panic。
    jobs: Mutex<HashMap<String, Arc<Mutex<ExecJob>>>>,
}

/// 任务生命周期管理：TTL 清扫、插入、查询与内联执行。
impl ExecJobStore {
    /// routes.js `pruneExecJobs`.
    ///
    /// 中文说明：删除 `updated_at` 距今超过 TTL 的任务；`updated_at`
    /// 为 0 的任务视为永久保留（与 JS 判定一致）。
    pub fn prune(&self) {
        let mut jobs = self.jobs.lock().unwrap_or_else(|e| e.into_inner());
        let now = now_ms();
        jobs.retain(|_, job| {
            let updated_at = job.lock().unwrap_or_else(|e| e.into_inner()).updated_at;
            updated_at == 0 || now.saturating_sub(updated_at) <= EXEC_JOB_TTL_MS
        });
    }

    /// 存入任务并返回其共享句柄（执行协程持有它逐条更新结果）。
    pub fn insert(&self, job: ExecJob) -> Arc<Mutex<ExecJob>> {
        let shared = Arc::new(Mutex::new(job));
        let job_id = shared
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .job_id
            .clone();
        self.jobs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(job_id, Arc::clone(&shared));
        shared
    }

    /// 按 jobId 取共享句柄；不存在时返回 None。
    pub fn get(&self, job_id: &str) -> Option<Arc<Mutex<ExecJob>>> {
        self.jobs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(job_id)
            .cloned()
    }

    /// routes.js `runExecJob` for the inline (non-background) path. Updates
    /// the stored job after every command so a concurrent
    /// `GET /api/fs/exec/:jobId` observes progress.
    ///
    /// 中文说明：内联（非 background）执行路径：逐条运行命令并把
    /// 已产出的结果即时写回任务，使并发的
    /// `GET /api/fs/exec/:jobId` 能观察到进度；全部命令的
    /// `success` 均为 true 时任务整体才算成功。
    pub async fn run_inline(
        &self,
        job: Arc<Mutex<ExecJob>>,
        commands: &[Value],
        shell: &str,
        shell_flag: &str,
        cache: &Arc<GitReadCache>,
        timeout_ms: u64,
    ) -> (bool, Vec<Value>) {
        {
            let mut guard = job.lock().unwrap_or_else(|e| e.into_inner());
            guard.status = "running";
            guard.updated_at = now_ms();
        }

        let resolved_cwd = job
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .resolved_cwd
            .clone();
        let mut results: Vec<Value> = Vec::new();
        for command in commands {
            let invalid = match command {
                Value::String(text) => text.trim().is_empty(),
                _ => true,
            };
            if invalid {
                results.push(CommandOutcome::invalid_command_json(command));
            } else {
                let command_text = command.as_str().unwrap_or_default().to_string();
                let outcome = cache
                    .run(shell, shell_flag, &command_text, &resolved_cwd, timeout_ms)
                    .await;
                results.push(outcome.to_json());
            }
            let mut guard = job.lock().unwrap_or_else(|e| e.into_inner());
            guard.results = results.clone();
            guard.updated_at = now_ms();
        }

        let success = results.iter().all(|result| {
            result
                .get("success")
                .and_then(Value::as_bool)
                .unwrap_or(false)
        });
        {
            let mut guard = job.lock().unwrap_or_else(|e| e.into_inner());
            guard.results = results.clone();
            guard.success = Some(success);
            guard.status = "done";
            guard.finished_at = Some(now_ms());
            guard.updated_at = now_ms();
        }
        (success, results)
    }
}

/// Shell resolution: `process.env.SHELL` (win fallback `cmd.exe`), flag `-c`
/// (`/c` on Windows).
///
/// 中文说明：读取 `SHELL` 环境变量（Windows 回落 cmd.exe），返回
/// (shell 路径, 命令标志)；POSIX 用 `-c`，Windows 用 `/c`。
pub fn resolve_shell() -> (String, &'static str) {
    if cfg!(windows) {
        (
            std::env::var("SHELL").unwrap_or_else(|_| "cmd.exe".to_string()),
            "/c",
        )
    } else {
        (
            std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string()),
            "-c",
        )
    }
}

/// 生成新的 exec 任务 id（randomUUID 形状，见 `paths::random_uuid`）。
pub fn new_job_id() -> String {
    random_uuid()
}

/// exec 子系统的单元与异步行为测试。
#[cfg(test)]
mod tests {
    use super::*;

    /// 验证环境变量解析镜像 JS `Number()` 守卫：非法/非正值回落默认。
    #[test]
    fn env_parsers_mirror_js_gates() {
        assert_eq!(command_timeout_ms_from_raw(Some("1500")), 1500);
        assert_eq!(command_timeout_ms_from_raw(Some("0")), 300_000);
        assert_eq!(command_timeout_ms_from_raw(Some("-5")), 300_000);
        assert_eq!(command_timeout_ms_from_raw(Some("abc")), 300_000);
        assert_eq!(command_timeout_ms_from_raw(None), 300_000);

        assert_eq!(git_read_cache_ttl_ms_from_raw(Some("0")), 0);
        assert_eq!(git_read_cache_ttl_ms_from_raw(Some("x")), 30_000);

        assert_eq!(upload_max_bytes_from_raw(Some("5")), 5);
        assert_eq!(upload_max_bytes_from_raw(Some("5.9")), 5);
        assert_eq!(upload_max_bytes_from_raw(Some("0")), 100 * 1024 * 1024);
        assert_eq!(upload_max_bytes_from_raw(None), 100 * 1024 * 1024);
    }

    /// 验证可缓存判定与 JS 正则等价：白名单标志、token 数上限、
    /// 拒绝组合命令与重复标志。
    #[test]
    fn cacheable_git_read_allowlist_matches_js_regex() {
        assert!(is_cacheable_git_read_command(
            "git rev-parse --absolute-git-dir"
        ));
        assert!(is_cacheable_git_read_command(
            "git rev-parse --absolute-git-dir --git-common-dir --show-toplevel"
        ));
        assert!(is_cacheable_git_read_command(
            "git   rev-parse   --show-toplevel"
        ));
        assert!(!is_cacheable_git_read_command("git rev-parse"));
        assert!(!is_cacheable_git_read_command("git rev-parse --git-dir"));
        assert!(!is_cacheable_git_read_command("git status"));
        assert!(!is_cacheable_git_read_command(
            "git rev-parse --absolute-git-dir && rm -rf /"
        ));
        assert!(!is_cacheable_git_read_command(
            "git rev-parse --absolute-git-dir --git-common-dir --show-toplevel --absolute-git-dir"
        ));
    }

    /// 验证结果 JSON 的字段省略规则：exitCode/error 仅在已知时出现。
    #[test]
    fn outcome_json_omits_unknown_fields() {
        let full = CommandOutcome {
            command: "id".into(),
            success: true,
            exit_code: Some(0),
            stdout: "0".into(),
            stderr: String::new(),
            error: None,
        };
        assert_eq!(
            full.to_json(),
            json!({"command": "id", "success": true, "exitCode": 0, "stdout": "0", "stderr": ""})
        );

        let spawn_error = CommandOutcome {
            command: "id".into(),
            success: false,
            exit_code: None,
            stdout: String::new(),
            stderr: String::new(),
            error: Some("spawn failed".into()),
        };
        assert_eq!(
            spawn_error.to_json(),
            json!({"command": "id", "success": false, "stdout": "", "stderr": "", "error": "spawn failed"})
        );

        assert_eq!(
            CommandOutcome::invalid_command_json(&json!(42)),
            json!({"command": 42, "success": false, "error": "Invalid command"})
        );
    }

    /// 验证真实命令执行捕获 stdout/stderr 且首尾空白被裁剪。
    #[tokio::test]
    async fn runs_a_command_and_captures_trimmed_output() {
        let (shell, flag, script) = if cfg!(windows) {
            ("cmd", "/C", "echo   out  & echo   err  1>&2")
        } else {
            (
                "/bin/sh",
                "-c",
                "printf '  out  ' ; printf ' err ' >&2 ; exit 0",
            )
        };
        let outcome =
            run_command_in_directory(shell, flag, script, &std::env::temp_dir(), 10_000).await;
        assert!(outcome.success, "{:?}", outcome.error);
        assert_eq!(outcome.exit_code, Some(0));
        assert_eq!(outcome.stdout, "out");
        assert_eq!(outcome.stderr, "err");
        assert!(outcome.error.is_none());
    }

    /// 验证 shell 不存在时不抛错，而是返回装入 `error` 的失败结果。
    #[tokio::test]
    async fn failed_spawn_resolves_with_error() {
        let outcome = run_command_in_directory(
            "/nonexistent-shell-fsport",
            "-c",
            "true",
            &std::env::temp_dir(),
            1_000,
        )
        .await;
        assert!(!outcome.success);
        assert!(outcome.error.is_some());
    }

    /// 验证超时触发 SIGKILL 并返回 "Command timed out" 错误。
    #[tokio::test]
    async fn timeout_kills_and_reports() {
        let (shell, flag, script) = if cfg!(windows) {
            ("cmd", "/C", "ping -n 5 127.0.0.1 > NUL")
        } else {
            ("/bin/sh", "-c", "sleep 5")
        };
        let outcome =
            run_command_in_directory(shell, flag, script, &std::env::temp_dir(), 150).await;
        assert!(!outcome.success);
        let error = outcome.error.expect("timeout error");
        assert!(
            error.starts_with("Command timed out after 150ms"),
            "{error}"
        );
    }

    /// 构造统一的成功样例结果，供缓存相关测试复用。
    fn sample_outcome() -> CommandOutcome {
        CommandOutcome {
            command: "git rev-parse --absolute-git-dir".into(),
            success: true,
            exit_code: Some(0),
            stdout: "/repo/.git".into(),
            stderr: String::new(),
            error: None,
        }
    }

    /// 验证缓存按键命中、异键未命中的行为。
    #[tokio::test]
    async fn git_read_cache_serves_and_misses_by_key() {
        let cache = GitReadCache::new(60_000);
        cache.set_entry("/repo".into(), sample_outcome());
        let cached = cache.fresh_entry("/repo").expect("fresh entry");
        assert_eq!(cached.stdout, "/repo/.git");
        assert!(cache.fresh_entry("/other").is_none());
    }

    /// 验证条目数上限触发最旧逐出且缓存容量保持不变。
    #[tokio::test]
    async fn git_read_cache_evicts_over_the_entry_cap() {
        let cache = GitReadCache::new(60_000);
        for index in 0..GIT_READ_CACHE_MAX_ENTRIES {
            cache.set_entry(format!("/repo/{index}"), sample_outcome());
        }
        assert_eq!(cache.len(), GIT_READ_CACHE_MAX_ENTRIES);
        cache.set_entry("/repo/overflow".into(), sample_outcome());
        assert_eq!(cache.len(), GIT_READ_CACHE_MAX_ENTRIES);
        assert!(
            cache.fresh_entry("/repo/0").is_none(),
            "oldest entry evicted"
        );
        assert!(cache.fresh_entry("/repo/overflow").is_some());
    }

    /// 验证任务按 `updated_at` 的 TTL 清扫：过期删除、新鲜保留。
    #[tokio::test]
    async fn exec_jobs_prune_by_ttl() {
        let store = ExecJobStore::default();
        store.insert(ExecJob {
            job_id: "job-old".into(),
            status: "done",
            success: Some(true),
            resolved_cwd: Path::new("/repo").to_path_buf(),
            results: vec![],
            started_at: 0,
            finished_at: None,
            updated_at: now_ms() - EXEC_JOB_TTL_MS - 5_000,
        });
        store.insert(ExecJob {
            job_id: "job-fresh".into(),
            status: "done",
            success: Some(true),
            resolved_cwd: Path::new("/repo").to_path_buf(),
            results: vec![],
            started_at: 0,
            finished_at: None,
            updated_at: now_ms(),
        });
        store.prune();
        assert!(store.get("job-old").is_none());
        assert!(store.get("job-fresh").is_some());
    }
}
