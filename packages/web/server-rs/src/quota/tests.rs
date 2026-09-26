//! Quota module tests — provider fixtures against the fake transport
//! (mirroring the JS provider tests), credential store roundtrips, runtime
//! coalescing, and route shapes.

use std::sync::Arc;

use axum::body::Body;
use axum::http::header::CONTENT_TYPE;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

use crate::quota::credentials::{write_managed_credential, write_quota_credential};
use crate::quota::http::{HttpRequest, HttpResponse};
use crate::quota::providers::{
    claude, codex, copilot, crof, cursor, deepseek, google, kimi, minimax, nanogpt, neuralwatt,
    ollama_cloud, openai, opencode_go, openrouter, wafer, xai, zai, zhipuai,
};
use crate::quota::routes;
use crate::quota::runtime::QuotaRuntime;
use crate::quota::tests_support::{FIXED_NOW, FakeHttp, TestEnv, json_response, status_response};

fn quota_runtime(env: &TestEnv, responses: Vec<HttpResponse>) -> (Arc<QuotaRuntime>, FakeHttp) {
    let fake = FakeHttp::new(responses);
    let runtime = Arc::new(QuotaRuntime::new(env.deps(fake.clone().transport())));
    (runtime, fake)
}

fn bearer_used(fake: &FakeHttp, index: usize) -> String {
    let requests = fake.requests.lock().unwrap();
    requests[index]
        .headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("authorization"))
        .map(|(_, value)| value.clone())
        .unwrap_or_default()
}

// ============== codex ==============

#[tokio::test]
async fn codex_labels_weekly_only_window_from_its_duration() {
    let env = TestEnv::new();
    env.set_auth(json!({ "openai": { "access": "test-token" } }));
    let (runtime, _) = quota_runtime(
        &env,
        vec![json_response(json!({
            "rate_limit": {
                "primary_window": { "used_percent": 3, "limit_window_seconds": 604800, "reset_at": 1784491827 },
                "secondary_window": null,
            }
        }))],
    );

    let result = codex::fetch_quota(runtime).await;

    assert_eq!(result["ok"], json!(true));
    assert_eq!(
        result["usage"]["windows"]["weekly"]["usedPercent"],
        json!(3)
    );
    assert!(result["usage"]["windows"].get("5h").is_none());
}

#[tokio::test]
async fn codex_labels_five_hour_and_weekly_windows() {
    let env = TestEnv::new();
    env.set_auth(json!({ "openai": { "access": "test-token" } }));
    let (runtime, _) = quota_runtime(
        &env,
        vec![json_response(json!({
            "rate_limit": {
                "primary_window": { "used_percent": 10, "limit_window_seconds": 18000, "reset_at": 1784491827 },
                "secondary_window": { "used_percent": 20, "limit_window_seconds": 604800, "reset_at": 1784491827 },
            }
        }))],
    );

    let result = codex::fetch_quota(runtime).await;

    assert_eq!(result["usage"]["windows"]["5h"]["usedPercent"], json!(10));
    assert_eq!(
        result["usage"]["windows"]["weekly"]["usedPercent"],
        json!(20)
    );
}

#[tokio::test]
async fn codex_surfaces_spend_control_individual_limit() {
    let env = TestEnv::new();
    env.set_auth(json!({ "openai": { "access": "test-token" } }));
    let (runtime, fake) = quota_runtime(
        &env,
        vec![json_response(json!({
            "plan_type": "business",
            "rate_limit": null,
            "credits": { "has_credits": true, "unlimited": false, "balance": null },
            "spend_control": {
                "individual_limit": {
                    "limit": "7500",
                    "used": "2674.8724080324173",
                    "remaining": "4825.127591967583",
                    "used_percent": 36,
                    "remaining_percent": 64
                }
            }
        }))],
    );

    let result = codex::fetch_quota(runtime).await;

    assert_eq!(result["ok"], json!(true));
    assert_eq!(
        result["usage"]["windows"]["credits"]["usedPercent"],
        json!(36)
    );
    assert_eq!(
        result["usage"]["windows"]["credits"]["valueLabel"],
        json!("2675 / 7500 used")
    );
    assert_eq!(fake.requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn codex_reports_session_expired_on_401_and_not_configured_without_auth() {
    let env = TestEnv::new();
    let (runtime, _) = quota_runtime(&env, vec![status_response(401, vec![], b"{}")]);
    let not_configured = codex::fetch_quota(runtime).await;
    assert_eq!(not_configured["configured"], json!(false));
    assert_eq!(not_configured["error"], json!("Not configured"));

    env.set_auth(json!({ "openai": { "access": "test-token" } }));
    let (runtime, _) = quota_runtime(&env, vec![status_response(401, vec![], b"{}")]);
    let expired = codex::fetch_quota(runtime).await;
    assert_eq!(expired["configured"], json!(true));
    assert_eq!(
        expired["error"],
        json!("Session expired \u{2014} please re-authenticate with OpenAI")
    );
}

// ============== openai (internal twin) ==============

#[tokio::test]
async fn openai_maps_windows_without_account_id() {
    let env = TestEnv::new();
    env.set_auth(json!({ "chatgpt": { "access": "tok" } }));
    let (runtime, _) = quota_runtime(
        &env,
        vec![json_response(json!({
            "rate_limit": {
                "primary_window": { "used_percent": 10, "limit_window_seconds": 18000, "reset_at": 1784491827 },
                "secondary_window": { "used_percent": 20, "limit_window_seconds": 604800, "reset_at": 1784491827 },
            }
        }))],
    );

    let result = openai::fetch_quota(runtime).await;

    assert_eq!(result["providerId"], json!("openai"));
    assert_eq!(result["usage"]["windows"]["5h"]["usedPercent"], json!(10));
    assert_eq!(
        result["usage"]["windows"]["weekly"]["usedPercent"],
        json!(20)
    );
}

// ============== copilot ==============

#[tokio::test]
async fn copilot_exposes_only_premium_interactions() {
    let env = TestEnv::new();
    env.set_auth(json!({ "github-copilot": { "access": "test-token" } }));
    let payload = json!({
        "quota_reset_date": "2026-09-01T00:00:00Z",
        "quota_snapshots": {
            "chat": { "entitlement": 100, "remaining": 80 },
            "completions": { "entitlement": 1000, "remaining": 900 },
            "premium_interactions": { "entitlement": 300, "remaining": 225 },
        }
    });
    let (runtime, _) = quota_runtime(&env, vec![json_response(payload.clone())]);

    let result = copilot::fetch_quota(runtime).await;

    let windows = result["usage"]["windows"].as_object().unwrap();
    assert_eq!(windows.len(), 1);
    assert_eq!(windows["premium_interactions"]["usedPercent"], json!(25));
    assert_eq!(
        windows["premium_interactions"]["valueLabel"],
        json!("225 / 300 left")
    );
}

#[tokio::test]
async fn copilot_reports_unlimited_and_percent_fallback() {
    let env = TestEnv::new();
    env.set_auth(json!({ "copilot": { "access": "test-token" } }));

    let (runtime, _) = quota_runtime(
        &env,
        vec![json_response(json!({
            "quota_reset_date": "2026-09-01T00:00:00Z",
            "quota_snapshots": { "premium_interactions": { "unlimited": true, "entitlement": -1, "remaining": -1 } }
        }))],
    );
    let unlimited = copilot::fetch_quota_addon(runtime).await;
    assert_eq!(unlimited["providerId"], json!("github-copilot-addon"));
    assert_eq!(
        unlimited["usage"]["windows"]["premium_interactions"]["usedPercent"],
        json!(null)
    );
    assert_eq!(
        unlimited["usage"]["windows"]["premium_interactions"]["valueLabel"],
        json!("Unlimited")
    );

    let (runtime, _) = quota_runtime(
        &env,
        vec![json_response(json!({
            "quota_reset_date": "2026-09-01T00:00:00Z",
            "quota_snapshots": { "premium_interactions": { "entitlement": 0, "remaining": 0, "percent_remaining": 75.5 } }
        }))],
    );
    let fallback = copilot::fetch_quota_addon(runtime).await;
    let used = fallback["usage"]["windows"]["premium_interactions"]["usedPercent"]
        .as_f64()
        .unwrap();
    assert!((used - 24.5).abs() < 1e-9);
    assert_eq!(
        fallback["usage"]["windows"]["premium_interactions"]["valueLabel"],
        json!(null)
    );
}

// ============== crof ==============

#[tokio::test]
async fn crof_reports_credits_value_label_and_maps_401() {
    let env = TestEnv::new();
    env.set_auth(json!({ "crof": { "key": "test-token" } }));

    let (runtime, _) = quota_runtime(
        &env,
        vec![json_response(
            json!({ "usable_requests": 450, "credits": 12.3456 }),
        )],
    );
    let result = crof::fetch_quota(runtime).await;
    assert_eq!(result["ok"], json!(true));
    let credits = &result["usage"]["windows"]["credits"];
    assert_eq!(credits["usedPercent"], json!(null));
    assert_eq!(credits["valueLabel"], json!("$12.35"));

    let (runtime, _) = quota_runtime(&env, vec![status_response(401, vec![], b"{}")]);
    let expired = crof::fetch_quota(runtime).await;
    assert_eq!(
        expired["error"],
        json!("Session expired \u{2014} please re-authenticate with CrofAI")
    );
}

// ============== deepseek ==============

#[tokio::test]
async fn deepseek_builds_credits_balance_from_documented_payload() {
    let env = TestEnv::new();
    env.set_auth(json!({ "deepseek": { "key": "test-token" } }));

    let (runtime, _) = quota_runtime(
        &env,
        vec![json_response(json!({
            "is_available": true,
            "balance_infos": [
                { "currency": "USD", "total_balance": "7.54", "granted_balance": "0.00", "topped_up_balance": "7.54" }
            ]
        }))],
    );
    let result = deepseek::fetch_quota(runtime).await;
    let window = &result["usage"]["windows"]["credits_balance"];
    assert_eq!(window["valueLabel"], json!("$7.54"));
    assert_eq!(window["usedPercent"], json!(null));

    // CNY fallback when no USD entry exists.
    let (runtime, _) = quota_runtime(
        &env,
        vec![json_response(json!({
            "is_available": true,
            "balance_infos": [{ "currency": "CNY", "total_balance": "100.00" }]
        }))],
    );
    let result = deepseek::fetch_quota(runtime).await;
    assert_eq!(
        result["usage"]["windows"]["credits_balance"]["valueLabel"],
        json!("\u{a5}100.00")
    );
}

#[tokio::test]
async fn deepseek_maps_auth_errors_and_empty_balances() {
    let env = TestEnv::new();
    env.set_auth(json!({ "deepseek": { "key": "test-token" } }));

    for status in [401u16, 403] {
        let (runtime, _) = quota_runtime(&env, vec![status_response(status, vec![], b"{}")]);
        let result = deepseek::fetch_quota(runtime).await;
        assert_eq!(
            result["error"],
            json!("Session expired \u{2014} please re-authenticate with DeepSeek")
        );
    }

    let (runtime, _) = quota_runtime(
        &env,
        vec![json_response(json!({
            "balance_infos": [{ "currency": "USD", "total_balance": "" }]
        }))],
    );
    let result = deepseek::fetch_quota(runtime).await;
    assert_eq!(result["ok"], json!(false));
    assert_eq!(result["error"], json!("No quota data in response"));
    assert_eq!(result["usage"], json!(null));

    // Literal zero stays a valid balance.
    let (runtime, _) = quota_runtime(
        &env,
        vec![json_response(json!({
            "balance_infos": [{ "currency": "USD", "total_balance": "0.00" }]
        }))],
    );
    let result = deepseek::fetch_quota(runtime).await;
    assert_eq!(result["ok"], json!(true));
    assert_eq!(
        result["usage"]["windows"]["credits_balance"]["valueLabel"],
        json!("$0.00")
    );
}

// ============== kimi ==============

#[tokio::test]
async fn kimi_computes_weekly_and_rate_limit_windows() {
    let env = TestEnv::new();
    env.set_auth(json!({ "kimi-for-coding": { "key": "test-token" } }));

    let (runtime, _) = quota_runtime(
        &env,
        vec![json_response(json!({
            "usage": { "limit": "100", "used": "100", "resetTime": "2026-08-04T06:21:48.514003Z" },
            "limits": [{
                "window": { "duration": 300, "timeUnit": "TIME_UNIT_MINUTE" },
                "detail": { "limit": "100", "remaining": "100", "resetTime": "2026-08-03T07:21:48.514003Z" },
            }]
        }))],
    );
    let result = kimi::fetch_quota(runtime).await;
    assert_eq!(
        result["usage"]["windows"]["weekly"]["usedPercent"],
        json!(100)
    );
    assert_eq!(
        result["usage"]["windows"]["Rate Limit (300m)"]["usedPercent"],
        json!(0)
    );

    // used wins over remaining when both exist.
    let (runtime, _) = quota_runtime(
        &env,
        vec![json_response(json!({
            "usage": { "limit": "100", "used": "30", "remaining": "999" },
            "limits": []
        }))],
    );
    let result = kimi::fetch_quota(runtime).await;
    assert_eq!(
        result["usage"]["windows"]["weekly"]["usedPercent"],
        json!(30)
    );

    // Remaining fallback and null when neither is present.
    let (runtime, _) = quota_runtime(
        &env,
        vec![json_response(
            json!({ "usage": { "limit": "2048", "remaining": "512" } }),
        )],
    );
    let result = kimi::fetch_quota(runtime).await;
    assert_eq!(
        result["usage"]["windows"]["weekly"]["usedPercent"],
        json!(75)
    );

    let (runtime, _) = quota_runtime(
        &env,
        vec![json_response(json!({ "usage": { "limit": "100" } }))],
    );
    let result = kimi::fetch_quota(runtime).await;
    assert_eq!(
        result["usage"]["windows"]["weekly"]["usedPercent"],
        json!(null)
    );

    // API errors surface the status.
    let (runtime, _) = quota_runtime(&env, vec![status_response(401, vec![], b"{}")]);
    let result = kimi::fetch_quota(runtime).await;
    assert_eq!(result["configured"], json!(true));
    assert_eq!(result["error"], json!("API error: 401"));
}

#[tokio::test]
async fn kimi_is_not_configured_without_credentials() {
    let env = TestEnv::new();
    let (runtime, _) = quota_runtime(&env, vec![]);
    let result = kimi::fetch_quota(runtime).await;
    assert_eq!(result["ok"], json!(false));
    assert_eq!(result["configured"], json!(false));
    assert_eq!(result["error"], json!("Not configured"));
}

// ============== openrouter / nanogpt / wafer / neuralwatt ==============

#[tokio::test]
async fn openrouter_reports_remaining_and_spent() {
    let env = TestEnv::new();
    env.set_auth(json!({ "openrouter": { "key": "k" } }));
    let (runtime, _) = quota_runtime(
        &env,
        vec![json_response(
            json!({ "data": { "total_credits": 20.0, "total_usage": 7.5 } }),
        )],
    );
    let result = openrouter::fetch_quota(runtime).await;
    let credits = &result["usage"]["windows"]["credits"];
    assert_eq!(
        credits["valueLabel"],
        json!("$12.50 left \u{b7} $7.50 spent")
    );
    assert_eq!(credits["usedPercent"], json!(null));
}

#[tokio::test]
async fn nanogpt_builds_daily_and_monthly_windows_with_state_labels() {
    let env = TestEnv::new();
    env.set_auth(json!({ "nanogpt": { "key": "k" } }));
    let (runtime, _) = quota_runtime(
        &env,
        vec![json_response(json!({
            "period": { "currentPeriodEnd": "2026-09-01T00:00:00Z" },
            "daily": { "percentUsed": 0.25, "resetAt": "2026-08-12T00:00:00Z" },
            "monthly": { "used": 40, "limit": 200 },
            "state": "past_due"
        }))],
    );
    let result = nanogpt::fetch_quota(runtime).await;
    let windows = &result["usage"]["windows"];
    assert_eq!(windows["daily"]["usedPercent"], json!(25));
    assert_eq!(windows["daily"]["valueLabel"], json!("(past_due)"));
    assert_eq!(windows["monthly"]["usedPercent"], json!(20));
    assert_eq!(windows["monthly"]["valueLabel"], json!("(past_due)"));
}

#[tokio::test]
async fn wafer_labels_window_from_payload_timestamps_and_overage() {
    let env = TestEnv::new();
    env.set_auth(json!({ "wafer": { "key": "k" } }));
    let (runtime, _) = quota_runtime(
        &env,
        vec![json_response(json!({
            "remaining_included_requests": 17,
            "included_request_limit": 50,
            "overage_request_count": 3,
            "current_period_used_percent": 96.4,
            "window_start": 1_780_000_000_000i64,
            "window_end": 1_780_018_000_000i64,
            "plan_tier": "pro"
        }))],
    );
    let result = wafer::fetch_quota(runtime).await;
    let window = &result["usage"]["windows"]["5h"];
    assert_eq!(window["usedPercent"], json!(96.4));
    assert_eq!(
        window["valueLabel"],
        json!("pro \u{b7} 17 / 50 left \u{b7} +3 overage")
    );
    assert_eq!(window["windowSeconds"], json!(18000));

    let (runtime, _) = quota_runtime(&env, vec![json_response(json!({}))]);
    let result = wafer::fetch_quota(runtime).await;
    assert_eq!(result["error"], json!("No quota data in response"));
}

#[tokio::test]
async fn neuralwatt_keys_subscription_window_by_plan_and_adds_credits() {
    let env = TestEnv::new();
    env.set_auth(json!({ "neuralwatt": { "key": "k" } }));
    let (runtime, _) = quota_runtime(
        &env,
        vec![json_response(json!({
            "snapshot_at": "2026-04-16T18:30:00Z",
            "balance": { "credits_remaining_usd": 32.6774 },
            "subscription": {
                "plan": "standard",
                "in_overage": false,
                "current_period_end": "2027-04-11T05:05:25Z",
                "kwh_included": 20.0,
                "kwh_used": 13.9023,
            },
            "key": { "name": "my-production-key", "allowance": null }
        }))],
    );
    let result = neuralwatt::fetch_quota(runtime).await;
    let standard = &result["usage"]["windows"]["standard"];
    let used = standard["usedPercent"].as_f64().unwrap();
    assert!((used - (13.9023 / 20.0) * 100.0).abs() < 1e-6);
    assert_eq!(standard["windowSeconds"], json!(null));
    assert_eq!(standard["resetAt"], json!(1_807_419_925_000i64));
    let credits = &result["usage"]["windows"]["credits_balance"];
    assert_eq!(credits["valueLabel"], json!("$32.68"));
}

#[tokio::test]
async fn neuralwatt_allowance_window_prefers_effective_limit() {
    let env = TestEnv::new();
    env.set_auth(json!({ "neuralwatt": { "key": "k" } }));
    let (runtime, _) = quota_runtime(
        &env,
        vec![json_response(json!({
            "balance": { "credits_remaining_usd": 5.0 },
            "subscription": null,
            "key": {
                "name": "prod",
                "allowance": { "spent_usd": 2.0, "limit_usd": 100.0, "period": "month", "reset_at": 1_790_000_000_000i64 }
            }
        }))],
    );
    let result = neuralwatt::fetch_quota(runtime).await;
    let monthly = &result["usage"]["windows"]["monthly"];
    // Effective limit = min(100, 5 + 2) = 7 → 2/7 used.
    let used = monthly["usedPercent"].as_f64().unwrap();
    assert!((used - (2.0 / 7.0) * 100.0).abs() < 1e-6);
    assert_eq!(monthly["valueLabel"], json!("prod"));
    assert_eq!(monthly["windowSeconds"], json!(2_592_000));
}

// ============== zai / zhipuai / minimax ==============

#[tokio::test]
async fn zai_surfaces_token_and_time_limit_windows() {
    let env = TestEnv::new();
    env.set_auth(json!({ "zai-coding-plan": { "key": "test-token" } }));

    let (runtime, _) = quota_runtime(
        &env,
        vec![json_response(json!({
            "data": { "limits": [
                { "type": "TOKENS_LIMIT", "unit": 3, "number": 5, "percentage": 0 },
                { "type": "TOKENS_LIMIT", "unit": 6, "number": 1, "percentage": 100, "nextResetTime": 1785659659993i64 },
                { "type": "TIME_LIMIT", "unit": 5, "number": 1, "percentage": 0, "nextResetTime": 1787128459979i64 },
            ]}
        }))],
    );
    let result = zai::fetch_quota(runtime).await;
    let windows = &result["usage"]["windows"];
    assert_eq!(windows["5h"]["usedPercent"], json!(0));
    assert_eq!(windows["5h"]["remainingPercent"], json!(100));
    assert_eq!(windows["5h"]["windowSeconds"], json!(18000));
    assert_eq!(windows["5h"]["resetAt"], json!(null));
    assert_eq!(windows["weekly"]["usedPercent"], json!(100));
    assert_eq!(windows["weekly"]["resetAt"], json!(1_785_659_659_993i64));
    assert_eq!(windows["MCP Tools"]["windowSeconds"], json!(2_592_000));
    assert_eq!(windows["MCP Tools"]["resetAt"], json!(1_787_128_459_979i64));
}

#[tokio::test]
async fn zai_maps_credit_limits_with_value_labels_and_plan_level() {
    let env = TestEnv::new();
    env.set_auth(json!({ "zai-coding-plan": { "key": "test-token" } }));
    let (runtime, _) = quota_runtime(
        &env,
        vec![json_response(json!({
            "code": 200,
            "data": {
                "limits": [
                    { "type": "CREDIT_LIMIT", "unit": 3, "number": 5, "usage": 12000, "currentValue": 65, "remaining": 11934, "percentage": 1, "nextResetTime": 1787257978907i64 },
                    { "type": "CREDIT_LIMIT", "unit": 6, "number": 1, "usage": 60000, "currentValue": 65, "remaining": 59934, "percentage": 1, "nextResetTime": 1787844668997i64 },
                ],
                "level": "pro"
            }
        }))],
    );
    let result = zai::fetch_quota(runtime).await;
    let windows = &result["usage"]["windows"];
    assert_eq!(result["planLabel"], json!("pro"));
    assert_eq!(windows["5h"]["valueLabel"], json!("65 / 12k credits"));
    assert_eq!(windows["weekly"]["valueLabel"], json!("65 / 60k credits"));
}

#[tokio::test]
async fn zhipuai_resolves_keys_from_auth_and_config_layers() {
    let env = TestEnv::new();

    // From the OpenCode config layer when auth.json has no entry.
    let config_dir = env.home.join(".config").join("opencode");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::write(
        config_dir.join("config.json"),
        serde_json::to_string(&json!({
            "provider": { "zhipu": { "options": { "apiKey": "config-key" } } }
        }))
        .unwrap(),
    )
    .unwrap();
    let (runtime, fake) = quota_runtime(
        &env,
        vec![json_response(json!({
            "data": { "limits": [
                { "type": "TOKENS_LIMIT", "unit": 3, "number": 5, "percentage": 42, "nextResetTime": 1787257978907i64 },
                { "type": "TIME_LIMIT", "percentage": 7 }
            ]}
        }))],
    );
    let result = zhipuai::fetch_quota(runtime).await;
    assert_eq!(bearer_used(&fake, 0), "Bearer config-key");
    let windows = &result["usage"]["windows"];
    assert_eq!(windows["Tokens"]["usedPercent"], json!(42));
    assert_eq!(windows["Tokens"]["windowSeconds"], json!(18000));
    assert_eq!(windows["MCP Tools"]["usedPercent"], json!(7));
    assert_eq!(windows["MCP Tools"]["windowSeconds"], json!(2_592_000));

    // The auth.json entry wins over the config layer.
    env.set_auth(json!({ "zhipuai": { "key": "auth-key" } }));
    let (runtime, fake) = quota_runtime(
        &env,
        vec![json_response(json!({ "data": { "limits": [] } }))],
    );
    zhipuai::fetch_quota(runtime).await;
    assert_eq!(bearer_used(&fake, 0), "Bearer auth-key");
}

#[tokio::test]
async fn minimax_token_plan_reports_remaining_semantics_and_skips_inactive_weekly() {
    let env = TestEnv::new();
    env.set_auth(json!({ "minimax-coding-plan": { "key": "k" } }));

    // M3 token-plan payload: usage_count is REMAINING (used = total - remaining).
    let (runtime, _) = quota_runtime(
        &env,
        vec![json_response(json!({
            "base_resp": { "status_code": 0 },
            "model_remains": [{
                "model_name": "MiniMax-M2",
                "current_interval_total_count": 500,
                "current_interval_usage_count": 400,
                "current_interval_remaining_percent": 20,
                "start_time": 1_780_000_000,
                "end_time": 1_780_018_000,
                "remains_time": 9_000_000,
                "current_weekly_status": 3
            }]
        }))],
    );
    let result = minimax::fetch_quota(runtime).await;
    let windows = &result["usage"]["windows"];
    assert_eq!(windows["5h"]["usedPercent"], json!(80));
    assert!(
        windows.get("weekly").is_none(),
        "status 3 omits the weekly window"
    );
    assert_eq!(windows["5h"]["windowSeconds"], json!(18000));

    // Legacy coding-plan payload: usage_count is CONSUMED and weekly active.
    let (runtime, _) = quota_runtime(
        &env,
        vec![
            status_response(404, vec![], b"{}"),
            json_response(json!({
                "model_remains": [{
                    "model_name": "general",
                    "current_interval_total_count": 200,
                    "current_interval_usage_count": 50,
                    "current_weekly_total_count": 1000,
                    "current_weekly_usage_count": 100,
                    "current_weekly_remaining_percent": 90
                }]
            })),
        ],
    );
    let result = minimax::fetch_quota(runtime).await;
    let windows = &result["usage"]["windows"];
    assert_eq!(windows["5h"]["usedPercent"], json!(25));
    assert_eq!(windows["weekly"]["usedPercent"], json!(10));

    // Both endpoints unusable.
    let (runtime, _) = quota_runtime(
        &env,
        vec![
            status_response(500, vec![], b"{}"),
            status_response(500, vec![], b"{}"),
        ],
    );
    let result = minimax::fetch_quota(runtime).await;
    assert_eq!(result["error"], json!("API returned no usable quota data"));
}

#[tokio::test]
async fn minimax_cn_uses_the_cn_endpoints() {
    let env = TestEnv::new();
    env.set_auth(json!({ "minimax-cn-coding-plan": { "key": "k" } }));
    let (runtime, fake) = quota_runtime(
        &env,
        vec![
            status_response(500, vec![], b"{}"),
            status_response(500, vec![], b"{}"),
        ],
    );
    minimax::fetch_quota_cn(runtime).await;
    let urls = fake.urls.lock().unwrap().clone();
    assert_eq!(urls[0], "https://api.minimaxi.com/v1/token_plan/remains");
    assert_eq!(
        urls[1],
        "https://www.minimaxi.com/v1/api/openplatform/coding_plan/remains"
    );
}

// ============== ollama-cloud / opencode-go ==============

#[tokio::test]
async fn ollama_cloud_parses_settings_html_and_rejects_redirects() {
    let env = TestEnv::new();
    let html = r#"<html><div>Session usage <b>42%</b> of your window. Weekly usage 7%. Premium requests: 12 / 40 used</div></html>"#;
    let windows = ollama_cloud::parse_ollama_settings_html(html, FIXED_NOW);
    assert_eq!(windows["session"]["usedPercent"], json!(42));
    assert_eq!(windows["weekly"]["usedPercent"], json!(7));
    assert_eq!(windows["premium"]["usedPercent"], json!(30));
    assert_eq!(windows["premium"]["valueLabel"], json!("12 / 40"));

    let redirect = status_response(302, vec![("location", "https://ollama.com/login")], b"");
    let fake = FakeHttp::new(vec![redirect]);
    let deps = env.deps(fake.transport());
    let error = ollama_cloud::fetch_ollama_cloud_usage(&deps, &json!({"cookie": "session=secret"}))
        .await
        .unwrap_err();
    assert_eq!(error, "Ollama Cloud authentication failed");

    let empty_page = FakeHttp::new(vec![HttpResponse {
        status: 200,
        headers: vec![],
        body: b"<html></html>".to_vec(),
    }]);
    let deps = env.deps(empty_page.transport());
    let error = ollama_cloud::fetch_ollama_cloud_usage(&deps, &json!({"cookie": "c"}))
        .await
        .unwrap_err();
    assert!(error.contains("could not be parsed"));

    assert!(!ollama_cloud::is_configured(
        &env.deps(FakeHttp::new(vec![]).transport())
    ));
    write_quota_credential(
        &env.deps(FakeHttp::new(vec![]).transport()),
        "ollama-cloud",
        &json!({"cookie": "c"}),
    )
    .unwrap();
    assert!(ollama_cloud::is_configured(
        &env.deps(FakeHttp::new(vec![]).transport())
    ));
}

#[tokio::test]
async fn opencode_go_parses_usage_and_maps_auth_failures() {
    let env = TestEnv::new();
    env.set_auth(json!({ "opencode-go": { "key": "test-key" } }));

    let payload = json!({
        "usage": {
            "rolling": { "percent": 25, "resetsAt": "2026-08-12T12:00:00.000Z" },
            "weekly": { "percent": 40, "resetsAt": "2026-08-19T12:00:00.000Z" }
        }
    });
    let windows = opencode_go::parse_opencode_go_usage(Some(&payload), FIXED_NOW);
    assert_eq!(windows["5h"]["usedPercent"], json!(25));
    assert_eq!(windows["5h"]["resetAt"], json!("2026-08-12T12:00:00.000Z"));
    assert_eq!(windows["weekly"]["usedPercent"], json!(40));
    assert!(windows.get("monthly").is_none());

    let fake = FakeHttp::new(vec![status_response(403, vec![], b"")]);
    let deps = env.deps(fake.transport());
    let error = opencode_go::fetch_opencode_go_usage(&deps, "secret")
        .await
        .unwrap_err();
    assert_eq!(error, "OpenCode Go authentication failed");

    let fake = FakeHttp::new(vec![json_response(payload)]);
    let deps = env.deps(fake.transport());
    let windows = opencode_go::fetch_opencode_go_usage(&deps, "secret")
        .await
        .unwrap();
    assert_eq!(windows["5h"]["usedPercent"], json!(25));
    let requests = fake.requests.lock().unwrap();
    assert_eq!(requests[0].url, "https://opencode.ai/zen/go/v1/usage");
    let authorization = requests[0]
        .headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("authorization"))
        .map(|(_, value)| value.clone());
    assert_eq!(authorization.as_deref(), Some("Bearer secret"));
    assert!(
        !requests[0]
            .headers
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case("cookie"))
    );
}

#[tokio::test]
async fn opencode_go_fetch_deletes_the_legacy_credential_file() {
    let env = TestEnv::new();
    env.set_auth(json!({ "opencode-go": { "key": "test-key" } }));
    let legacy = env.home.join("data").join("quota").join("opencode-go.json");
    std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
    std::fs::write(&legacy, "{not valid json").unwrap();

    let (runtime, _) = quota_runtime(
        &env,
        vec![json_response(json!({
            "usage": { "rolling": { "percent": 25, "resetsAt": "2026-08-12T12:00:00.000Z" } }
        }))],
    );
    let result = opencode_go::fetch_quota(runtime).await;
    assert_eq!(result["providerId"], json!("opencode-go"));
    assert_eq!(result["ok"], json!(true));
    assert_eq!(result["configured"], json!(true));
    assert!(!legacy.exists());
}

// ============== claude ==============

fn claude_payload() -> Value {
    json!({
        "limits": [
            { "kind": "session", "percent": 5, "resets_at": "2026-08-14T19:10:00Z", "scope": null },
            { "kind": "weekly_all", "percent": 4, "resets_at": "2026-08-20T15:00:00Z", "scope": null },
            { "kind": "weekly_scoped", "percent": 12, "resets_at": "2026-08-20T15:00:00Z",
              "scope": { "model": { "id": null, "display_name": "Fable" } } }
        ],
        "spend": {
            "used": { "amount_minor": 250, "currency": "USD", "exponent": 2 },
            "limit": { "amount_minor": 10000, "currency": "USD", "exponent": 2 },
            "percent": 2.5,
            "enabled": true
        }
    })
}

#[test]
fn claude_transforms_map_limits_models_and_extra_usage() {
    let (windows, models) = claude::to_claude_usage(Some(&claude_payload()), FIXED_NOW);
    assert_eq!(windows["5h"]["usedPercent"], json!(5));
    assert_eq!(windows["5h"]["windowSeconds"], json!(18000));
    assert_eq!(windows["5h"]["resetAt"], json!(1_786_734_600_000i64));
    assert_eq!(windows["7d"]["usedPercent"], json!(4));
    assert_eq!(models["Fable"]["windows"]["7d"]["usedPercent"], json!(12));
    assert_eq!(
        models["Fable"]["windows"]["7d"]["windowSeconds"],
        json!(604800)
    );
    assert_eq!(windows["extra_usage"]["usedPercent"], json!(2.5));
    assert_eq!(
        windows["extra_usage"]["valueLabel"],
        json!("$2.50 / $100.00")
    );

    // Disabled spend block omits the window; unknown kinds are ignored.
    let mut payload = claude_payload();
    payload["spend"]["enabled"] = json!(false);
    let (windows, _) = claude::to_claude_usage(Some(&payload), FIXED_NOW);
    assert!(windows.get("extra_usage").is_none());

    let (windows, models) = claude::to_claude_usage(
        Some(
            &json!({ "limits": [{ "kind": "iguana_necktie", "percent": 90, "resets_at": null }] }),
        ),
        FIXED_NOW,
    );
    assert!(windows.is_empty());
    assert!(models.is_empty());

    // Legacy fallback when no limits array is present.
    let legacy = json!({
        "five_hour": { "utilization": 5.0, "resets_at": "2026-08-14T19:10:00.313090+00:00" },
        "seven_day": { "utilization": 4.0, "resets_at": "2026-08-20T15:00:00.313112+00:00" }
    });
    let (windows, models) = claude::to_claude_usage(Some(&legacy), FIXED_NOW);
    assert_eq!(windows["5h"]["usedPercent"], json!(5));
    assert_eq!(windows["7d"]["usedPercent"], json!(4));
    assert!(models.is_empty());

    let (windows, models) = claude::to_claude_usage(None, FIXED_NOW);
    assert!(windows.is_empty() && models.is_empty());
}

#[test]
fn claude_credential_sources_fall_through_in_priority_order() {
    let env = TestEnv::new();
    let deps = env.deps(FakeHttp::new(vec![]).transport());

    // Keychain wins over a stale credentials file.
    env.set_keychain(Some(
        serde_json::to_string(&json!({
            "mcpOAuth": { "linear|abc": { "accessToken": "unrelated-mcp-token" } },
            "claudeAiOauth": {
                "accessToken": "keychain-token",
                "refreshToken": "keychain-token-refresh",
                "expiresAt": 1_786_735_755_912i64,
                "subscriptionType": "max"
            }
        }))
        .unwrap(),
    ));
    std::fs::create_dir_all(env.home.join(".claude")).unwrap();
    std::fs::write(
        env.home.join(".claude").join(".credentials.json"),
        serde_json::to_string(&json!({
            "claudeAiOauth": { "accessToken": "file-token", "refreshToken": "file-refresh", "expiresAt": 1, "subscriptionType": "pro" }
        }))
        .unwrap(),
    )
    .unwrap();
    // The keychain source is only consulted on macOS; mirror the JS priority
    // by asserting whichever the platform yields is a real credential.
    let credential = claude::load_claude_credential(&deps)
        .unwrap()
        .expect("credential");
    if cfg!(target_os = "macos") {
        assert_eq!(credential.access_token, "keychain-token");
        assert_eq!(credential.plan_label.as_deref(), Some("max"));
    } else {
        assert_eq!(credential.access_token, "file-token");
    }

    // OpenCode auth entry before env var.
    env.set_keychain(None);
    std::fs::remove_file(env.home.join(".claude").join(".credentials.json")).unwrap();
    env.set_auth(json!({ "anthropic": { "access": "opencode-token", "refresh": "opencode-refresh", "expires": 1_786_735_755_912i64 } }));
    let credential = claude::load_claude_credential(&deps).unwrap().unwrap();
    assert_eq!(credential.access_token, "opencode-token");
    assert_eq!(credential.plan_label, None);

    env.set_auth(json!({}));
    env.set_env("CLAUDE_CODE_OAUTH_TOKEN", "env-token");
    let credential = claude::load_claude_credential(&deps).unwrap().unwrap();
    assert_eq!(credential.access_token, "env-token");
    assert_eq!(credential.refresh_token, None);
}

#[tokio::test]
async fn claude_fetch_reports_windows_plan_label_and_not_configured() {
    let env = TestEnv::new();
    let (runtime, _) = quota_runtime(&env, vec![]);
    let not_configured = claude::fetch_quota(runtime).await;
    assert_eq!(not_configured["configured"], json!(false));
    assert_eq!(not_configured["usage"], json!(null));

    env.set_auth(json!({ "claude": { "access": "access-a", "refresh": "refresh-a", "expires": 1_786_735_755_912i64 } }));
    let (runtime, fake) = quota_runtime(&env, vec![json_response(claude_payload())]);
    let result = claude::fetch_quota(runtime).await;
    assert_eq!(result["ok"], json!(true));
    assert_eq!(result["usage"]["windows"]["5h"]["usedPercent"], json!(5));
    let requests = fake.requests.lock().unwrap();
    assert_eq!(requests[0].url, "https://api.anthropic.com/api/oauth/usage");
    let beta = requests[0]
        .headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("anthropic-beta"))
        .map(|(_, value)| value.clone());
    assert_eq!(beta.as_deref(), Some("oauth-2025-04-20"));
}

#[tokio::test]
async fn claude_serves_cached_values_during_the_429_cooldown() {
    let env = TestEnv::new();
    env.set_keychain(Some(
        serde_json::to_string(&json!({ "claudeAiOauth": {
            "accessToken": "access-a", "refreshToken": "refresh-a", "subscriptionType": "max"
        }}))
        .unwrap(),
    ));
    let (runtime, fake) = quota_runtime(
        &env,
        vec![
            json_response(claude_payload()),
            status_response(429, vec![("retry-after", "120")], b"{}"),
        ],
    );
    // The keychain is only consulted on macOS; fall back to the opencode entry.
    if !cfg!(target_os = "macos") {
        env.set_keychain(None);
        env.set_auth(json!({ "claude": { "access": "access-a", "refresh": "refresh-a" } }));
    }

    let first = claude::fetch_quota(runtime.clone()).await;
    assert_eq!(first["ok"], json!(true));
    let second = claude::fetch_quota(runtime.clone()).await;
    let third = claude::fetch_quota(runtime.clone()).await;

    assert_eq!(
        second["ok"],
        json!(true),
        "cached values serve through the cooldown"
    );
    assert_eq!(second["usage"]["windows"]["5h"]["usedPercent"], json!(5));
    assert_eq!(third["ok"], json!(true));
    assert_eq!(
        fake.requests.lock().unwrap().len(),
        2,
        "cooldown short-circuits the third call"
    );
}

#[tokio::test]
async fn claude_switching_credentials_drops_the_cache() {
    let env = TestEnv::new();
    env.set_auth(json!({ "claude": { "access": "access-a", "refresh": "refresh-a" } }));
    let (runtime, _) = quota_runtime(
        &env,
        vec![
            json_response(claude_payload()),
            status_response(429, vec![], b"{}"),
        ],
    );
    let first = claude::fetch_quota(runtime.clone()).await;
    assert_eq!(first["ok"], json!(true));

    env.set_auth(json!({ "claude": { "access": "access-b", "refresh": "refresh-b" } }));
    let after_switch = claude::fetch_quota(runtime.clone()).await;
    assert_eq!(after_switch["ok"], json!(false));
    assert_eq!(after_switch["usage"], json!(null));
    assert_eq!(after_switch["error"], json!("Rate limited. Retrying soon."));
}

#[tokio::test]
async fn claude_explains_expired_sessions_and_api_errors() {
    let env = TestEnv::new();
    env.set_auth(json!({ "claude": { "access": "access-a" } }));

    let (runtime, _) = quota_runtime(&env, vec![status_response(401, vec![], b"{}")]);
    let expired = claude::fetch_quota(runtime).await;
    assert_eq!(expired["configured"], json!(true));
    assert_eq!(
        expired["error"],
        json!("Claude session expired. Open Claude Code to sign in again.")
    );

    let (runtime, _) = quota_runtime(&env, vec![status_response(500, vec![], b"{}")]);
    let api_error = claude::fetch_quota(runtime).await;
    assert_eq!(api_error["error"], json!("API error: 500"));
}

// ============== google ==============

#[test]
fn google_transforms_parse_refresh_tokens_and_model_windows() {
    let (token, project, managed) =
        google::parse_google_refresh_token(Some(&json!("rt|proj-1|proj-2")));
    assert_eq!(token.as_deref(), Some("rt"));
    assert_eq!(project.as_deref(), Some("proj-1"));
    assert_eq!(managed.as_deref(), Some("proj-2"));
    let (token, project, managed) = google::parse_google_refresh_token(Some(&json!("")));
    assert!(token.is_none() && project.is_none() && managed.is_none());

    let bucket = json!({
        "modelId": "gemini-2.5-pro",
        "remainingFraction": 0.75,
        "resetTime": 1_786_723_400
    });
    let (name, entry) = google::transform_quota_bucket(&bucket, "gemini", FIXED_NOW).unwrap();
    assert_eq!(name, "gemini/gemini-2.5-pro");
    let window = &entry["windows"]["daily"];
    assert_eq!(window["usedPercent"], json!(25));
    assert_eq!(window["windowSeconds"], json!(86400));

    let model_data =
        json!({ "quotaInfo": { "remainingFraction": 0.5, "resetTime": "2026-08-20T15:00:00Z" } });
    let (name, entry) =
        google::transform_model_data("gemini-2.5-flash", &model_data, "antigravity", FIXED_NOW);
    assert_eq!(name, "antigravity/gemini-2.5-flash");
    let window = &entry["windows"]["daily"];
    assert_eq!(window["usedPercent"], json!(50));
}

#[tokio::test]
async fn google_fetch_merges_models_from_all_sources() {
    let env = TestEnv::new();
    env.set_auth(json!({ "google": { "oauth": { "access": "gemini-access", "expires": 1_786_735_755_912i64 } } }));
    std::fs::create_dir_all(env.home.join(".local").join("share").join("opencode")).unwrap();
    std::fs::write(
        env.home.join(".local").join("share").join("opencode").join("antigravity-accounts.json"),
        serde_json::to_string(&json!({
            "accounts": [{ "refreshToken": "antigravity-refresh", "projectId": "proj-9", "email": "a@b.c" }],
            "activeIndex": 0
        }))
        .unwrap(),
    )
    .unwrap();

    // Source order is gemini first, antigravity second: gemini quota
    // buckets → gemini models → antigravity refresh → antigravity models.
    let (runtime, fake) = quota_runtime(
        &env,
        vec![
            json_response(json!({ "buckets": [] })),
            json_response(json!({
                "models": { "gemini-model": { "quotaInfo": { "remainingFraction": 0.2 } } }
            })),
            json_response(json!({ "access_token": "antigravity-access" })),
            json_response(json!({
                "models": { "antigravity-model": { "quotaInfo": { "remainingFraction": 0.9 } } }
            })),
        ],
    );
    let result = google::fetch_quota(runtime).await;

    let urls = fake.urls.lock().unwrap().clone();
    assert_eq!(
        urls[0],
        "https://cloudcode-pa.googleapis.com/v1internal:retrieveUserQuota"
    );
    assert!(urls[1].starts_with(
        "https://daily-cloudcode-pa.sandbox.googleapis.com/v1internal:fetchAvailableModels"
    ));
    assert_eq!(urls[2], "https://oauth2.googleapis.com/token");
    assert!(urls[3].starts_with(
        "https://daily-cloudcode-pa.sandbox.googleapis.com/v1internal:fetchAvailableModels"
    ));

    assert_eq!(result["ok"], json!(true));
    let models = result["usage"]["models"].as_object().unwrap();
    assert_eq!(models.len(), 2);
    assert_eq!(
        models["antigravity/antigravity-model"]["windows"]["5h"]["usedPercent"],
        json!(10)
    );
    assert_eq!(
        models["gemini/gemini-model"]["windows"]["daily"]["usedPercent"],
        json!(80)
    );
    assert_eq!(result["usage"]["windows"].as_object().unwrap().len(), 0);
}

#[tokio::test]
async fn google_reports_source_errors_when_no_models_merge() {
    let env = TestEnv::new();
    env.set_auth(json!({ "google": { "oauth": { "refresh": "refresh|proj-1" } } }));

    let (runtime, _) = quota_runtime(&env, vec![status_response(400, vec![], b"{}")]);
    let result = google::fetch_quota(runtime).await;
    assert_eq!(result["ok"], json!(false));
    assert_eq!(result["configured"], json!(true));
    assert_eq!(
        result["error"],
        json!("Gemini: Failed to refresh OAuth token")
    );
}

// ============== cursor ==============

fn cursor_usage_payload() -> Value {
    json!({
        "enabled": true,
        "planUsage": {
            "totalSpend": 1250,
            "totalPercentUsed": 25,
            "limit": 5000,
            "remaining": 3750,
            "autoPercentUsed": 10,
            "apiPercentUsed": 15
        },
        "spendLimitUsage": {
            "individualLimit": 2000,
            "individualRemaining": 500
        },
        "billingCycleEnd": 1_786_800_000_000i64
    })
}

#[tokio::test]
async fn cursor_builds_windows_from_dashboard_payload() {
    let plan =
        json!({ "planInfo": { "planName": "Pro", "billingCycleEnd": 1_786_800_000_000i64 } });
    let windows = cursor::build_windows(&cursor_usage_payload(), Some(&plan), FIXED_NOW);
    assert_eq!(windows["billing_cycle"]["usedPercent"], json!(25));
    assert_eq!(windows["billing_cycle"]["valueLabel"], json!("$12.50"));
    assert_eq!(windows["auto"]["usedPercent"], json!(10));
    assert_eq!(windows["api"]["usedPercent"], json!(15));
    assert_eq!(
        windows["plan_limit"]["valueLabel"],
        json!("$37.50 remaining of $50.00")
    );
    assert_eq!(
        windows["on_demand"]["valueLabel"],
        json!("$5.00 remaining of $20.00")
    );
    assert_eq!(windows["on_demand"]["usedPercent"], json!(75));
}

#[tokio::test]
async fn cursor_fetch_uses_bearer_token_and_rejects_missing_subscription() {
    let env = TestEnv::new();
    // A JWT far from expiry keeps the access token as-is.
    let claims = base64_url(json!({ "exp": (FIXED_NOW + 3_600_000) / 1000 }));
    let token = format!("header.{claims}.signature");
    env.set_env("CURSOR_TOKEN", &token);

    let (runtime, fake) = quota_runtime(
        &env,
        vec![
            json_response(cursor_usage_payload()),
            json_response(json!({ "planInfo": { "planName": "Pro" } })),
            json_response(json!({ "balanceCents": 990 })),
        ],
    );
    let result = cursor::fetch_quota(runtime).await;

    let urls = fake.urls.lock().unwrap().clone();
    assert_eq!(
        urls[0],
        "https://api2.cursor.sh/aiserver.v1.DashboardService/GetCurrentPeriodUsage"
    );
    assert_eq!(bearer_used(&fake, 0), format!("Bearer {token}"));

    assert_eq!(result["ok"], json!(true));
    assert_eq!(result["providerName"], json!("Cursor Pro"));
    assert_eq!(
        result["usage"]["windows"]["credits"]["valueLabel"],
        json!("$9.90")
    );

    // Missing planUsage → "No active Cursor subscription".
    let (runtime, _) = quota_runtime(&env, vec![json_response(json!({ "enabled": false }))]);
    let result = cursor::fetch_quota(runtime).await;
    assert_eq!(result["error"], json!("No active Cursor subscription"));
}

fn base64_url(value: Value) -> String {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(serde_json::to_vec(&value).unwrap())
}

#[tokio::test]
async fn cursor_import_reads_state_db_and_stores_the_credential() {
    let env = TestEnv::new();
    let state_db = env
        .home
        .join("Library")
        .join("Application Support")
        .join("Cursor")
        .join("User")
        .join("globalStorage")
        .join("state.vscdb");
    std::fs::create_dir_all(state_db.parent().unwrap()).unwrap();
    std::fs::write(&state_db, b"").unwrap();
    let db_prefix = format!("{}\0", state_db.to_string_lossy());
    env.sqlite.lock().unwrap().insert(
        format!("{db_prefix}cursorAuth/accessToken"),
        "stale".to_string(),
    );
    env.sqlite.lock().unwrap().insert(
        format!("{db_prefix}cursorAuth/refreshToken"),
        "refresh-token".to_string(),
    );

    // The stale access token needs a refresh; the oauth endpoint mints a new
    // one which the import persists.
    let (runtime, fake) = quota_runtime(
        &env,
        vec![json_response(json!({ "access_token": "resolved-token" }))],
    );
    let status = cursor::import_cursor_credential(&runtime.deps)
        .await
        .unwrap();
    assert_eq!(status["configured"], json!(true));
    assert_eq!(
        fake.urls.lock().unwrap()[0],
        "https://api2.cursor.sh/oauth/token"
    );
    let deps = runtime.deps.clone();
    let stored = crate::quota::credentials::read_managed_credential(&deps, "cursor").unwrap();
    assert_eq!(stored["accessToken"], json!("resolved-token"));
    assert_eq!(stored["refreshToken"], json!("refresh-token"));

    // Without any sqlite rows the import reports unavailable credentials.
    env.sqlite.lock().unwrap().clear();
    let deps = env.deps(FakeHttp::new(vec![]).transport());
    let error = cursor::import_cursor_credential(&deps).await.unwrap_err();
    assert_eq!(error, "Cursor credentials are unavailable");
}

// ============== xai ==============

fn varint(mut value: u64) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        let mut byte = (value & 0x7f) as u8;
        value >>= 7;
        if value != 0 {
            byte |= 0x80;
        }
        out.push(byte);
        if value == 0 {
            return out;
        }
    }
}

fn grpc_web_frame(payload: &[u8]) -> Vec<u8> {
    let mut frame = vec![0u8];
    frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    frame.extend_from_slice(payload);
    frame
}

#[test]
fn xai_parse_usage_reads_percent_and_reset_from_protobuf() {
    // field 1, wire 5 (fixed32) = 42.0 → path [1].
    let mut payload = vec![0x0D];
    payload.extend_from_slice(&42.0f32.to_le_bytes());
    // field 1, wire 2 (LEN) → nested field 5 wire 2 → field 1 varint 1_800_000_000.
    let inner_field = [0x08u8]
        .iter()
        .copied()
        .chain(varint(1_800_000_000))
        .collect::<Vec<_>>();
    let mut middle = vec![0x2A];
    middle.extend_from_slice(&varint(inner_field.len() as u64));
    middle.extend_from_slice(&inner_field);
    payload.extend_from_slice(&middle);

    let usage = xai::parse_usage(&grpc_web_frame(&payload), FIXED_NOW).unwrap();
    assert_eq!(usage.used_percent, Some(42.0));
    assert_eq!(usage.reset_at, Some(1_800_000_000_000));

    // Malformed framing (declared length exceeds the buffer) and missing
    // usage surface the JS error strings.
    let bad_frame = vec![0x00, 0, 0, 0, 5, 0x0A];
    let error = xai::parse_usage(&bad_frame, FIXED_NOW).unwrap_err();
    assert_eq!(error, "xAI billing returned malformed gRPC-web framing");

    // A non-zero flag byte means "not framed at all" — the payload is then
    // rejected as non-protobuf (empty), matching the JS fallback order.
    let unframed = vec![0x01, 0, 0, 0, 1, 0xFF];
    let error = xai::parse_usage(&unframed, FIXED_NOW).unwrap_err();
    assert_eq!(error, "xAI billing returned an empty protobuf response");

    // Trailer with a non-zero status fails the RPC.
    let mut with_trailer = grpc_web_frame(&payload);
    let trailer = b"grpc-status: 7";
    with_trailer.push(0x80);
    with_trailer.extend_from_slice(&(trailer.len() as u32).to_be_bytes());
    with_trailer.extend_from_slice(trailer);
    let error = xai::parse_usage(&with_trailer, FIXED_NOW).unwrap_err();
    assert_eq!(error, "xAI billing RPC failed with status 7");

    // Empty response.
    let error = xai::parse_usage(&[], FIXED_NOW).unwrap_err();
    assert_eq!(error, "xAI billing returned an empty protobuf response");
}

#[tokio::test]
async fn xai_fetch_reports_zero_when_only_a_reset_and_period_exist() {
    // varint at [1,6] (usage period) + reset varint at [1,5,1].
    let inner_field = [0x08u8]
        .iter()
        .copied()
        .chain(varint(1_800_000_000))
        .collect::<Vec<_>>();
    let mut middle = vec![0x2A];
    middle.extend_from_slice(&varint(inner_field.len() as u64));
    middle.extend_from_slice(&inner_field);
    let mut payload = Vec::new();
    // field 6 varint inside field 1 (LEN): path [1,6].
    let nested_field = [0x30u8, 0x01].to_vec();
    payload.push(0x0A);
    payload.extend_from_slice(&varint(nested_field.len() as u64));
    payload.extend_from_slice(&nested_field);
    payload.extend_from_slice(&middle);

    let env = TestEnv::new();
    env.set_auth(json!({ "xai": { "type": "oauth", "access": "fresh-access", "refresh": "r", "expires": FIXED_NOW + 3_600_000 } }));
    let (runtime, _) = quota_runtime(
        &env,
        vec![HttpResponse {
            status: 200,
            headers: vec![],
            body: grpc_web_frame(&payload),
        }],
    );
    let result = xai::fetch_quota(runtime).await;
    assert_eq!(result["ok"], json!(true));
    assert_eq!(
        result["usage"]["windows"]["billing_cycle"]["usedPercent"],
        json!(0)
    );
    assert_eq!(
        result["usage"]["windows"]["billing_cycle"]["resetAt"],
        json!(1_800_000_000_000i64)
    );
}

#[tokio::test]
async fn xai_refreshes_expired_tokens_and_persists_them() {
    let env = TestEnv::new();
    env.set_auth(json!({ "xai": { "type": "oauth", "access": "stale", "refresh": "refresh-token", "expires": FIXED_NOW } }));

    let usage_payload = {
        let mut payload = vec![0x0D];
        payload.extend_from_slice(&10.0f32.to_le_bytes());
        payload
    };
    let (runtime, _) = quota_runtime(
        &env,
        vec![
            json_response(
                json!({ "access_token": "new-access", "refresh_token": "new-refresh", "expires_in": 3600 }),
            ),
            HttpResponse {
                status: 200,
                headers: vec![],
                body: grpc_web_frame(&usage_payload),
            },
        ],
    );
    let result = xai::fetch_quota(runtime).await;
    assert_eq!(result["ok"], json!(true));
    assert_eq!(
        result["usage"]["windows"]["billing_cycle"]["usedPercent"],
        json!(10)
    );

    let written = env.written_auth.lock().unwrap();
    assert_eq!(written.len(), 1);
    assert_eq!(written[0]["xai"]["access"], json!("new-access"));
    assert_eq!(written[0]["xai"]["refresh"], json!("new-refresh"));
    assert_eq!(written[0]["xai"]["type"], json!("oauth"));
    drop(written);
}

#[tokio::test]
async fn xai_reports_read_errors_and_missing_configuration() {
    let env = TestEnv::new();
    let (runtime, _) = quota_runtime(&env, vec![]);
    let not_configured = xai::fetch_quota(runtime).await;
    assert_eq!(not_configured["configured"], json!(false));
    assert_eq!(not_configured["error"], json!("Not configured"));

    env.set_auth(json!({ "xai": { "type": "api" } }));
    let (runtime, _) = quota_runtime(&env, vec![]);
    let wrong_type = xai::fetch_quota(runtime).await;
    assert_eq!(wrong_type["configured"], json!(false));
}

// ============== runtime dispatcher ==============

#[tokio::test]
async fn runtime_answers_unsupported_providers_with_the_envelope() {
    let env = TestEnv::new();
    let runtime = Arc::new(QuotaRuntime::new(
        env.deps(FakeHttp::new(vec![]).transport()),
    ));
    let result = runtime
        .fetch_quota_for_provider("unsupported-test-provider")
        .await;
    assert_eq!(result["providerId"], json!("unsupported-test-provider"));
    assert_eq!(result["providerName"], json!("unsupported-test-provider"));
    assert_eq!(result["ok"], json!(false));
    assert_eq!(result["configured"], json!(false));
    assert_eq!(result["error"], json!("Unsupported provider"));
    // The slot clears after completion so a later call refreshes again.
    let _ = runtime
        .fetch_quota_for_provider("unsupported-test-provider")
        .await;
}

#[tokio::test]
async fn runtime_coalesces_concurrent_refreshes_by_provider_id() {
    let env = TestEnv::new();
    env.set_auth(json!({ "openrouter": { "key": "k" } }));

    // Hold the response open long enough for both callers to subscribe.
    let seen = Arc::new(std::sync::Mutex::new(0));
    let seen2 = seen.clone();
    let http: crate::quota::http::HttpFetch = Arc::new(move |_request: HttpRequest| {
        let seen = seen2.clone();
        Box::pin(async move {
            *seen.lock().unwrap() += 1;
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            Ok(json_response(
                json!({ "data": { "total_credits": 10.0, "total_usage": 4.0 } }),
            ))
        })
    });
    let runtime = Arc::new(QuotaRuntime::new(env.deps(http)));

    let first = runtime.fetch_quota_for_provider("openrouter");
    let second = runtime.fetch_quota_for_provider("openrouter");
    let (first, second) = tokio::join!(first, second);

    assert_eq!(first["ok"], json!(true));
    assert_eq!(second["ok"], json!(true));
    assert_eq!(
        *seen.lock().unwrap(),
        1,
        "concurrent refreshes share one HTTP call"
    );
}

#[tokio::test]
async fn runtime_lists_configured_providers_in_registry_order() {
    let env = TestEnv::new();
    env.set_auth(json!({
        "openai": { "access": "a" },
        "crof": { "key": "k" },
        "openrouter": { "key": "k" }
    }));
    let runtime = Arc::new(QuotaRuntime::new(
        env.deps(FakeHttp::new(vec![]).transport()),
    ));
    let configured = runtime.list_configured();
    assert_eq!(configured, vec!["codex", "crof", "openrouter"]);
}

// ============== routes ==============

fn quota_app(env: &TestEnv, responses: Vec<HttpResponse>) -> axum::Router {
    let fake = FakeHttp::new(responses);
    routes::router_with(Arc::new(QuotaRuntime::new(env.deps(fake.transport()))))
}

async fn get_json(app: axum::Router, uri: &str) -> (StatusCode, Value) {
    let response = app
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
}

#[tokio::test]
async fn routes_list_providers_and_answer_unknown_quota_ids() {
    let env = TestEnv::new();
    env.set_auth(json!({ "openrouter": { "key": "k" } }));

    let (status, body) = get_json(quota_app(&env, vec![]), "/api/quota/providers").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({ "providers": ["openrouter"] }));

    let (status, body) = get_json(quota_app(&env, vec![]), "/api/quota/nope").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["ok"], json!(false));
    assert_eq!(body["error"], json!("Unsupported provider"));
}

#[tokio::test]
async fn routes_credential_status_and_delete_shapes() {
    let env = TestEnv::new();

    let (status, body) = get_json(quota_app(&env, vec![]), "/api/quota/credentials/unknown").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(
        body,
        json!({ "code": "UNSUPPORTED_PROVIDER", "error": "Unsupported credential provider" })
    );

    let (status, body) = get_json(quota_app(&env, vec![]), "/api/quota/credentials/cursor").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({ "configured": false }));

    let deps = env.deps(FakeHttp::new(vec![]).transport());
    write_managed_credential(&deps, "ollama-cloud", &json!({ "cookie": "session=1" })).unwrap();
    let (status, body) = get_json(
        quota_app(&env, vec![]),
        "/api/quota/credentials/ollama-cloud",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body,
        json!({ "configured": true, "secretMasked": "\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}" })
    );

    let response = quota_app(&env, vec![])
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/api/quota/credentials/ollama-cloud")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn routes_put_validates_and_stores_ollama_credentials() {
    let env = TestEnv::new();
    let html = HttpResponse {
        status: 200,
        headers: vec![("content-type".to_string(), "text/html".to_string())],
        body: b"<html>Session usage 30%</html>".to_vec(),
    };

    let app = quota_app(&env, vec![html.clone()]);
    let response = app
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/quota/credentials/ollama-cloud")
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{ "cookie": "session=abc" }"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = serde_json::from_slice::<Value>(
        &axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        body,
        json!({ "configured": true, "secretMasked": "\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}" })
    );

    // An invalid cookie is rejected before the validator runs.
    let app = quota_app(&env, vec![html]);
    let response = app
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/quota/credentials/ollama-cloud")
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{ "cookie": "" }"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = serde_json::from_slice::<Value>(
        &axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(body["code"], json!("INVALID_CREDENTIAL"));
    assert_eq!(body["error"], json!("Invalid credential"));

    // A failing validator surfaces its message.
    let app = quota_app(
        &env,
        vec![status_response(302, vec![("location", "/login")], b"")],
    );
    let response = app
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/quota/credentials/ollama-cloud")
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{ "cookie": "session=abc" }"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn routes_validate_reports_missing_credentials_and_import_unavailable() {
    let env = TestEnv::new();

    let response = quota_app(&env, vec![])
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/quota/credentials/ollama-cloud/validate")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let body = serde_json::from_slice::<Value>(
        &axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        body,
        json!({ "code": "NOT_CONFIGURED", "error": "Not configured" })
    );

    let response = quota_app(&env, vec![])
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/quota/credentials/nope/import")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let body = serde_json::from_slice::<Value>(
        &axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(body["code"], json!("UNSUPPORTED_PROVIDER"));

    // A GET probe on the POST-only import route falls through like Express.
    let response = quota_app(&env, vec![])
        .oneshot(
            Request::builder()
                .uri("/api/quota/credentials/cursor/import")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    let response = quota_app(&env, vec![])
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/quota/credentials/ollama-cloud/import")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let body = serde_json::from_slice::<Value>(
        &axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        body,
        json!({ "code": "IMPORT_UNAVAILABLE", "error": "Import unavailable" })
    );
}
