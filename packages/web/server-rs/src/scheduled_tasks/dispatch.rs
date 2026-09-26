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

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};

use crate::engine::EngineState;
use crate::projects::Execution;

pub const GOAL_OBJECTIVE_CHAR_LIMIT: usize = 5_000;
const TRIM_MARKER: &str = "\n\n[… objective trimmed for the auditor — the full prompt was delivered in the chat message …]\n\n";

pub type BoxFut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// A resolved slash command (name + raw arguments + engine template).
#[derive(Debug, Clone)]
pub struct ScheduledCommand {
    pub command: String,
    pub arguments: String,
    pub template: Option<String>,
}

/// Engine operations used by one scheduled run (JS: local-engine-client +
/// fetch + createSessionGoal + setSessionAutoAccept + waitForOpenCodeReady).
pub trait EngineDispatch: Send + Sync {
    fn wait_ready(&self) -> BoxFut<'_, Result<(), String>>;
    fn create_session(&self, directory: &str, title: &str) -> BoxFut<'_, Result<String, String>>;
    /// Commands carrying a `template` (empty when the engine lists none).
    fn list_commands(&self, directory: &str) -> BoxFut<'_, Result<Vec<ScheduledCommand>, String>>;
    fn run_session_command(
        &self,
        session_id: &str,
        directory: &str,
        command: &ScheduledCommand,
        execution: &Execution,
    ) -> BoxFut<'_, Result<(), String>>;
    fn prompt_async(
        &self,
        session_id: &str,
        directory: &str,
        payload: &Value,
    ) -> BoxFut<'_, Result<(), String>>;
    fn create_session_goal(
        &self,
        session_id: &str,
        directory: &str,
        objective: &str,
        token_budget: Option<u64>,
        provider_id: Option<&str>,
        model_id: Option<&str>,
    ) -> BoxFut<'_, Result<(), String>>;
    fn set_session_auto_accept(
        &self,
        session_id: &str,
        enabled: bool,
        directory: &str,
    ) -> BoxFut<'_, Result<(), String>>;
}

/// Engine-backed implementation. `goals_dir` is `<data_dir>/goals`
/// (session-goal/objectives.js `goalsDir`).
pub struct HttpEngineDispatch {
    engine: Arc<EngineState>,
    goals_dir: PathBuf,
    auto_accept: Option<Arc<crate::permission_auto_accept::PermissionAutoAccept>>,
}

impl HttpEngineDispatch {
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

    fn base(&self) -> Result<String, String> {
        let base = self.engine.base_url().unwrap_or_default();
        let trimmed = base.trim_end_matches('/');
        if trimmed.is_empty() {
            return Err("engine unavailable".to_string());
        }
        Ok(trimmed.to_string())
    }

    fn auth_header(&self) -> Option<String> {
        self.engine.auth_header()
    }

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

fn query(directory: &str) -> String {
    let mut serializer = url::form_urlencoded::Serializer::new(String::new());
    serializer.append_pair("directory", directory);
    serializer.finish()
}

impl EngineDispatch for HttpEngineDispatch {
    fn wait_ready(&self) -> BoxFut<'_, Result<(), String>> {
        let engine = Arc::clone(&self.engine);
        Box::pin(async move {
            engine
                .wait_ready(Duration::from_secs(10))
                .await
                .map_err(|e| e.to_string())
        })
    }

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

fn crate_time_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn base36(mut value: u64) -> String {
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

fn random_base36(count: usize) -> String {
    use rand::Rng;
    const ALPHABET: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut rng = rand::rng();
    (0..count)
        .map(|_| ALPHABET[rng.random_range(0..36)] as char)
        .collect()
}

/// OpenCode session ids are URL-safe tokens; anything else is rejected before
/// touching the filesystem (`isValidObjectiveKey`).
fn session_id_is_valid(id: &str) -> bool {
    let len = id.chars().count();
    (4..=128).contains(&len)
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_objective_passes_short_and_trims_long() {
        assert_eq!(fit_objective("short").as_deref(), Some("short"));
        let long = "x".repeat(6_000);
        let fitted = fit_objective(&long).expect("fitted");
        assert!(fitted.chars().count() <= GOAL_OBJECTIVE_CHAR_LIMIT);
        assert!(fitted.contains("objective trimmed"));
    }

    #[test]
    fn session_id_pattern_gates_file_paths() {
        assert!(session_id_is_valid("ses_1234"));
        assert!(session_id_is_valid("A".repeat(128).as_str()));
        assert!(!session_id_is_valid("ab"));
        assert!(!session_id_is_valid("../etc/passwd"));
        assert!(!session_id_is_valid("has space"));
    }

    #[test]
    fn encodes_segments_and_queries() {
        assert_eq!(encode_path_segment("ses_1"), "ses_1");
        assert_eq!(encode_path_segment("a/b c"), "a%2Fb%20c");
        assert_eq!(query("/repo x"), "directory=%2Frepo+x");
    }

    #[test]
    fn base36_matches_js_tostring36() {
        assert_eq!(base36(0), "0");
        assert_eq!(base36(35), "z");
        assert_eq!(base36(123456789), "21i3v9");
    }
}
