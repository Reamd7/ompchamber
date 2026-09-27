//! Payload-shape helpers ported from the top of
//! `openchamber-sessions/routes.js` (`asNonEmptyString`, `splitModel`,
//! `resolveRequestedModel`, `resolveGoalInput`, `resolveWorktreeInput` plus
//! the JS truthiness rules those functions rely on).
//!
//! 中文说明：移植 `openchamber-sessions/routes.js` 顶部的 payload 形状
//! 工具——`asNonEmptyString`、`splitModel`、`resolveRequestedModel`、
//! `resolveGoalInput`、`resolveWorktreeInput`，以及它们依赖的 JS 真值规则。

use serde_json::{Map, Value};

/// JS `asNonEmptyString`: strings trim to a non-empty value or `null`.
///
/// 中文说明：仅接受字符串且 trim 后非空，返回 trim 后的值；其它输入
/// （缺省/非字符串/纯空白）返回 `None`。
pub fn as_non_empty_string(value: Option<&Value>) -> Option<String> {
    let text = value?.as_str()?;
    let trimmed = text.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// JS truthiness over a JSON value (`if (payload?.worktree)`).
///
/// 中文说明：复刻 JS 真值表——null/缺省为假、布尔取本值、数字非 0 为真、
/// 字符串非空为真、数组与对象恒为真。
pub fn is_truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(flag)) => *flag,
        Some(Value::Number(number)) => number.as_f64().is_some_and(|n| n != 0.0),
        Some(Value::String(text)) => !text.is_empty(),
        Some(Value::Array(_)) | Some(Value::Object(_)) => true,
    }
}

/// `{ providerID, modelID }` — the model reference shape shared by the
/// request payload, engine messages, and dispatch bodies.
///
/// 中文说明：请求 payload、engine 消息与派发体共用的模型引用形状。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelRef {
    /// provider ID。
    pub provider_id: String,
    /// model ID。
    pub model_id: String,
}

/// wire 形态序列化辅助。
impl ModelRef {
    /// 序列化为 engine 消息/prompt payload 中的 `{ providerID, modelID }` 对象。
    pub fn to_json(&self) -> Value {
        Value::Object(Map::from_iter([
            (
                "providerID".to_string(),
                Value::String(self.provider_id.clone()),
            ),
            ("modelID".to_string(), Value::String(self.model_id.clone())),
        ]))
    }

    /// `"providerID/modelID"` — the wire form `session.command` expects.
    ///
    /// 中文说明：`"providerID/modelID"` 斜杠串，`session.command` 期望的
    /// wire 形式。
    pub fn to_slash_form(&self) -> String {
        format!("{}/{}", self.provider_id, self.model_id)
    }
}

/// JS `splitModel`: `provider/model` split on the first slash; leading or
/// trailing slashes and slash-less values are rejected.
///
/// 中文说明：按第一个 `/` 拆分 `provider/model`；首尾是斜杠或没有斜杠的值
/// 拒绝（返回 `None`）；model 部分允许继续包含 `/`。
pub fn split_model(value: Option<&Value>) -> Option<ModelRef> {
    let model = as_non_empty_string(value)?;
    let slash = model.find('/')?;
    if slash == 0 || slash == model.len() - 1 {
        return None;
    }
    Some(ModelRef {
        provider_id: model[..slash].to_string(),
        model_id: model[slash + 1..].to_string(),
    })
}

/// JS `resolveRequestedModel`: the `model` string wins, else explicit
/// `providerID` + `modelID`.
///
/// 中文说明：`model` 字符串优先；其次显式 `providerID` + `modelID` 组合；
/// 都无效返回 `None`。
pub fn resolve_requested_model(payload: &Value) -> Option<ModelRef> {
    if let Some(model) = split_model(payload.get("model")) {
        return Some(model);
    }
    let provider_id = as_non_empty_string(payload.get("providerID"));
    let model_id = as_non_empty_string(payload.get("modelID"));
    match (provider_id, model_id) {
        (Some(provider_id), Some(model_id)) => Some(ModelRef {
            provider_id,
            model_id,
        }),
        _ => None,
    }
}

/// goal token 预算下限（含）。
pub const MIN_GOAL_TOKEN_BUDGET: u64 = 1_000;
/// goal token 预算上限（含）。
pub const MAX_GOAL_TOKEN_BUDGET: u64 = 100_000_000;

/// JS `resolveGoalInput` output (`{ ok, enabled, tokenBudget }`).
///
/// 中文说明：`resolveGoalInput` 的输出——`enabled` 表示是否启用 goal，
/// `token_budget` 为通过校验的可选预算。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GoalInput {
    /// 是否启用 goal（payload `goal === true`）。
    pub enabled: bool,
    /// 校验通过的 token 预算；未提供时为 `None`。
    pub token_budget: Option<u64>,
}

/// JS `resolveGoalInput(payload, prompt)` — validates the goal flags before
/// any side effect. Errors map to `OMPChamberControlError(message, 400)`.
///
/// 中文说明：在任何副作用之前校验 goal 标志——预算必须伴随 goal、goal 必须
/// 伴随 prompt、预算必须是区间内的整数；错误消息与 JS 逐字一致并映射为
/// 400 控制错误。
pub fn resolve_goal_input(payload: &Value, prompt: Option<&str>) -> Result<GoalInput, String> {
    let enabled = payload.get("goal") == Some(&Value::Bool(true));
    let budget_value = payload.get("goalTokenBudget");
    if budget_value.is_some() && !enabled {
        return Err("goalTokenBudget requires goal".to_string());
    }
    if enabled && prompt.is_none() {
        return Err("prompt is required when goal is enabled".to_string());
    }
    let Some(budget_value) = budget_value else {
        return Ok(GoalInput {
            enabled,
            token_budget: None,
        });
    };
    // `Number.isSafeInteger` + the documented range. Booleans, strings, and
    // non-integral numbers all fail the JS check.
    let budget = budget_value
        .as_f64()
        .filter(|value| value.is_finite() && value.fract() == 0.0);
    let token_budget = match budget {
        Some(value)
            if (MIN_GOAL_TOKEN_BUDGET as f64..=MAX_GOAL_TOKEN_BUDGET as f64).contains(&value) =>
        {
            Some(value as u64)
        }
        _ => {
            return Err(format!(
                "goalTokenBudget must be an integer from {MIN_GOAL_TOKEN_BUDGET} to {MAX_GOAL_TOKEN_BUDGET}"
            ));
        }
    };
    Ok(GoalInput {
        enabled,
        token_budget,
    })
}

/// JS `resolveWorktreeInput`: `{ mode: 'new', name, branchName?, startRef?,
/// setUpstream? }` — `null` unless `worktree` is an object with a name.
///
/// 中文说明：`worktree` 为对象且带非空 `name` 时归一化为
/// `{ mode: 'new', name, branchName?, startRef?, setUpstream? }`；
/// `setUpstream` 仅在顶层 payload 中为布尔值时带入，其余情况返回 `None`。
pub fn resolve_worktree_input(payload: &Value) -> Option<Value> {
    let worktree = payload.get("worktree")?;
    if !worktree.is_object() {
        return None;
    }
    let name = as_non_empty_string(worktree.get("name"))?;
    let mut input = Map::new();
    input.insert("mode".to_string(), Value::String("new".to_string()));
    input.insert("name".to_string(), Value::String(name));
    if let Some(branch) = as_non_empty_string(worktree.get("branchName")) {
        input.insert("branchName".to_string(), Value::String(branch));
    }
    if let Some(start_ref) = as_non_empty_string(worktree.get("startRef")) {
        input.insert("startRef".to_string(), Value::String(start_ref));
    }
    if let Some(set_upstream) = payload
        .get("setUpstream")
        .filter(|value| value.is_boolean())
    {
        input.insert("setUpstream".to_string(), set_upstream.clone());
    }
    Some(Value::Object(input))
}

/// payload 形状工具与 JS 行为一致性的单元测试。
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 验证 `split_model` 只按第一个斜杠拆分，并拒绝首/尾斜杠与无斜杠输入。
    #[test]
    fn split_model_accepts_first_slash_only() {
        assert_eq!(
            split_model(Some(&json!("openai/gpt-5.5"))),
            Some(ModelRef {
                provider_id: "openai".into(),
                model_id: "gpt-5.5".into()
            })
        );
        assert_eq!(
            split_model(Some(&json!(" openai/gpt/mini "))),
            Some(ModelRef {
                provider_id: "openai".into(),
                model_id: "gpt/mini".into()
            })
        );
        assert_eq!(split_model(Some(&json!("/leading"))), None);
        assert_eq!(split_model(Some(&json!("trailing/"))), None);
        assert_eq!(split_model(Some(&json!("nonsense"))), None);
        assert_eq!(split_model(Some(&json!("  "))), None);
        assert_eq!(split_model(None), None);
        assert_eq!(split_model(Some(&json!(42))), None);
    }

    /// 验证 `model` 字符串优先于显式 `providerID`+`modelID`，两者皆缺时为
    /// `None`。
    #[test]
    fn resolve_requested_model_prefers_model_string() {
        let payload = json!({
            "model": "openai/gpt-5.5",
            "providerID": "anthropic",
            "modelID": "claude"
        });
        assert_eq!(
            resolve_requested_model(&payload).map(|m| m.to_slash_form()),
            Some("openai/gpt-5.5".to_string())
        );
        let payload = json!({ "providerID": "anthropic", "modelID": "claude" });
        assert_eq!(
            resolve_requested_model(&payload).map(|m| m.to_slash_form()),
            Some("anthropic/claude".to_string())
        );
        assert_eq!(resolve_requested_model(&json!({ "providerID": "x" })), None);
        assert_eq!(resolve_requested_model(&json!({})), None);
    }

    /// 验证 goal 校验规则与 JS 错误消息逐字一致（缺 prompt、预算无 goal、
    /// 预算越界/非整数、`null` 预算视为已提供）。
    #[test]
    fn goal_input_validation_matches_js_messages() {
        assert_eq!(
            resolve_goal_input(&json!({ "goal": true }), None),
            Err("prompt is required when goal is enabled".to_string())
        );
        assert_eq!(
            resolve_goal_input(&json!({ "prompt": "x", "goalTokenBudget": 5 }), Some("x")),
            Err("goalTokenBudget requires goal".to_string())
        );
        assert_eq!(
            resolve_goal_input(
                &json!({ "prompt": "x", "goal": true, "goalTokenBudget": 999 }),
                Some("x")
            ),
            Err("goalTokenBudget must be an integer from 1000 to 100000000".to_string())
        );
        assert_eq!(
            resolve_goal_input(
                &json!({ "prompt": "x", "goal": true, "goalTokenBudget": 1.5 }),
                Some("x")
            ),
            Err("goalTokenBudget must be an integer from 1000 to 100000000".to_string())
        );
        // `goalTokenBudget: null` counts as defined (JS `!== undefined`).
        assert_eq!(
            resolve_goal_input(
                &json!({ "prompt": "x", "goalTokenBudget": null }),
                Some("x")
            ),
            Err("goalTokenBudget requires goal".to_string())
        );
        assert_eq!(
            resolve_goal_input(
                &json!({ "prompt": "x", "goal": true, "goalTokenBudget": 100000000 }),
                Some("x")
            ),
            Ok(GoalInput {
                enabled: true,
                token_budget: Some(100000000)
            })
        );
        assert_eq!(
            resolve_goal_input(&json!({ "prompt": "x" }), Some("x")),
            Ok(GoalInput {
                enabled: false,
                token_budget: None
            })
        );
    }

    /// 验证 worktree 输入归一化：必须是带 name 的对象，`setUpstream` 仅在
    /// 布尔值时带入。
    #[test]
    fn worktree_input_requires_named_object() {
        let payload = json!({
            "worktree": { "name": "side-task", "branchName": "ompchamber/side-task", "startRef": "main" },
            "setUpstream": false
        });
        assert_eq!(
            resolve_worktree_input(&payload),
            Some(json!({
                "mode": "new",
                "name": "side-task",
                "branchName": "ompchamber/side-task",
                "startRef": "main",
                "setUpstream": false,
            }))
        );
        assert_eq!(resolve_worktree_input(&json!({ "worktree": {} })), None);
        assert_eq!(resolve_worktree_input(&json!({ "worktree": "side" })), None);
        assert_eq!(resolve_worktree_input(&json!({})), None);
        // `setUpstream` only rides along when it is a boolean.
        let payload = json!({ "worktree": { "name": "x" }, "setUpstream": "yes" });
        assert_eq!(
            resolve_worktree_input(&payload),
            Some(json!({ "mode": "new", "name": "x" }))
        );
    }

    /// 验证 `is_truthy` 复刻 JS 真值表（null/false/0/空串/缺省为假，其余
    /// 为真）。
    #[test]
    fn js_truthiness_rules() {
        assert!(is_truthy(Some(&json!({}))));
        assert!(is_truthy(Some(&json!([]))));
        assert!(is_truthy(Some(&json!(1))));
        assert!(is_truthy(Some(&json!("x"))));
        assert!(!is_truthy(Some(&json!(false))));
        assert!(!is_truthy(Some(&json!(null))));
        assert!(!is_truthy(Some(&json!(0))));
        assert!(!is_truthy(Some(&json!(""))));
        assert!(!is_truthy(None));
    }
}
