//! Engine dispatch for scheduled task runs.
//!
//! `runTaskWithWatchdog` in runtime.js drives the engine through
//! `createLocalEngineClient` (`POST /session`, `GET /command`,
//! `POST /session/:id/command`), `fetch` (`prompt_async`), the session-goal
//! creator (objective file + `PATCH /session/:id`) and the permission
//! auto-accept runtime. Those calls are behind the [`EngineDispatch`] seam so
//! tests can double the engine (like the JS issue-2710 suite mocks the SDK);
//! [`HttpEngineDispatch`] is the honest engine-backed implementation.
//!
//! Known gap: long objectives are trimmed to the 5 000-char limit instead of
//! being distilled by the small-model service (module not ported yet).
//!
//! 计划任务运行期与 OpenCode 引擎之间的调度边界：JS 版 runTaskWithWatchdog
//! 依赖的引擎调用（建会话、列命令、执行命令、prompt_async、session goal、
//! permission 自动放行）在这里统一收敛为 [`EngineDispatch`] trait，
//! 使 runtime/service 可用假实现测试；[`HttpEngineDispatch`] 是真实实现。
//! 已知缺口：超长 objective 以截断适配而非小模型提炼（模块未移植）。

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};

use crate::engine::EngineState;
use crate::projects::Execution;

/// goal objective 文本的最大字符数；超限部分被截断而非拒收。
pub const GOAL_OBJECTIVE_CHAR_LIMIT: usize = 5_000;
/// objective 中段截断时插入的说明文本，告知审计方完整内容已随聊天消息送达。
const TRIM_MARKER: &str = "\n\n[… objective trimmed for the auditor — the full prompt was delivered in the chat message …]\n\n";

/// 装箱 future 别名：trait 方法需要返回固定大小、可 Send 的异步值。
pub type BoxFut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// A resolved slash command (name + raw arguments + engine template).
/// 已解析的斜杠命令（名称 + 原始参数 + 引擎模板）。
#[derive(Debug, Clone)]
pub struct ScheduledCommand {
    /// 命令名（不含前导斜杠）。
    pub command: String,
    /// 传给命令的原始参数串（可为空）。
    pub arguments: String,
    /// 引擎提供的模板文本；引擎未列出模板时为 None。
    pub template: Option<String>,
}

/// Engine operations used by one scheduled run (JS: local-engine-client +
/// fetch + createSessionGoal + setSessionAutoAccept + waitForOpenCodeReady).
/// 单次计划任务运行所需的全部引擎操作集合；真实副作用只经此 trait
/// 发生，测试以假实现（如 testing::RecordingDispatch）替换。
pub trait EngineDispatch: Send + Sync {
    /// 等待引擎就绪（waitForOpenCodeReady）；超时或失败返回 Err。
    fn wait_ready(&self) -> BoxFut<'_, Result<(), String>>;
    /// 在 directory 下创建标题为 title 的新会话，返回会话 id。
    fn create_session(&self, directory: &str, title: &str) -> BoxFut<'_, Result<String, String>>;
    /// Commands carrying a `template` (empty when the engine lists none).
    /// 列出引擎可用斜杠命令；带 template 的命令可按模板执行，
    /// 引擎未列出时返回空表。
    fn list_commands(&self, directory: &str) -> BoxFut<'_, Result<Vec<ScheduledCommand>, String>>;
    /// 在既有会话上执行一条斜杠命令（POST /session/:id/command）；
    /// 按 execution 附带 agent/model/variant。
    fn run_session_command(
        &self,
        session_id: &str,
        directory: &str,
        command: &ScheduledCommand,
        execution: &Execution,
    ) -> BoxFut<'_, Result<(), String>>;
    /// 异步投递一条 prompt（prompt_async），payload 为完整请求 JSON；
    /// 只等送达，不等生成完成。
    fn prompt_async(
        &self,
        session_id: &str,
        directory: &str,
        payload: &Value,
    ) -> BoxFut<'_, Result<(), String>>;
    /// 为会话创建 goal：objective 落盘到 goals 目录并 PATCH 会话
    /// metadata；token 预算与 provider/model 可选。
    fn create_session_goal(
        &self,
        session_id: &str,
        directory: &str,
        objective: &str,
        token_budget: Option<u64>,
        provider_id: Option<&str>,
        model_id: Option<&str>,
    ) -> BoxFut<'_, Result<(), String>>;
    /// 开/关会话的 permission 自动放行策略；模块未配置时静默成功。
    fn set_session_auto_accept(
        &self,
        session_id: &str,
        enabled: bool,
        directory: &str,
    ) -> BoxFut<'_, Result<(), String>>;
}

/// Engine-backed implementation. `goals_dir` is `<data_dir>/goals`
/// (session-goal/objectives.js `goalsDir`).
/// 经 EngineState 直连引擎的 HTTP 实现；goals_dir 用于 objective 文件落盘。
pub struct HttpEngineDispatch {
    /// 引擎连接状态：base URL、bearer token 与共享 HTTP 客户端来源。
    engine: Arc<EngineState>,
    /// objective 文件目录（<data_dir>/goals，对应 JS goalsDir）。
    goals_dir: PathBuf,
    /// permission 自动放行模块句柄；None 表示该部署未启用。
    auto_accept: Option<Arc<crate::permission_auto_accept::PermissionAutoAccept>>,
}

/// 构造辅助与共享的 HTTP 请求底座。
impl HttpEngineDispatch {
    /// 以引擎状态、goals 目录与可选自动放行模块构造分发器。
    pub fn new(
        engine: Arc<EngineState>,
        goals_dir: PathBuf,
        auto_accept: Option<Arc<crate::permission_auto_accept::PermissionAutoAccept>>,
    ) -> Self {
        Self {
            engine,
            goals_dir,
            auto_accept,
        }
    }

    /// 返回去尾斜杠的引擎 base URL；引擎不可用（空 URL）时报错。
    fn base(&self) -> Result<String, String> {
        let base = self.engine.base_url().unwrap_or_default();
        let trimmed = base.trim_end_matches('/');
        if trimmed.is_empty() {
            return Err("engine unavailable".to_string());
        }
        Ok(trimmed.to_string())
    }

    /// 引擎的 authorization 头（bearer token）；未配置时为 None。
    fn auth_header(&self) -> Option<String> {
        self.engine.auth_header()
    }

    /// 统一 JSON 请求：附加可选 auth 与 body，返回
    /// (是否 2xx/3xx, 状态码, 响应文本)；网络错误映射为 Err(消息)。
    async fn request_json(
        &self,
        method: reqwest::Method,
        url: String,
        body: Option<Value>,
    ) -> Result<(bool, u16, String), String> {
        let mut request = self.engine.http().request(method, url);
        if let Some(auth) = self.auth_header() {
            request = request.header("authorization", auth);
        }
        if let Some(payload) = body {
            request = request
                .header("content-type", "application/json")
                .header("accept", "application/json")
                .json(&payload);
        }
        let response = request.send().await.map_err(|e| e.to_string())?;
        let status = response.status().as_u16();
        let text = response.text().await.unwrap_or_default();
        Ok((status < 400, status, text))
    }
}

/// 按 RFC 3986 unreserved 集合做路径段百分号编码，其余字节转大写 %XX。
fn encode_path_segment(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        let c = byte as char;
        if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '~') {
            out.push(c);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// 构造 directory=... 的 form-urlencoded 查询串。
fn query(directory: &str) -> String {
    let mut serializer = url::form_urlencoded::Serializer::new(String::new());
    serializer.append_pair("directory", directory);
    serializer.finish()
}

/// EngineDispatch 的 HTTP 实现：每个方法把参数克隆进 async 块后请求引擎。
impl EngineDispatch for HttpEngineDispatch {
    /// 委托 EngineState::wait_ready，最多等待 10 秒。
    fn wait_ready(&self) -> BoxFut<'_, Result<(), String>> {
        let engine = Arc::clone(&self.engine);
        Box::pin(async move {
            engine
                .wait_ready(Duration::from_secs(10))
                .await
                .map_err(|e| e.to_string())
        })
    }

    /// POST /session 创建会话；从 data.id（或顶层 id）取会话 id，
    /// 缺失时报 "failed to create session"（对齐 JS）。
    fn create_session(&self, directory: &str, title: &str) -> BoxFut<'_, Result<String, String>> {
        let directory = directory.to_string();
        let title = title.to_string();
        Box::pin(async move {
            let url = match self.base() {
                Ok(base) => format!("{base}/session"),
                Err(error) => return Err(error),
            };
            let body = json!({ "directory": directory, "title": title });
            let (ok, _status, text) = self
                .request_json(reqwest::Method::POST, url, Some(body))
                .await?;
            // JS: a missing `data.id` maps to 'failed to create session'.
            if ok && let Ok(payload) = serde_json::from_str::<Value>(&text) {
                let id = payload
                    .get("data")
                    .and_then(|d| d.get("id"))
                    .or_else(|| payload.get("id"))
                    .and_then(Value::as_str);
                if let Some(id) = id {
                    return Ok(id.to_string());
                }
            }
            Err("failed to create session".to_string())
        })
    }

    /// GET /command 列出命令并映射为 name+template；
    /// 非 2xx 或非法 JSON 返回 Err。
    fn list_commands(&self, directory: &str) -> BoxFut<'_, Result<Vec<ScheduledCommand>, String>> {
        let directory = directory.to_string();
        Box::pin(async move {
            let base = self.base()?;
            let url = format!("{base}/command?{}", query(&directory));
            let (ok, _status, text) = self.request_json(reqwest::Method::GET, url, None).await?;
            if !ok {
                return Err(format!("GET /command failed: {text}"));
            }
            let payload: Value = serde_json::from_str(&text)
                .map_err(|e| format!("invalid /command response: {e}"))?;
            let empty = Vec::new();
            let items = payload
                .get("data")
                .and_then(Value::as_array)
                .unwrap_or(&empty);
            let commands = items
                .iter()
                .filter_map(|item| {
                    let name = item.get("name")?.as_str()?.to_string();
                    let template = item
                        .get("template")
                        .and_then(Value::as_str)
                        .map(String::from);
                    Some(ScheduledCommand {
                        command: name,
                        arguments: String::new(),
                        template,
                    })
                })
                .collect();
            Ok(commands)
        })
    }

    /// POST /session/:id/command 执行命令；仅传输失败上抛，
    /// 业务错误信封按 JS 行为忽略。
    fn run_session_command(
        &self,
        session_id: &str,
        directory: &str,
        command: &ScheduledCommand,
        execution: &Execution,
    ) -> BoxFut<'_, Result<(), String>> {
        let session_id = session_id.to_string();
        let directory = directory.to_string();
        let command = command.clone();
        let execution = execution.clone();
        Box::pin(async move {
            let base = self.base()?;
            let url = format!(
                "{base}/session/{}/command",
                encode_path_segment(&session_id)
            );
            let mut body = json!({
                "command": command.command,
                "arguments": command.arguments,
                "directory": directory,
            });
            if let Some(agent) = &execution.agent {
                body["agent"] = json!(agent);
            }
            if let (Some(provider), Some(model)) = (&execution.provider_id, &execution.model_id) {
                body["model"] = json!(format!("{provider}/{model}"));
            }
            if let Some(variant) = &execution.variant {
                body["variant"] = json!(variant);
            }
            // JS ignores the session.command { data, error } envelope — only
            // transport failures surface here.
            let _ = self
                .request_json(reqwest::Method::POST, url, Some(body))
                .await?;
            Ok(())
        })
    }

    /// POST /session/:id/prompt_async 异步投递 prompt；
    /// 非 2xx 时带状态码与响应体报错。
    fn prompt_async(
        &self,
        session_id: &str,
        directory: &str,
        payload: &Value,
    ) -> BoxFut<'_, Result<(), String>> {
        let session_id = session_id.to_string();
        let directory = directory.to_string();
        let payload = payload.clone();
        Box::pin(async move {
            let base = self.base()?;
            let url = format!(
                "{base}/session/{}/prompt_async?{}",
                encode_path_segment(&session_id),
                query(&directory)
            );
            let (ok, status, text) = self
                .request_json(reqwest::Method::POST, url, Some(payload.clone()))
                .await?;
            if !ok {
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

    /// 落盘 objective 文件并 PATCH metadata.ompchamber.goal：
    /// objective 为空报错；文件写失败回退为内联文本。
    fn create_session_goal(
        &self,
        session_id: &str,
        directory: &str,
        objective: &str,
        token_budget: Option<u64>,
        provider_id: Option<&str>,
        model_id: Option<&str>,
    ) -> BoxFut<'_, Result<(), String>> {
        let session_id = session_id.to_string();
        let directory = directory.to_string();
        let objective = objective.to_string();
        let provider_id = provider_id.map(String::from);
        let model_id = model_id.map(String::from);
        let _ = (provider_id, model_id);
        Box::pin(async move {
            let base = self.base()?;
            let url = format!(
                "{base}/session/{}?{}",
                encode_path_segment(&session_id),
                query(&directory)
            );

            let objective_text = fit_objective(objective.trim());
            let Some(objective_text) = objective_text.filter(|text| !text.is_empty()) else {
                return Err("goal objective is required".to_string());
            };

            // Write the objective file (session metadata stays light); a
            // write failure falls back to the inline objective.
            let mut objective_file = false;
            if session_id_is_valid(&session_id) {
                let path = self.goals_dir.join(format!("{session_id}.md"));
                match std::fs::create_dir_all(&self.goals_dir)
                    .and_then(|_| std::fs::write(&path, &objective_text))
                {
                    Ok(()) => objective_file = true,
                    Err(error) => {
                        tracing::warn!(
                            "[scheduled-tasks] goal objective file write failed, falling back to inline: {error}"
                        );
                    }
                }
            }

            let now = crate_time_ms();
            let goal = json!({
                "id": format!("{}{}", base36(now), random_base36(6)),
                "objective": if objective_file {
                    String::new()
                } else {
                    objective_text.chars().take(GOAL_OBJECTIVE_CHAR_LIMIT).collect::<String>()
                },
                "objectiveFile": objective_file,
                "status": "active",
                "tokenBudget": token_budget.map(Value::from).unwrap_or(Value::Null),
                "tokensUsed": 0,
                "turnsUsed": 0,
                "blockedStreak": 0,
                "note": "",
                "statusReason": "",
                "lastAccountedMessageID": "",
                "createdAt": now,
                "updatedAt": now,
            });
            let body = json!({ "metadata": { "ompchamber": { "goal": goal } } });
            let (ok, status, _text) = self
                .request_json(reqwest::Method::PATCH, url, Some(body))
                .await?;
            if !ok {
                return Err(format!("goal metadata patch failed ({status})"));
            }
            Ok(())
        })
    }

    /// 调 permission 自动放行模块设置会话策略；模块缺失时直接成功。
    fn set_session_auto_accept(
        &self,
        session_id: &str,
        enabled: bool,
        directory: &str,
    ) -> BoxFut<'_, Result<(), String>> {
        let session_id = session_id.to_string();
        let directory = directory.to_string();
        Box::pin(async move {
            let Some(auto_accept) = &self.auto_accept else {
                return Ok(());
            };
            auto_accept
                .set_session_policy(&session_id, Some(enabled), Some(directory.as_str()))
                .await
                .map(|_| ())
                .map_err(|e| e.to_string())
        })
    }
}

/// `fitObjective` without the small-model distiller (module not ported):
/// short objectives pass through, long ones are middle-trimmed.
/// objective 适配（无小模型提炼版）：不超上限原样通过；超限时保留
/// 首尾各半、中段以 TRIM_MARKER 替代，总长压回上限内。
fn fit_objective(objective: &str) -> Option<String> {
    if objective.chars().count() <= GOAL_OBJECTIVE_CHAR_LIMIT {
        return Some(objective.to_string());
    }
    let marker_len = TRIM_MARKER.chars().count();
    let half = GOAL_OBJECTIVE_CHAR_LIMIT.saturating_sub(marker_len) / 2;
    let chars: Vec<char> = objective.chars().collect();
    let head: String = chars[..half.min(chars.len())].iter().collect();
    let tail: String = chars[chars.len().saturating_sub(half)..].iter().collect();
    Some(format!("{head}{TRIM_MARKER}{tail}"))
}

/// 当前 Unix 毫秒时间戳；系统时钟早于 epoch 时退化为 0。
fn crate_time_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// u64 转 base36 字符串（小写），与 JS Number.toString(36) 对齐。
fn base36(mut value: u64) -> String {
    // base36 字母表（0-9a-z）。
    const ALPHABET: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut out = Vec::new();
    loop {
        out.push(ALPHABET[(value % 36) as usize]);
        value /= 36;
        if value == 0 {
            break;
        }
    }
    out.reverse();
    String::from_utf8(out).unwrap_or_default()
}

/// 生成 count 位随机 base36 字符，用作 goal id 的随机后缀。
fn random_base36(count: usize) -> String {
    use rand::Rng;
    // base36 字母表（0-9a-z）。
    const ALPHABET: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut rng = rand::rng();
    (0..count)
        .map(|_| ALPHABET[rng.random_range(0..36)] as char)
        .collect()
}

/// OpenCode session ids are URL-safe tokens; anything else is rejected before
/// touching the filesystem (`isValidObjectiveKey`).
/// 会话 id 白名单校验（4..=128 位字母数字/_/-）；在触碰文件系统前
/// 拒绝路径穿越等非法 id。
fn session_id_is_valid(id: &str) -> bool {
    let len = id.chars().count();
    (4..=128).contains(&len)
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// dispatch 纯函数（objective 适配、id 校验、URL 编码、base36）的测试。
#[cfg(test)]
mod tests {
    use super::*;

    /// 验证短 objective 原样通过、超长 objective 被中段截断并压回上限。
    #[test]
    fn fit_objective_passes_short_and_trims_long() {
        assert_eq!(fit_objective("short").as_deref(), Some("short"));
        let long = "x".repeat(6_000);
        let fitted = fit_objective(&long).expect("fitted");
        assert!(fitted.chars().count() <= GOAL_OBJECTIVE_CHAR_LIMIT);
        assert!(fitted.contains("objective trimmed"));
    }

    /// 验证会话 id 校验放行合法 token、拒绝过短与含穿越/空白的输入。
    #[test]
    fn session_id_pattern_gates_file_paths() {
        assert!(session_id_is_valid("ses_1234"));
        assert!(session_id_is_valid("A".repeat(128).as_str()));
        assert!(!session_id_is_valid("ab"));
        assert!(!session_id_is_valid("../etc/passwd"));
        assert!(!session_id_is_valid("has space"));
    }

    /// 验证路径段百分号编码与 directory 查询串的编码形状。
    #[test]
    fn encodes_segments_and_queries() {
        assert_eq!(encode_path_segment("ses_1"), "ses_1");
        assert_eq!(encode_path_segment("a/b c"), "a%2Fb%20c");
        assert_eq!(query("/repo x"), "directory=%2Frepo+x");
    }

    /// 验证 base36 输出与 JS toString(36) 基准值一致。
    #[test]
    fn base36_matches_js_tostring36() {
        assert_eq!(base36(0), "0");
        assert_eq!(base36(35), "z");
        assert_eq!(base36(123456789), "21i3v9");
    }
}
