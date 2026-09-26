//! Port of `server/lib/session-goal/create.js`.
//!
//! Server-side goal creation shared by scheduled tasks and the OMPChamber
//! session-orchestration routes: fit the objective to the metadata size
//! budget (distill via the small model when available, else head/tail trim),
//! write the objective FILE first (inline metadata fallback on failure), then
//! PATCH active goal metadata onto the session. The synthetic first-turn
//! goal-mode reminder (`buildGoalIntroText`) is appended to the dispatch
//! prompt by the callers.

use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;

use rand::Rng;
use serde_json::{Value, json};

use crate::engine::EngineState;
use crate::session_goal::objectives::{GOAL_OBJECTIVE_CHAR_LIMIT, chars_take, write_objective};
use crate::session_goal::runtime::GoalMetadata;

const TRIM_MARKER: &str = "\n\n[… objective trimmed for the auditor — the full prompt was delivered in the chat message …]\n\n";

/// JS: `buildGoalIntroText` — the synthetic goal-mode system reminder attached
/// to the first user prompt. A zero/absent budget omits the budget sentence.
pub fn build_goal_intro_text(token_budget: Option<u64>) -> String {
    let budget_line = match token_budget {
        Some(budget) if budget != 0 => {
            format!(" A token budget of {budget} tokens applies to this goal.")
        }
        _ => String::new(),
    };
    "<system-reminder>\n".to_string()
        + "Goal mode is active for this session. The user message above defines the goal objective. "
        + "Work toward it across turns; whenever you stop before the objective is verifiably complete, the system will automatically prompt you to continue. "
        + "Progress is evaluated independently after each turn, so end every turn with a clear, factual statement of what is done, what was verified, and what remains."
        + &budget_line
        + "\n</system-reminder>"
}

/// JS distills over-long objectives through the small model
/// (`generateSmallModelText` with the completion-criteria prompt). The
/// small-model module is not ported yet, so the seam stays injectable and the
/// production path passes `None` — falling back to the head/tail trim exactly
/// like the JS catch branch when the model is unavailable.
pub struct DistillRequest<'a> {
    pub objective: &'a str,
    pub directory: &'a str,
    pub provider_id: Option<&'a str>,
    pub model_id: Option<&'a str>,
}

pub type DistillFuture = Pin<Box<dyn Future<Output = Result<String, String>> + Send>>;
pub type Distiller = Arc<dyn Fn(DistillRequest<'_>) -> DistillFuture + Send + Sync>;

/// `onWarning(message, error)` sink; JS defaults to `console.warn`.
pub type WarningSink = Arc<dyn Fn(&str, &str) + Send + Sync>;

/// Single-shot session-metadata PATCH seam (`url`, `body`) resolving to the
/// HTTP status; `Ok(())` mirrors `response.ok`.
pub type PatchFuture = Pin<Box<dyn Future<Output = Result<(), u16>> + Send>>;
pub type PatchFetch = Arc<dyn Fn(&str, &Value) -> PatchFuture + Send + Sync>;

pub struct CreateGoalParams {
    pub session_id: String,
    pub directory: String,
    pub objective: String,
    pub token_budget: Option<u64>,
    pub provider_id: Option<String>,
    pub model_id: Option<String>,
}

/// Injectable dependencies; production builds `patch` via
/// [`engine_patch_fetch`] over the managed engine.
pub struct CreateDeps {
    pub patch: PatchFetch,
    pub distiller: Option<Distiller>,
    pub warn: Option<WarningSink>,
}

#[derive(Debug, thiserror::Error)]
pub enum CreateGoalError {
    #[error("goal objective is required")]
    ObjectiveRequired,
    #[error("goal metadata patch failed ({0})")]
    PatchFailed(u16),
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

const RADIX36: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";

/// JS: `${now.toString(36)}${Math.random().toString(36).slice(2, 8)}` —
/// radix-36 timestamp plus 6 random base-36 characters.
fn generate_goal_id(now: u64) -> String {
    let mut id = String::new();
    let mut value = now;
    loop {
        id.push(RADIX36[(value % 36) as usize] as char);
        value /= 36;
        if value == 0 {
            break;
        }
    }
    let id: String = id.chars().rev().collect();
    let mut rng = rand::rng();
    let suffix: String = (0..6)
        .map(|_| RADIX36[rng.random_range(0..36)] as char)
        .collect();
    format!("{id}{suffix}")
}

fn default_warn() -> WarningSink {
    Arc::new(|message: &str, error: &str| {
        tracing::warn!("[session-goal] {message}: {error}");
    })
}

async fn fit_objective(
    objective: &str,
    directory: &str,
    provider_id: Option<&str>,
    model_id: Option<&str>,
    distiller: Option<&Distiller>,
    warn: &WarningSink,
) -> String {
    if objective.chars().count() <= GOAL_OBJECTIVE_CHAR_LIMIT {
        return objective.to_string();
    }

    let mut distilled: Option<String> = None;
    match distiller {
        Some(distiller) => {
            match distiller(DistillRequest {
                objective,
                directory,
                provider_id,
                model_id,
            })
            .await
            {
                Ok(text) => distilled = Some(text),
                Err(error) => warn("goal objective distillation failed", &error),
            }
        }
        // Small-model module not ported yet: behave like the JS failure path.
        None => warn(
            "goal objective distillation failed",
            "small model unavailable",
        ),
    }

    if let Some(distilled) = distilled {
        return chars_take(distilled.trim(), GOAL_OBJECTIVE_CHAR_LIMIT);
    }
    let marker_len = TRIM_MARKER.chars().count();
    let half = GOAL_OBJECTIVE_CHAR_LIMIT.saturating_sub(marker_len) / 2;
    let chars: Vec<char> = objective.chars().collect();
    let head: String = chars.iter().take(half).collect();
    let tail: String = if half == 0 || chars.len() < half {
        String::new()
    } else {
        chars[chars.len() - half..].iter().collect()
    };
    format!("{head}{TRIM_MARKER}{tail}")
}

/// JS: `createSessionGoal` — objective-file-before-metadata ordering, inline
/// fallback, active goal payload, PATCH via the seam.
pub async fn create_session_goal(
    base_url: &str,
    data_dir: &Path,
    params: CreateGoalParams,
    deps: CreateDeps,
) -> Result<GoalMetadata, CreateGoalError> {
    let warn = deps.warn.unwrap_or_else(default_warn);
    let objective = params.objective.trim().to_string();
    let objective_text = fit_objective(
        &objective,
        &params.directory,
        params.provider_id.as_deref(),
        params.model_id.as_deref(),
        deps.distiller.as_ref(),
        &warn,
    )
    .await;
    if objective_text.is_empty() {
        return Err(CreateGoalError::ObjectiveRequired);
    }

    let mut objective_file = false;
    match write_objective(
        data_dir,
        &params.session_id,
        &Value::String(objective_text.clone()),
    )
    .await
    {
        Ok(_) => objective_file = true,
        Err(error) => warn(
            "goal objective file write failed, falling back to inline",
            &error.to_string(),
        ),
    }

    let now = now_ms();
    let token_budget = params.token_budget.filter(|budget| *budget != 0);
    let goal = json!({
        "id": generate_goal_id(now),
        "objective": if objective_file { String::new() } else { chars_take(&objective_text, GOAL_OBJECTIVE_CHAR_LIMIT) },
        "objectiveFile": objective_file,
        "status": "active",
        "tokenBudget": token_budget,
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
    let url = format!(
        "{base_url}/session/{}?directory={}",
        encode_uri_component(&params.session_id),
        encode_uri_component(&params.directory),
    );
    (deps.patch)(&url, &body)
        .await
        .map_err(CreateGoalError::PatchFailed)?;

    Ok(GoalMetadata {
        id: goal["id"].as_str().unwrap_or_default().to_string(),
        objective: goal["objective"].as_str().unwrap_or_default().to_string(),
        objective_file,
        status: "active".to_string(),
        token_budget,
        tokens_used: 0,
        tokens_baseline: 0,
        tokens_committed: 0,
        turns_used: 0,
        blocked_streak: 0,
        audit_fail_streak: 0,
        note: String::new(),
        status_reason: String::new(),
        evaluation_provider_id: String::new(),
        evaluation_model_id: String::new(),
        last_accounted_message_id: String::new(),
        created_at: now as f64,
        updated_at: now as f64,
    })
}

/// Production wiring: PATCH through the managed engine's HTTP client with the
/// Basic auth header. Mirrors the JS `baseUrl` + `authHeaders` injection.
pub fn engine_patch_fetch(engine: Arc<EngineState>) -> PatchFetch {
    Arc::new(move |url: &str, body: &Value| {
        let engine = Arc::clone(&engine);
        let url = url.to_string();
        let body = body.clone();
        Box::pin(async move {
            let mut request = engine
                .http()
                .request(reqwest::Method::PATCH, &url)
                .header("accept", "application/json")
                .header("content-type", "application/json");
            if let Some(auth) = engine.auth_header() {
                request = request.header("authorization", auth);
            }
            let response = request.json(&body).send().await.map_err(|_| 502u16)?;
            let status = response.status().as_u16();
            if (200..300).contains(&status) {
                Ok(())
            } else {
                Err(status)
            }
        })
    })
}

/// JS `encodeURIComponent` (RFC 3986 unreserved + JS extras literal). Local
/// copy of `fs_routes::paths::encode_uri_component` (that module is private).
fn encode_uri_component(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for &byte in value.as_bytes() {
        let keep = byte.is_ascii_alphanumeric()
            || matches!(
                byte,
                b'-' | b'_' | b'.' | b'!' | b'~' | b'*' | b'\'' | b'(' | b')'
            );
        if keep {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use std::sync::Mutex;

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ompchamber-session-goal-create-{tag}-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    struct PatchLog {
        calls: Mutex<Vec<(String, Value)>>,
    }

    impl PatchLog {
        fn ok() -> (PatchFetch, Arc<Self>) {
            let log = Arc::new(PatchLog {
                calls: Mutex::new(Vec::new()),
            });
            let captured = Arc::clone(&log);
            let fetch: PatchFetch = Arc::new(move |url: &str, body: &Value| {
                captured
                    .calls
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push((url.to_string(), body.clone()));
                Box::pin(async { Ok(()) })
            });
            (fetch, log)
        }
    }

    fn deps(patch: PatchFetch) -> CreateDeps {
        CreateDeps {
            patch,
            distiller: None,
            warn: None,
        }
    }

    #[test]
    fn builds_the_same_goal_intro_with_an_optional_budget() {
        let no_budget = build_goal_intro_text(None);
        assert!(no_budget.contains("Goal mode is active for this session."));
        assert!(!no_budget.contains("token budget of"));
        let budgeted = build_goal_intro_text(Some(200_000));
        assert!(budgeted.contains("A token budget of 200000 tokens applies to this goal."));
    }

    #[test]
    fn zero_budget_omits_the_budget_sentence() {
        assert!(!build_goal_intro_text(Some(0)).contains("token budget of"));
    }

    #[tokio::test]
    async fn writes_the_objective_before_patching_active_goal_metadata() {
        let dir = temp_dir("ordering");
        let (patch, log) = PatchLog::ok();

        let goal = create_session_goal(
            "http://opencode.test",
            &dir,
            CreateGoalParams {
                session_id: "ses_123".to_string(),
                directory: "/repo/app".to_string(),
                objective: "Finish and verify the migration".to_string(),
                token_budget: Some(200_000),
                provider_id: Some("openai".to_string()),
                model_id: Some("gpt-5.5".to_string()),
            },
            deps(patch),
        )
        .await
        .expect("goal created");

        // Objective file written before the metadata patch…
        assert_eq!(
            crate::session_goal::objectives::read_objective(&dir, "ses_123").await,
            Some("Finish and verify the migration".to_string())
        );
        let calls = log.calls.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(calls.len(), 1);
        assert_eq!(
            calls[0].0,
            "http://opencode.test/session/ses_123?directory=%2Frepo%2Fapp"
        );
        let written = &calls[0].1["metadata"]["ompchamber"]["goal"];
        assert_eq!(written["status"], "active");
        assert_eq!(written["objectiveFile"], true);
        assert_eq!(written["objective"], "");
        assert_eq!(written["tokenBudget"], 200_000);

        assert_eq!(goal.status, "active");
        assert!(goal.objective_file);
        assert_eq!(goal.token_budget, Some(200_000));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn falls_back_to_inline_metadata_when_objective_storage_fails() {
        let dir = temp_dir("fallback");
        let (patch, log) = PatchLog::ok();
        let warnings = Arc::new(Mutex::new(Vec::new()));
        let warnings_captured = Arc::clone(&warnings);
        let warn: WarningSink = Arc::new(move |message: &str, error: &str| {
            warnings_captured
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push((message.to_string(), error.to_string()));
        });

        // Invalid session id makes writeObjective throw, like disk failure.
        create_session_goal(
            "http://opencode.test",
            &dir,
            CreateGoalParams {
                session_id: "../bad".to_string(),
                directory: "/repo".to_string(),
                objective: "Finish the migration".to_string(),
                token_budget: None,
                provider_id: None,
                model_id: None,
            },
            CreateDeps {
                patch,
                distiller: None,
                warn: Some(warn),
            },
        )
        .await
        .expect("goal created");

        let calls = log.calls.lock().unwrap_or_else(|e| e.into_inner());
        let written = &calls[0].1["metadata"]["ompchamber"]["goal"];
        assert_eq!(written["objective"], "Finish the migration");
        assert_eq!(written["objectiveFile"], false);

        let warnings = warnings.lock().unwrap_or_else(|e| e.into_inner());
        assert!(
            warnings
                .iter()
                .any(|(m, _)| m == "goal objective file write failed, falling back to inline")
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn empty_objective_is_rejected() {
        let dir = temp_dir("empty");
        let (patch, _) = PatchLog::ok();
        let error = create_session_goal(
            "http://opencode.test",
            &dir,
            CreateGoalParams {
                session_id: "ses_ok1".to_string(),
                directory: "/repo".to_string(),
                objective: "   ".to_string(),
                token_budget: None,
                provider_id: None,
                model_id: None,
            },
            deps(patch),
        )
        .await
        .unwrap_err();
        assert_eq!(error.to_string(), "goal objective is required");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn overlong_objective_head_tail_trims_without_distiller() {
        let dir = temp_dir("trim");
        let (patch, log) = PatchLog::ok();
        let warnings = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&warnings);
        let warn: WarningSink = Arc::new(move |m: &str, e: &str| {
            captured
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push((m.to_string(), e.to_string()));
        });

        let long = "a".repeat(3_000) + &"b".repeat(3_000);
        create_session_goal(
            "http://opencode.test",
            &dir,
            CreateGoalParams {
                session_id: "ses_trim1".to_string(),
                directory: "/repo".to_string(),
                objective: long,
                token_budget: None,
                provider_id: None,
                model_id: None,
            },
            CreateDeps {
                patch,
                distiller: None,
                warn: Some(warn),
            },
        )
        .await
        .expect("goal created");

        let calls = log.calls.lock().unwrap_or_else(|e| e.into_inner());
        let objective = calls[0].1["metadata"]["ompchamber"]["goal"]["objective"]
            .as_str()
            .expect("objective");
        // File write failed? No — session id valid; objectiveFile true ⇒ inline empty.
        // The trimmed text lives in the file; verify it there instead.
        let stored = crate::session_goal::objectives::read_objective(&dir, "ses_trim1")
            .await
            .expect("stored");
        assert!(stored.contains(TRIM_MARKER));
        assert!(stored.starts_with(&"a".repeat(2_448)));
        assert!(stored.ends_with(&"b".repeat(2_448)));
        assert!(stored.chars().count() <= GOAL_OBJECTIVE_CHAR_LIMIT);
        assert_eq!(objective, "");

        let warnings = warnings.lock().unwrap_or_else(|e| e.into_inner());
        assert!(
            warnings
                .iter()
                .any(|(m, _)| m == "goal objective distillation failed")
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn distiller_output_replaces_the_objective() {
        let dir = temp_dir("distill");
        let (patch, log) = PatchLog::ok();
        let distiller: Distiller = Arc::new(|request: DistillRequest<'_>| {
            let objective = request.objective.to_string();
            Box::pin(async move {
                assert!(objective.chars().count() > GOAL_OBJECTIVE_CHAR_LIMIT);
                Ok("Criteria: it works".to_string())
            })
        });

        create_session_goal(
            "http://opencode.test",
            &dir,
            CreateGoalParams {
                session_id: "ses_dist1".to_string(),
                directory: "/repo".to_string(),
                objective: "z".repeat(6_000),
                token_budget: None,
                provider_id: None,
                model_id: None,
            },
            CreateDeps {
                patch,
                distiller: Some(distiller),
                warn: None,
            },
        )
        .await
        .expect("goal created");

        assert_eq!(
            crate::session_goal::objectives::read_objective(&dir, "ses_dist1").await,
            Some("Criteria: it works".to_string())
        );
        let calls = log.calls.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(
            calls[0].1["metadata"]["ompchamber"]["goal"]["objectiveFile"],
            true
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn patch_failure_maps_to_status_error() {
        let dir = temp_dir("patchfail");
        let patch: PatchFetch = Arc::new(|_: &str, _: &Value| Box::pin(async { Err(500) }));
        let error = create_session_goal(
            "http://opencode.test",
            &dir,
            CreateGoalParams {
                session_id: "ses_pf01".to_string(),
                directory: "/repo".to_string(),
                objective: "Do it".to_string(),
                token_budget: None,
                provider_id: None,
                model_id: None,
            },
            deps(patch),
        )
        .await
        .unwrap_err();
        assert_eq!(error.to_string(), "goal metadata patch failed (500)");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn goal_ids_are_radix36_timestamps_with_suffix() {
        let a = generate_goal_id(1_000_000_000_000);
        let b = generate_goal_id(1_000_000_000_000);
        // Same timestamp ⇒ identical radix-36 prefix, distinct random suffixes.
        let prefix_len = a.len() - 6;
        assert_eq!(prefix_len, b.len() - 6);
        assert_eq!(&a[..prefix_len], &b[..prefix_len]);
        assert_ne!(a, b);
        assert!(
            a.chars()
                .all(|c| c.is_ascii_digit() || c.is_ascii_lowercase())
        );
    }
}
